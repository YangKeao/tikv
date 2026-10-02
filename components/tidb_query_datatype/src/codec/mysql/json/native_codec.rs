// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! The native SDK's lossless node encoder, distinct from wire JSON builders and
//! serde-value conversion. Scalar bytes are representation, not validated
//! input.

use super::native_policy::NativeJsonNode;

const HEADER_SIZE: usize = 8;
const KEY_ENTRY_SIZE: usize = 6;
const VALUE_ENTRY_SIZE: usize = 5;
const MAX_JSON_DEPTH: usize = 100;
const JSON_TYPE_CODE_OBJECT: u8 = 0x01;
const JSON_TYPE_CODE_ARRAY: u8 = 0x03;
const JSON_TYPE_CODE_LITERAL: u8 = 0x04;

/// The original native node encoder's three failure classes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeBinaryJsonEncodeError {
    InvalidBinary,
    TooDeep,
    KeyTooLong,
}

/// Encodes a lossless node with the native SDK's original byte/error policy.
/// `scalar_parts` only borrows the scalar's stored type code and payload. It is
/// a noncapturing representation projection, never an evaluator or policy hook,
/// and is called only for a scalar after the depth check has passed.
pub fn encode_native_binary_json_node<T>(
    node: &NativeJsonNode<T>,
    scalar_parts: fn(&T) -> (u8, &[u8]),
) -> Result<(u8, Vec<u8>), NativeBinaryJsonEncodeError> {
    encode_node(node, 0, scalar_parts)
}

fn encode_node<T>(
    node: &NativeJsonNode<T>,
    depth: usize,
    scalar_parts: fn(&T) -> (u8, &[u8]),
) -> Result<(u8, Vec<u8>), NativeBinaryJsonEncodeError> {
    if depth > MAX_JSON_DEPTH {
        return Err(NativeBinaryJsonEncodeError::TooDeep);
    }
    match node {
        NativeJsonNode::Scalar(value) => {
            let (type_code, value) = scalar_parts(value);
            Ok((type_code, value.to_vec()))
        }
        NativeJsonNode::Array(values) => {
            let values = values
                .iter()
                .map(|value| encode_node(value, depth + 1, scalar_parts))
                .collect::<Result<Vec<_>, _>>()?;
            encode_binary_array(&values)
        }
        NativeJsonNode::Object(values) => {
            let values = values
                .iter()
                .map(|(key, value)| {
                    Ok((key.as_str(), encode_node(value, depth + 1, scalar_parts)?))
                })
                .collect::<Result<Vec<_>, NativeBinaryJsonEncodeError>>()?;
            encode_binary_object(&values)
        }
    }
}

fn encode_binary_array(
    values: &[(u8, Vec<u8>)],
) -> Result<(u8, Vec<u8>), NativeBinaryJsonEncodeError> {
    let data_start = HEADER_SIZE + values.len() * VALUE_ENTRY_SIZE;
    let mut output = vec![0; data_start];
    let mut payload = Vec::new();
    for (index, value) in values.iter().enumerate() {
        let entry = HEADER_SIZE + index * VALUE_ENTRY_SIZE;
        output[entry] = value.0;
        if value.0 == JSON_TYPE_CODE_LITERAL {
            output[entry + 1] = *value
                .1
                .first()
                .ok_or(NativeBinaryJsonEncodeError::InvalidBinary)?;
        } else {
            let offset = u32::try_from(data_start + payload.len())
                .map_err(|_| NativeBinaryJsonEncodeError::InvalidBinary)?;
            output[entry + 1..entry + 5].copy_from_slice(&offset.to_le_bytes());
            payload.extend_from_slice(&value.1);
        }
    }
    output.extend_from_slice(&payload);
    write_native_binary_json_header(&mut output, values.len())?;
    Ok((JSON_TYPE_CODE_ARRAY, output))
}

fn encode_binary_object(
    values: &[(&str, (u8, Vec<u8>))],
) -> Result<(u8, Vec<u8>), NativeBinaryJsonEncodeError> {
    let mut values = values.to_vec();
    values.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let key_entry_start = HEADER_SIZE;
    let value_entry_start = key_entry_start + values.len() * KEY_ENTRY_SIZE;
    let key_data_start = value_entry_start + values.len() * VALUE_ENTRY_SIZE;
    let key_bytes = values.iter().try_fold(0_usize, |total, (key, _)| {
        if key.len() > u16::MAX as usize {
            Err(NativeBinaryJsonEncodeError::KeyTooLong)
        } else {
            total
                .checked_add(key.len())
                .ok_or(NativeBinaryJsonEncodeError::InvalidBinary)
        }
    })?;
    let value_data_start = key_data_start + key_bytes;
    let mut output = vec![0; key_data_start];
    let mut keys = Vec::with_capacity(key_bytes);
    let mut payload = Vec::new();
    for (index, (key, value)) in values.iter().enumerate() {
        let key_entry = key_entry_start + index * KEY_ENTRY_SIZE;
        let key_offset = u32::try_from(key_data_start + keys.len())
            .map_err(|_| NativeBinaryJsonEncodeError::InvalidBinary)?;
        output[key_entry..key_entry + 4].copy_from_slice(&key_offset.to_le_bytes());
        output[key_entry + 4..key_entry + 6].copy_from_slice(&(key.len() as u16).to_le_bytes());
        keys.extend_from_slice(key.as_bytes());

        let value_entry = value_entry_start + index * VALUE_ENTRY_SIZE;
        output[value_entry] = value.0;
        if value.0 == JSON_TYPE_CODE_LITERAL {
            output[value_entry + 1] = *value
                .1
                .first()
                .ok_or(NativeBinaryJsonEncodeError::InvalidBinary)?;
        } else {
            let offset = u32::try_from(value_data_start + payload.len())
                .map_err(|_| NativeBinaryJsonEncodeError::InvalidBinary)?;
            output[value_entry + 1..value_entry + 5].copy_from_slice(&offset.to_le_bytes());
            payload.extend_from_slice(&value.1);
        }
    }
    output.extend_from_slice(&keys);
    output.extend_from_slice(&payload);
    write_native_binary_json_header(&mut output, values.len())?;
    Ok((JSON_TYPE_CODE_OBJECT, output))
}

/// Writes a checked native header into an existing container buffer. As in the
/// source codec, the caller supplies at least eight bytes. Count and total size
/// conversions both precede writes. The original serde encoders also use this
/// primitive, without changing their distinct conversion/error ordering.
pub fn write_native_binary_json_header(
    output: &mut [u8],
    count: usize,
) -> Result<(), NativeBinaryJsonEncodeError> {
    let count = u32::try_from(count).map_err(|_| NativeBinaryJsonEncodeError::InvalidBinary)?;
    let size =
        u32::try_from(output.len()).map_err(|_| NativeBinaryJsonEncodeError::InvalidBinary)?;
    output[..4].copy_from_slice(&count.to_le_bytes());
    output[4..8].copy_from_slice(&size.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use NativeJsonNode::{Array, Object, Scalar};

    use super::*;

    fn parts(value: &(u8, Vec<u8>)) -> (u8, &[u8]) {
        (value.0, &value.1)
    }

    #[test]
    fn raw_codec_keeps_fixed_headers_duplicate_keys_and_unvalidated_scalar_bytes() {
        let array = Array(vec![
            Scalar((0x04, vec![1, 99])),
            Scalar((0x0d, vec![15, 2, 0xff, 0])),
            Scalar((0x0e, vec![1, 2, 3, 4, 5, 6, 7, 8])),
        ]);
        assert_eq!(
            encode_native_binary_json_node(&array, parts).unwrap(),
            (
                0x03,
                vec![
                    3, 0, 0, 0, 35, 0, 0, 0, 0x04, 1, 0, 0, 0, 0x0d, 23, 0, 0, 0, 0x0e, 27, 0, 0,
                    0, 15, 2, 0xff, 0, 1, 2, 3, 4, 5, 6, 7, 8,
                ]
            )
        );
        let object = Object(vec![
            ("b".to_owned(), Scalar((0x04, vec![2]))),
            ("a".to_owned(), Scalar((0x04, vec![1]))),
            ("a".to_owned(), Scalar((0x04, vec![0]))),
        ]);
        assert_eq!(
            encode_native_binary_json_node(&object, parts).unwrap(),
            (
                0x01,
                vec![
                    3, 0, 0, 0, 44, 0, 0, 0, 41, 0, 0, 0, 1, 0, 42, 0, 0, 0, 1, 0, 43, 0, 0, 0, 1,
                    0, 0x04, 1, 0, 0, 0, 0x04, 0, 0, 0, 0, 0x04, 2, 0, 0, 0, b'a', b'a', b'b',
                ]
            )
        );
        for tag in [0x03, 0x04, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0xfe] {
            let scalar = (tag, vec![0x80, 0]);
            assert_eq!(
                encode_native_binary_json_node(&Scalar(scalar.clone()), parts).unwrap(),
                scalar
            );
        }
        assert_eq!(
            encode_native_binary_json_node(&Scalar((0x04, vec![])), parts).unwrap(),
            (0x04, vec![])
        );
        assert_eq!(
            encode_native_binary_json_node(&Array(vec![Scalar((0x04, vec![]))]), parts),
            Err(NativeBinaryJsonEncodeError::InvalidBinary)
        );
    }

    #[test]
    fn raw_codec_preserves_depth_child_key_and_header_error_order() {
        let mut deep = Scalar((0xfe, vec![]));
        for _ in 0..100 {
            deep = Array(vec![deep]);
        }
        assert!(encode_native_binary_json_node(&deep, parts).is_ok());
        deep = Array(vec![deep]);
        fn forbidden_parts(_: &(u8, Vec<u8>)) -> (u8, &[u8]) {
            panic!("scalar projection must not run beyond the depth limit")
        }
        assert_eq!(
            encode_native_binary_json_node(&deep, forbidden_parts),
            Err(NativeBinaryJsonEncodeError::TooDeep)
        );
        let long_key = "x".repeat(usize::from(u16::MAX) + 1);
        let long_empty_literal = Object(vec![(long_key.clone(), Scalar((0x04, vec![])))]);
        assert_eq!(
            encode_native_binary_json_node(&long_empty_literal, parts),
            Err(NativeBinaryJsonEncodeError::KeyTooLong)
        );
        // Child failures are visited in input order, before sorting or checking
        // the parent key lengths, and before inlining another parent's literal.
        let children_first = Object(vec![
            (long_key, Scalar((0x04, vec![]))),
            ("z".to_owned(), deep.clone()),
        ]);
        assert_eq!(
            encode_native_binary_json_node(&children_first, parts),
            Err(NativeBinaryJsonEncodeError::TooDeep)
        );
        let input_order = Object(vec![
            ("z".to_owned(), deep),
            ("a".to_owned(), Array(vec![Scalar((0x04, vec![]))])),
        ]);
        assert_eq!(
            encode_native_binary_json_node(&input_order, parts),
            Err(NativeBinaryJsonEncodeError::TooDeep)
        );
        let mut header = [0xff; 8];
        if let Ok(count) = usize::try_from(u64::from(u32::MAX) + 1) {
            assert_eq!(
                write_native_binary_json_header(&mut header, count),
                Err(NativeBinaryJsonEncodeError::InvalidBinary)
            );
            assert_eq!(header, [0xff; 8]);
        }
        write_native_binary_json_header(&mut header, 1).unwrap();
        assert_eq!(header, [1, 0, 0, 0, 8, 0, 0, 0]);
    }
}
