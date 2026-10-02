// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Shared node merge algorithms and the raw SDK's distinct codec staging.
//! Serde callers convert their actual trees to nodes; SQL NULL/reset policy
//! belongs to their sequence wrapper, not these structural operations.

use super::{
    NativeBinaryJsonEncodeError, NativeBinaryJsonError, NativeJsonNode,
    decode_native_binary_json_node, encode_native_binary_json_node,
};

/// Merges each adjacent run of objects before flattening the remaining arrays.
/// A left fold is not equivalent when an array interrupts two object runs.
/// Duplicate object keys recursively use this same algorithm.
pub fn merge_native_json_nodes<T: Clone>(values: &[NativeJsonNode<T>]) -> NativeJsonNode<T> {
    if values.is_empty() {
        return NativeJsonNode::Array(Vec::new());
    }
    let mut results = Vec::with_capacity(values.len());
    let mut index = 0;
    while index < values.len() {
        if matches!(values[index], NativeJsonNode::Object(_)) {
            let start = index;
            while index < values.len() && matches!(values[index], NativeJsonNode::Object(_)) {
                index += 1;
            }
            results.push(merge_objects(&values[start..index]));
        } else {
            results.push(values[index].clone());
            index += 1;
        }
    }
    if results.len() == 1 {
        return results.pop().expect("one merge result");
    }
    let mut flattened = Vec::new();
    for result in results {
        match result {
            NativeJsonNode::Array(values) => flattened.extend(values),
            value => flattened.push(value),
        }
    }
    NativeJsonNode::Array(flattened)
}

fn merge_objects<T: Clone>(objects: &[NativeJsonNode<T>]) -> NativeJsonNode<T> {
    let mut entries: Vec<(String, NativeJsonNode<T>)> = Vec::new();
    for object in objects {
        let NativeJsonNode::Object(values) = object else {
            unreachable!("merge_binary_objects receives only objects");
        };
        for (key, value) in values {
            if let Some(index) = entries.iter().position(|(name, _)| name == key) {
                let previous = entries[index].1.clone();
                entries[index].1 = merge_native_json_nodes(&[previous, value.clone()]);
            } else {
                entries.push((key.clone(), value.clone()));
            }
        }
    }
    entries.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    NativeJsonNode::Object(entries)
}

/// Applies one merge patch. Scalar callbacks are monomorphic representation
/// leaves, not evaluation bindings: one tests JSON null, one constructs it.
/// Raw duplicate keys retain first-match removal and remove-then-push ordering.
pub fn merge_patch_native_json_node<T: Clone>(
    target: NativeJsonNode<T>,
    patch: NativeJsonNode<T>,
    is_null: fn(&T) -> bool,
    null_scalar: fn() -> T,
) -> NativeJsonNode<T> {
    let NativeJsonNode::Object(patch) = patch else {
        return patch;
    };
    let mut target = match target {
        NativeJsonNode::Object(target) => target,
        _ => Vec::new(),
    };
    for (key, patch) in patch {
        if matches!(&patch, NativeJsonNode::Scalar(value) if is_null(value)) {
            if let Some(index) = target.iter().position(|(name, _)| name == &key) {
                target.remove(index);
            }
        } else {
            let current = target
                .iter()
                .position(|(name, _)| name == &key)
                .map(|index| target.remove(index).1)
                .unwrap_or_else(|| NativeJsonNode::Scalar(null_scalar()));
            target.push((
                key,
                merge_patch_native_json_node(current, patch, is_null, null_scalar),
            ));
        }
    }
    NativeJsonNode::Object(target)
}

fn decode_error(error: NativeBinaryJsonError) -> NativeBinaryJsonEncodeError {
    match error {
        NativeBinaryJsonError::InvalidBinary => NativeBinaryJsonEncodeError::InvalidBinary,
        NativeBinaryJsonError::TooDeep => NativeBinaryJsonEncodeError::TooDeep,
    }
}

/// The SDK's full raw preserve sequence: decode ALL inputs in order, merge,
/// then encode once. Empty input is an encoded empty array, not absence.
pub fn merge_native_binary_json(
    values: &[(u8, &[u8])],
) -> Result<(u8, Vec<u8>), NativeBinaryJsonEncodeError> {
    let values = values
        .iter()
        .map(|(kind, value)| decode_native_binary_json_node(*kind, value).map_err(decode_error))
        .collect::<Result<Vec<_>, _>>()?;
    let merged = merge_native_json_nodes(&values);
    encode_native_binary_json_node(&merged, |value| (value.0, &value.1))
}

/// The SDK's full raw patch sequence: empty input is absence; otherwise decode
/// the first value, interleave each subsequent decode with its patch, and only
/// encode the final result. Earlier malformed input cannot be discarded merely
/// because a later nonobject patch would replace the entire document.
pub fn merge_patch_native_binary_json(
    values: &[(u8, &[u8])],
) -> Result<Option<(u8, Vec<u8>)>, NativeBinaryJsonEncodeError> {
    let Some((kind, value)) = values.first() else {
        return Ok(None);
    };
    let mut result = decode_native_binary_json_node(*kind, value).map_err(decode_error)?;
    for (kind, patch) in &values[1..] {
        let patch = decode_native_binary_json_node(*kind, patch).map_err(decode_error)?;
        result = merge_patch_native_json_node(
            result,
            patch,
            |value| value.0 == 0x04 && value.1.as_slice() == [0],
            || (0x04, vec![0]),
        );
    }
    encode_native_binary_json_node(&result, |value| (value.0, &value.1)).map(Some)
}

#[cfg(test)]
mod tests {
    use NativeJsonNode::{Array, Object, Scalar};

    use super::*;

    fn encoded(node: &NativeJsonNode<(u8, Vec<u8>)>) -> (u8, Vec<u8>) {
        encode_native_binary_json_node(node, |value| (value.0, &value.1)).unwrap()
    }
    fn view(value: &(u8, Vec<u8>)) -> (u8, &[u8]) {
        (value.0, &value.1)
    }
    fn decoded(value: &(u8, Vec<u8>)) -> NativeJsonNode<(u8, Vec<u8>)> {
        decode_native_binary_json_node(value.0, &value.1).unwrap()
    }

    #[test]
    fn preserve_shares_object_runs_duplicates_and_encoder_depth_limit() {
        let duplicate = Object(vec![
            ("a".to_owned(), Scalar(2)),
            ("a".to_owned(), Scalar(3)),
        ]);
        let merged = merge_native_json_nodes(&[
            Object(vec![("x".to_owned(), Scalar(1))]),
            Array(vec![Scalar(9)]),
            duplicate.clone(),
            Object(vec![("a".to_owned(), Scalar(4))]),
        ]);
        assert_eq!(
            merged,
            Array(vec![
                Object(vec![("x".to_owned(), Scalar(1))]),
                Scalar(9),
                Object(vec![(
                    "a".to_owned(),
                    Array(vec![Scalar(2), Scalar(3), Scalar(4)])
                )]),
            ])
        );
        assert_eq!(
            merge_native_json_nodes(&[duplicate]),
            Object(vec![("a".to_owned(), Array(vec![Scalar(2), Scalar(3)]))])
        );
        assert_eq!(
            merge_native_binary_json(&[]).unwrap(),
            (0x03, vec![0, 0, 0, 0, 8, 0, 0, 0])
        );
        let opaque = (0x0d, vec![15, 2, 0xff, 0]);
        let time = (0x0e, 0_u64.to_le_bytes().to_vec());
        let actual = merge_native_binary_json(&[view(&opaque), view(&time)]).unwrap();
        assert_eq!(decoded(&actual), Array(vec![Scalar(opaque), Scalar(time)]));
        let mut deep = Scalar((0x04, vec![1]));
        for _ in 0..100 {
            deep = Object(vec![("a".to_owned(), deep)]);
        }
        // Exercise the merged depth boundary without the existing raw decoder's
        // repeated recursive validation. This is not a raw decode-error-order test.
        let merged = merge_native_json_nodes(&[deep.clone(), deep]);
        assert_eq!(
            encode_native_binary_json_node(&merged, |value| (value.0, &value.1)),
            Err(NativeBinaryJsonEncodeError::TooDeep)
        );
        // Shallow inputs keep malformed trailing-document coverage bounded.
        assert_eq!(
            merge_native_binary_json(&[(0x04, &[1]), (0x04, &[2]), (0x03, &[])]),
            Err(NativeBinaryJsonEncodeError::InvalidBinary)
        );
    }

    #[test]
    fn patch_keeps_first_duplicate_mutations_empty_and_singleton_codec_stages() {
        let target = Object(vec![
            ("a".to_owned(), Scalar(1)),
            ("a".to_owned(), Scalar(2)),
        ]);
        let deleted = merge_patch_native_json_node(
            target.clone(),
            Object(vec![("a".to_owned(), Scalar(0))]),
            |value| *value == 0,
            || 0,
        );
        assert_eq!(deleted, Object(vec![("a".to_owned(), Scalar(2))]));
        let replaced = merge_patch_native_json_node(
            target,
            Object(vec![("a".to_owned(), Scalar(3))]),
            |value| *value == 0,
            || 0,
        );
        assert_eq!(
            replaced,
            Object(vec![
                ("a".to_owned(), Scalar(2)),
                ("a".to_owned(), Scalar(3))
            ])
        );
        assert_eq!(merge_patch_native_binary_json(&[]).unwrap(), None);
        assert_eq!(
            merge_patch_native_binary_json(&[(0x03, &[]), (0x04, &[0])]),
            Err(NativeBinaryJsonEncodeError::InvalidBinary)
        );
        let duplicate = encoded(&Object(vec![
            ("a".to_owned(), Scalar((0x04, vec![1]))),
            ("a".to_owned(), Scalar((0x04, vec![2]))),
        ]));
        assert_eq!(
            merge_patch_native_binary_json(&[view(&duplicate)]).unwrap(),
            Some(duplicate.clone())
        );
        assert_eq!(
            merge_patch_native_binary_json(&[view(&duplicate), (0x04, &[0])]).unwrap(),
            Some((0x04, vec![0]))
        );
        let unsorted = (
            0x01,
            vec![
                2, 0, 0, 0, 32, 0, 0, 0, 30, 0, 0, 0, 1, 0, 31, 0, 0, 0, 1, 0, 0x04, 1, 0, 0, 0,
                0x04, 2, 0, 0, 0, b'z', b'a',
            ],
        );
        let actual = merge_patch_native_binary_json(&[view(&unsorted)])
            .unwrap()
            .unwrap();
        assert_ne!(actual, unsorted);
        assert_eq!(
            decoded(&actual),
            Object(vec![
                ("a".to_owned(), Scalar((0x04, vec![2]))),
                ("z".to_owned(), Scalar((0x04, vec![1])))
            ])
        );
        let opaque = (0x0d, vec![15, 2, 0xff, 0]);
        assert_eq!(
            merge_patch_native_binary_json(&[view(&duplicate), view(&opaque)]).unwrap(),
            Some(opaque)
        );
    }
}
