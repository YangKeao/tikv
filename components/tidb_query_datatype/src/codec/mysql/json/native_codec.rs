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

// These policies are private writer details, never tags carried by a value.
#[derive(Clone, Copy)]
enum LiteralPolicy {
    Index,
    Checked,
}
#[derive(Clone, Copy)]
enum OffsetPolicy {
    Plain,
    Checked,
}

// One owner for value-entry layout, payload appending and final header writes.
struct NativeJsonContainerLayout {
    type_code: u8,
    count: usize,
    value_entry_start: usize,
    value_data_start: usize,
    output: Vec<u8>,
    payload: Vec<u8>,
}

impl NativeJsonContainerLayout {
    fn push(
        &mut self,
        index: usize,
        type_code: u8,
        value: &[u8],
        literal_policy: LiteralPolicy,
        offset_policy: OffsetPolicy,
    ) -> Result<(), NativeBinaryJsonEncodeError> {
        let entry = self.value_entry_start + index * VALUE_ENTRY_SIZE;
        self.output[entry] = type_code;
        if type_code == JSON_TYPE_CODE_LITERAL {
            self.output[entry + 1] = match literal_policy {
                LiteralPolicy::Index => value[0],
                LiteralPolicy::Checked => *value
                    .first()
                    .ok_or(NativeBinaryJsonEncodeError::InvalidBinary)?,
            };
        } else {
            let offset = match offset_policy {
                OffsetPolicy::Plain => u32::try_from(self.value_data_start + self.payload.len())
                    .map_err(|_| NativeBinaryJsonEncodeError::InvalidBinary)?,
                OffsetPolicy::Checked => self
                    .value_data_start
                    .checked_add(self.payload.len())
                    .and_then(|offset| u32::try_from(offset).ok())
                    .ok_or(NativeBinaryJsonEncodeError::InvalidBinary)?,
            };
            self.output[entry + 1..entry + 5].copy_from_slice(&offset.to_le_bytes());
            self.payload.extend_from_slice(value);
        }
        Ok(())
    }

    fn finish(mut self, keys: &[u8]) -> Result<(u8, Vec<u8>), NativeBinaryJsonEncodeError> {
        self.output.extend_from_slice(keys);
        self.output.extend_from_slice(&self.payload);
        write_native_binary_json_header(&mut self.output, self.count)?;
        Ok((self.type_code, self.output))
    }
}

/// Staged native array layout. Callers retain their own child-conversion order.
pub(crate) struct NativeJsonArrayWriter {
    layout: NativeJsonContainerLayout,
}

impl NativeJsonArrayWriter {
    pub(crate) fn new(count: usize) -> Self {
        let data_start = HEADER_SIZE + count * VALUE_ENTRY_SIZE;
        let output = vec![0; data_start];
        let payload = Vec::new();
        Self {
            layout: NativeJsonContainerLayout {
                type_code: JSON_TYPE_CODE_ARRAY,
                count,
                value_entry_start: HEADER_SIZE,
                value_data_start: data_start,
                output,
                payload,
            },
        }
    }

    pub(crate) fn push_serde(
        &mut self,
        index: usize,
        type_code: u8,
        value: &[u8],
    ) -> Result<(), NativeBinaryJsonEncodeError> {
        self.layout.push(
            index,
            type_code,
            value,
            LiteralPolicy::Index,
            OffsetPolicy::Checked,
        )
    }

    pub(crate) fn push_node(
        &mut self,
        index: usize,
        type_code: u8,
        value: &[u8],
    ) -> Result<(), NativeBinaryJsonEncodeError> {
        self.layout.push(
            index,
            type_code,
            value,
            LiteralPolicy::Checked,
            OffsetPolicy::Plain,
        )
    }

    pub(crate) fn finish(self) -> Result<(u8, Vec<u8>), NativeBinaryJsonEncodeError> {
        self.layout.finish(&[])
    }
}

/// Staged native object layout. The caller supplies sorted keys and determines
/// whether child conversion happens before construction or after each key
/// write.
pub(crate) struct NativeJsonObjectWriter {
    layout: NativeJsonContainerLayout,
    key_data_start: usize,
    keys: Vec<u8>,
}

impl NativeJsonObjectWriter {
    pub(crate) fn new<'a>(
        mut keys: impl ExactSizeIterator<Item = &'a str>,
    ) -> Result<Self, NativeBinaryJsonEncodeError> {
        let count = keys.len();
        let value_entry_start = HEADER_SIZE + count * KEY_ENTRY_SIZE;
        let key_data_start = value_entry_start + count * VALUE_ENTRY_SIZE;
        let key_bytes = keys.try_fold(0_usize, |total, key| {
            if key.len() > u16::MAX as usize {
                Err(NativeBinaryJsonEncodeError::KeyTooLong)
            } else {
                total
                    .checked_add(key.len())
                    .ok_or(NativeBinaryJsonEncodeError::InvalidBinary)
            }
        })?;
        let value_data_start = key_data_start + key_bytes;
        let output = vec![0; key_data_start];
        let keys = Vec::with_capacity(key_bytes);
        let payload = Vec::new();
        Ok(Self {
            layout: NativeJsonContainerLayout {
                type_code: JSON_TYPE_CODE_OBJECT,
                count,
                value_entry_start,
                value_data_start,
                output,
                payload,
            },
            key_data_start,
            keys,
        })
    }

    pub(crate) fn push_key(
        &mut self,
        index: usize,
        key: &str,
    ) -> Result<(), NativeBinaryJsonEncodeError> {
        let entry = HEADER_SIZE + index * KEY_ENTRY_SIZE;
        let offset = u32::try_from(self.key_data_start + self.keys.len())
            .map_err(|_| NativeBinaryJsonEncodeError::InvalidBinary)?;
        self.layout.output[entry..entry + 4].copy_from_slice(&offset.to_le_bytes());
        self.layout.output[entry + 4..entry + 6].copy_from_slice(&(key.len() as u16).to_le_bytes());
        self.keys.extend_from_slice(key.as_bytes());
        Ok(())
    }

    pub(crate) fn push_value_serde(
        &mut self,
        index: usize,
        type_code: u8,
        value: &[u8],
    ) -> Result<(), NativeBinaryJsonEncodeError> {
        self.layout.push(
            index,
            type_code,
            value,
            LiteralPolicy::Index,
            OffsetPolicy::Plain,
        )
    }

    pub(crate) fn push_value_node(
        &mut self,
        index: usize,
        type_code: u8,
        value: &[u8],
    ) -> Result<(), NativeBinaryJsonEncodeError> {
        self.layout.push(
            index,
            type_code,
            value,
            LiteralPolicy::Checked,
            OffsetPolicy::Plain,
        )
    }

    pub(crate) fn finish(self) -> Result<(u8, Vec<u8>), NativeBinaryJsonEncodeError> {
        self.layout.finish(&self.keys)
    }
}

fn encode_binary_array(
    values: &[(u8, Vec<u8>)],
) -> Result<(u8, Vec<u8>), NativeBinaryJsonEncodeError> {
    let mut writer = NativeJsonArrayWriter::new(values.len());
    for (index, value) in values.iter().enumerate() {
        writer.push_node(index, value.0, &value.1)?;
    }
    writer.finish()
}

fn encode_binary_object(
    values: &[(&str, (u8, Vec<u8>))],
) -> Result<(u8, Vec<u8>), NativeBinaryJsonEncodeError> {
    let mut values = values.to_vec();
    values.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let mut writer = NativeJsonObjectWriter::new(values.iter().map(|(key, _)| *key))?;
    for (index, (key, value)) in values.iter().enumerate() {
        writer.push_key(index, key)?;
        writer.push_value_node(index, value.0, &value.1)?;
    }
    writer.finish()
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

#[cfg(test)]
#[test]
fn staged_writers_keep_fixed_layout_and_distinct_literal_offset_policies() {
    use NativeBinaryJsonEncodeError::{InvalidBinary, KeyTooLong};
    for serde in [false, true] {
        let mut array = NativeJsonArrayWriter::new(2);
        assert_eq!(&array.layout.output[..8], &[0; 8]);
        if serde {
            array.push_serde(0, 0x04, &[1, 99]).unwrap();
            array.push_serde(1, 0x0c, &[1, b'a']).unwrap();
        } else {
            array.push_node(0, 0x04, &[1, 99]).unwrap();
            array.push_node(1, 0x0c, &[1, b'a']).unwrap();
        }
        assert_eq!(
            array.finish().unwrap(),
            (
                0x03,
                vec![
                    2, 0, 0, 0, 20, 0, 0, 0, 0x04, 1, 0, 0, 0, 0x0c, 18, 0, 0, 0, 1, b'a',
                ]
            )
        );
        let mut object = NativeJsonObjectWriter::new(["a", "b"].into_iter()).unwrap();
        object.push_key(0, "a").unwrap();
        assert_eq!(object.keys, b"a");
        assert_eq!(object.layout.output[20], 0);
        if serde {
            object.push_value_serde(0, 0x04, &[1]).unwrap();
        } else {
            object.push_value_node(0, 0x04, &[1]).unwrap();
        }
        object.push_key(1, "b").unwrap();
        if serde {
            object.push_value_serde(1, 0x0c, &[1, b'x']).unwrap();
        } else {
            object.push_value_node(1, 0x0c, &[1, b'x']).unwrap();
        }
        assert_eq!(
            object.finish().unwrap(),
            (
                0x01,
                vec![
                    2, 0, 0, 0, 34, 0, 0, 0, 30, 0, 0, 0, 1, 0, 31, 0, 0, 0, 1, 0, 0x04, 1, 0, 0,
                    0, 0x0c, 32, 0, 0, 0, b'a', b'b', 1, b'x',
                ]
            )
        );
    }
    let mut array = NativeJsonArrayWriter::new(1);
    assert_eq!(array.push_node(0, 0x04, &[]), Err(InvalidBinary));
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| array.push_serde(
            0,
            0x04,
            &[]
        )))
        .is_err()
    );
    let mut object = NativeJsonObjectWriter::new(["a"].into_iter()).unwrap();
    object.push_key(0, "a").unwrap();
    assert_eq!(object.push_value_node(0, 0x04, &[]), Err(InvalidBinary));
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| object.push_value_serde(
            0,
            0x04,
            &[]
        )))
        .is_err()
    );
    let long_key = "x".repeat(usize::from(u16::MAX) + 1);
    assert!(matches!(
        NativeJsonObjectWriter::new([long_key.as_str()].into_iter()),
        Err(KeyTooLong)
    ));
    // Exercise offsets without allocating multi-gigabyte containers. Checked
    // serde-array addition must reject usize overflow before a payload append.
    let mut array = NativeJsonArrayWriter::new(1);
    array.layout.value_data_start = usize::MAX;
    array.layout.payload.push(0);
    assert_eq!(array.push_serde(0, 0x0c, &[0]), Err(InvalidBinary));
    assert_eq!(array.layout.payload, [0]);
    if let Ok(too_large) = usize::try_from(u64::from(u32::MAX) + 1) {
        let mut array = NativeJsonArrayWriter::new(1);
        array.layout.value_data_start = too_large;
        assert_eq!(array.push_node(0, 0x0c, &[0]), Err(InvalidBinary));
        let mut object = NativeJsonObjectWriter::new(["a"].into_iter()).unwrap();
        object.key_data_start = too_large;
        assert_eq!(object.push_key(0, "a"), Err(InvalidBinary));
        assert!(object.keys.is_empty());
        assert_eq!(&object.layout.output[8..14], &[0; 6]);
    }
}
