// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Legacy raw JSON value policies. Frontends prepare actual operands and retain
//! child demand; these cores preserve the original SDK codec stages and their
//! distinct absent-result versus unchanged-document outcomes.

use tidb_query_datatype::codec::mysql::json::{
    JsonType, NativeBinaryJsonModifyType, NativeBinaryJsonPathLeg, NativeJsonNode,
    decode_native_binary_json_node, encode_native_binary_json_node, extract_native_json_node,
    modify_native_json_node,
};

/// Applies legacy REPLACE after all complete path/value pairs are prepared.
/// Even an empty pair list decodes and re-encodes the document.
pub fn native_json_replace_raw_legacy(
    document: (u8, &[u8]),
    paths: &[(&[NativeBinaryJsonPathLeg], bool)],
    values: &[(u8, &[u8])],
) -> Option<(u8, Vec<u8>)> {
    modify_raw(document, paths, values, NativeBinaryJsonModifyType::Replace)
}

/// Applies one legacy ARRAY_APPEND pair. Failed extraction is an identity,
/// whereas a selected non-array or a later codec failure is an absent value.
pub fn native_json_array_append_raw_legacy(
    document: (u8, &[u8]),
    path: (&[NativeBinaryJsonPathLeg], bool),
    value: (u8, &[u8]),
) -> Option<(u8, Vec<u8>)> {
    if path.1 {
        return None;
    }
    let Some(target) = extract_raw(document, path) else {
        return Some((document.0, document.1.to_vec()));
    };
    if target.0 != JsonType::Array as u8 {
        return None;
    }

    // Preserve element_count's full decode, then every array_get's full decode
    // and selected-child encode. Reusing a flattened tree skips codec stages.
    let count = element_count_raw(raw_scalar(&target))?;
    let mut items = Vec::with_capacity(count + 1);
    for cell in 0..count {
        let element = array_get_raw(raw_scalar(&target), cell)?.expect("within count");
        items.push(element);
    }

    // BinaryJSONValue::Array(Binary(each), Binary(value)) decodes the encoded
    // children in order before decoding the actual value and encoding the array.
    let mut nodes = items
        .iter()
        .map(|item| decode_native_binary_json_node(item.0, &item.1))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    nodes.push(decode_native_binary_json_node(value.0, value.1).ok()?);
    let appended = encode_raw(&NativeJsonNode::Array(nodes))?;

    // Modify the ORIGINAL raw document, preserving its second decode and the
    // appended value's decode before the shared SET traversal and final encode.
    modify_raw(
        document,
        &[path],
        &[raw_scalar(&appended)],
        NativeBinaryJsonModifyType::Set,
    )
}

fn raw_scalar(value: &(u8, Vec<u8>)) -> (u8, &[u8]) {
    (value.0, &value.1)
}

fn encode_raw(node: &NativeJsonNode<(u8, Vec<u8>)>) -> Option<(u8, Vec<u8>)> {
    encode_native_binary_json_node(node, raw_scalar).ok()
}

fn modify_raw(
    document: (u8, &[u8]),
    paths: &[(&[NativeBinaryJsonPathLeg], bool)],
    values: &[(u8, &[u8])],
    mode: NativeBinaryJsonModifyType,
) -> Option<(u8, Vec<u8>)> {
    if paths.len() != values.len() {
        return None;
    }
    let mut document = decode_native_binary_json_node(document.0, document.1).ok()?;
    for ((legs, could_match_multiple), value) in paths.iter().zip(values) {
        // The original flags distinguish a quoted "*" key from a wildcard even
        // though their shared raw legs are identical. Never reconstruct flags.
        if *could_match_multiple {
            return None;
        }
        let value = decode_native_binary_json_node(value.0, value.1).ok()?;
        document = modify_native_json_node(document, legs, value, mode);
    }
    encode_raw(&document)
}

fn extract_raw(
    document: (u8, &[u8]),
    path: (&[NativeBinaryJsonPathLeg], bool),
) -> Option<(u8, Vec<u8>)> {
    let document = decode_native_binary_json_node(document.0, document.1).ok()?;
    let selected = extract_native_json_node(&document, &[path])?;
    encode_raw(&selected)
}

fn element_count_raw(value: (u8, &[u8])) -> Option<usize> {
    Some(
        match decode_native_binary_json_node(value.0, value.1).ok()? {
            NativeJsonNode::Array(values) => values.len(),
            NativeJsonNode::Object(values) => values.len(),
            NativeJsonNode::Scalar(_) => 1,
        },
    )
}

fn array_get_raw(value: (u8, &[u8]), index: usize) -> Option<Option<(u8, Vec<u8>)>> {
    let NativeJsonNode::Array(values) = decode_native_binary_json_node(value.0, value.1).ok()?
    else {
        return Some(None);
    };
    match values.get(index) {
        Some(value) => encode_raw(value).map(Some),
        None => Some(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_legacy_replace_raw_empty_codec_and_original_flags() {
        // A valid empty array with unused payload is canonicalized even with no
        // pairs. These expected bytes come from the original SDK wire layout.
        let padded_empty = [0, 0, 0, 0, 9, 0, 0, 0, 77];
        assert_eq!(
            native_json_replace_raw_legacy((3, &padded_empty), &[], &[]),
            Some((3, vec![0, 0, 0, 0, 8, 0, 0, 0])),
        );
        assert_eq!(native_json_replace_raw_legacy((3, &[255]), &[], &[]), None);
        assert_eq!(
            native_json_replace_raw_legacy((4, &[0]), &[(&[], false)], &[]),
            None,
        );
        let missing = [NativeBinaryJsonPathLeg::Key("missing".to_owned())];
        assert_eq!(
            native_json_replace_raw_legacy((4, &[0]), &[(&missing, false)], &[(255, &[])]),
            None,
        );

        // {"*": null}: the quoted key's false flag permits literal replacement.
        let object = [
            1, 0, 0, 0, 20, 0, 0, 0, 19, 0, 0, 0, 1, 0, 4, 0, 0, 0, 0, b'*',
        ];
        let star = [NativeBinaryJsonPathLeg::Key("*".to_owned())];
        assert_eq!(
            native_json_replace_raw_legacy((1, &object), &[(&star, false)], &[(4, &[1])]),
            Some((
                1,
                vec![
                    1, 0, 0, 0, 20, 0, 0, 0, 19, 0, 0, 0, 1, 0, 4, 1, 0, 0, 0, b'*'
                ]
            )),
        );
        assert_eq!(
            native_json_replace_raw_legacy((1, &object), &[(&star, true)], &[(4, &[1])]),
            None,
        );
        for (tag, payload) in [(13, vec![252, 2, 0xde, 0xad]), (14, vec![0; 8])] {
            assert_eq!(
                native_json_replace_raw_legacy((4, &[0]), &[(&[], false)], &[(tag, &payload)]),
                Some((tag, payload)),
            );
        }
    }

    #[test]
    fn test_legacy_append_raw_identity_stages_and_typed_payloads() {
        assert_eq!(
            native_json_array_append_raw_legacy((3, &[255]), (&[], false), (255, &[])),
            Some((3, vec![255])),
        );
        assert_eq!(
            native_json_array_append_raw_legacy((3, &[255]), (&[], true), (4, &[0])),
            None,
        );
        assert_eq!(
            native_json_array_append_raw_legacy((4, &[0]), (&[], false), (255, &[])),
            None,
        );
        let missing = [NativeBinaryJsonPathLeg::Key("missing".to_owned())];
        assert_eq!(
            native_json_array_append_raw_legacy((4, &[0]), (&missing, false), (255, &[])),
            Some((4, vec![0])),
        );
        let empty = [0, 0, 0, 0, 8, 0, 0, 0];
        assert_eq!(
            native_json_array_append_raw_legacy((3, &empty), (&[], false), (255, &[])),
            None,
        );

        // One raw DATE cell followed by an appended OPAQUE value. No textual
        // surrogate may replace either scalar tag or its payload.
        let dated = [
            1, 0, 0, 0, 21, 0, 0, 0, 14, 13, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(
            native_json_array_append_raw_legacy(
                (3, &dated),
                (&[], false),
                (13, &[252, 2, 0xde, 0xad])
            ),
            Some((
                3,
                vec![
                    2, 0, 0, 0, 30, 0, 0, 0, 14, 18, 0, 0, 0, 13, 26, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    0, 252, 2, 0xde, 0xad,
                ]
            )),
        );

        // Quoted "*" extracts the existing array through wildcard selection,
        // then SET inserts the literal "*" key into the original {"a": []}.
        let object = [
            1, 0, 0, 0, 28, 0, 0, 0, 19, 0, 0, 0, 1, 0, 3, 20, 0, 0, 0, b'a', 0, 0, 0, 0, 8, 0, 0,
            0,
        ];
        let star = [NativeBinaryJsonPathLeg::Key("*".to_owned())];
        assert_eq!(
            native_json_array_append_raw_legacy((1, &object), (&star, false), (4, &[0])),
            Some((
                1,
                vec![
                    2, 0, 0, 0, 53, 0, 0, 0, 30, 0, 0, 0, 1, 0, 31, 0, 0, 0, 1, 0, 3, 32, 0, 0, 0,
                    3, 45, 0, 0, 0, b'*', b'a', 1, 0, 0, 0, 13, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0,
                    8, 0, 0, 0,
                ]
            )),
        );
    }
}
