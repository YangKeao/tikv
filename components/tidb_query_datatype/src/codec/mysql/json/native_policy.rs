// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native JSON representation policies. Native decoding and comparison retain
//! their own malformed-input boundaries, separate from wire `JsonRef` policy.

use std::cmp::Ordering;

use codec::number::NumberCodec;
use serde_json::{Map, Number, Value};

use super::{
    JsonType,
    constants::{
        HEADER_LEN as HEADER_SIZE, JSON_LITERAL_FALSE, JSON_LITERAL_NIL as JSON_LITERAL_NULL,
        JSON_LITERAL_TRUE, KEY_ENTRY_LEN as KEY_ENTRY_SIZE, LITERAL_LEN, NUMBER_LEN, TYPE_LEN,
        VALUE_ENTRY_LEN as VALUE_ENTRY_SIZE,
    },
    jcodec::{array_metadata_len, object_metadata_len, out_of_line_payload_len},
    json_type::json_type_name,
};
use crate::codec::mysql::Time;

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

const MAX_NATIVE_JSON_DEPTH: usize = 100;
const FLOAT_EPSILON: f64 = 1e-8;
const JSON_TYPE_CODE_OBJECT: u8 = JsonType::Object as u8;
const JSON_TYPE_CODE_ARRAY: u8 = JsonType::Array as u8;
const JSON_TYPE_CODE_LITERAL: u8 = JsonType::Literal as u8;
const JSON_TYPE_CODE_INT64: u8 = JsonType::I64 as u8;
const JSON_TYPE_CODE_UINT64: u8 = JsonType::U64 as u8;
const JSON_TYPE_CODE_FLOAT64: u8 = JsonType::Double as u8;
const JSON_TYPE_CODE_STRING: u8 = JsonType::String as u8;
const JSON_TYPE_CODE_OPAQUE: u8 = JsonType::Opaque as u8;
const JSON_TYPE_CODE_DATE: u8 = JsonType::Date as u8;
const JSON_TYPE_CODE_DATETIME: u8 = JsonType::Datetime as u8;
const JSON_TYPE_CODE_TIMESTAMP: u8 = JsonType::Timestamp as u8;
const JSON_TYPE_CODE_DURATION: u8 = JsonType::Time as u8;

/// The two original native binary-decoder failures. Text/parser errors remain
/// in `NativeJsonError`; a binary depth error is not collapsed into bad bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeBinaryJsonError {
    InvalidBinary,
    TooDeep,
}

/// Lossless container structure with an owner-selected scalar carrier. Object
/// entries remain a vector: duplicate keys and their original count survive.
#[derive(Clone, Debug, PartialEq)]
pub enum NativeJsonNode<T> {
    Scalar(T),
    Array(Vec<NativeJsonNode<T>>),
    Object(Vec<(String, NativeJsonNode<T>)>),
}

/// Native SDK KEYS preserves duplicate object keys and returns an empty list
/// for non-objects. Scalar payloads are opaque to this structural operation.
pub fn native_json_sorted_object_keys<T>(node: &NativeJsonNode<T>) -> Vec<String> {
    match node {
        NativeJsonNode::Object(values) => {
            let mut keys = values
                .iter()
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            keys.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
            keys
        }
        _ => Vec::new(),
    }
}

type NativeBinaryJsonNode = NativeJsonNode<(u8, Vec<u8>)>;

/// Decodes the original native lossless tree, preserving each scalar's tag and
/// exact payload. This is not the wire JSON decoder or a serde sentinel format.
pub fn decode_native_binary_json_node(
    type_code: u8,
    value: &[u8],
) -> Result<NativeJsonNode<(u8, Vec<u8>)>, NativeBinaryJsonError> {
    decode_native_node(type_code, value, 0)
}

/// Decodes into the original native serde value model. Its scalar admission and
/// malformed-entry slice behavior intentionally remain distinct from the
/// lossless node decoder, as in the original `BinaryJSON::to_value` API.
pub fn decode_native_binary_json_value(
    type_code: u8,
    value: &[u8],
) -> Result<Value, NativeBinaryJsonError> {
    decode_native_value(type_code, value, 0)
}

/// Compares native type-code/payload pairs without changing wire
/// `JsonRef::cmp`. Rank is checked first. Malformed equal-rank
/// containers/opaque/temporal values compare equal; ordinary scalar decode
/// failures fall back to raw payloads.
pub fn compare_native_binary_json(
    left_type: u8,
    left_raw: &[u8],
    right_type: u8,
    right_raw: &[u8],
) -> Ordering {
    let left_rank = native_json_precedence(left_type, left_raw);
    let right_rank = native_json_precedence(right_type, right_raw);
    if left_rank != right_rank {
        return left_rank.cmp(&right_rank);
    }
    if left_type == JSON_TYPE_CODE_OPAQUE && right_type == JSON_TYPE_CODE_OPAQUE {
        return match (
            native_json_opaque(left_type, left_raw),
            native_json_opaque(right_type, right_raw),
        ) {
            (Ok((_, left)), Ok((_, right))) => left.cmp(right),
            _ => Ordering::Equal,
        };
    }
    if matches!(
        left_type,
        JSON_TYPE_CODE_DATE | JSON_TYPE_CODE_DATETIME | JSON_TYPE_CODE_TIMESTAMP
    ) && matches!(
        right_type,
        JSON_TYPE_CODE_DATE | JSON_TYPE_CODE_DATETIME | JSON_TYPE_CODE_TIMESTAMP
    ) {
        // Native Time::new(core, kind, 0) does not validate calendar fields.
        // Only the exact eight-byte shape is checked before core comparison.
        return match (
            <[u8; 8]>::try_from(left_raw),
            <[u8; 8]>::try_from(right_raw),
        ) {
            (Ok(left), Ok(right)) => {
                Time::native_core_compare(u64::from_le_bytes(left), u64::from_le_bytes(right))
            }
            _ => Ordering::Equal,
        };
    }
    if left_type == JSON_TYPE_CODE_DURATION && right_type == JSON_TYPE_CODE_DURATION {
        return if left_raw.len() == 12 && right_raw.len() == 12 {
            i64::from_le_bytes(left_raw[..8].try_into().unwrap())
                .cmp(&i64::from_le_bytes(right_raw[..8].try_into().unwrap()))
        } else {
            Ordering::Equal
        };
    }
    if matches!(left_type, JSON_TYPE_CODE_ARRAY | JSON_TYPE_CODE_OBJECT) && left_type == right_type
    {
        return match (
            decode_native_binary_json_node(left_type, left_raw),
            decode_native_binary_json_node(right_type, right_raw),
        ) {
            (Ok(left), Ok(right)) => compare_native_container_nodes(&left, &right),
            _ => Ordering::Equal,
        };
    }
    match (
        decode_native_binary_json_value(left_type, left_raw),
        decode_native_binary_json_value(right_type, right_raw),
    ) {
        (Ok(left), Ok(right)) => compare_json_value(&left, &right),
        _ => left_raw.cmp(right_raw),
    }
}

fn compare_native_container_nodes(
    left: &NativeBinaryJsonNode,
    right: &NativeBinaryJsonNode,
) -> Ordering {
    match (left, right) {
        (NativeJsonNode::Array(left), NativeJsonNode::Array(right)) => left
            .iter()
            .zip(right)
            .map(|(left, right)| compare_native_nodes(left, right))
            .find(|ordering| !ordering.is_eq())
            .unwrap_or_else(|| left.len().cmp(&right.len())),
        (NativeJsonNode::Object(left), NativeJsonNode::Object(right)) => {
            let count = left.len().cmp(&right.len());
            if !count.is_eq() {
                return count;
            }
            let mut left = left.iter().collect::<Vec<_>>();
            let mut right = right.iter().collect::<Vec<_>>();
            left.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            right.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            left.into_iter()
                .zip(right)
                .find_map(|((left_key, left_value), (right_key, right_value))| {
                    let key = left_key.as_bytes().cmp(right_key.as_bytes());
                    if !key.is_eq() {
                        return Some(key);
                    }
                    let value = compare_native_nodes(left_value, right_value);
                    (!value.is_eq()).then_some(value)
                })
                .unwrap_or(Ordering::Equal)
        }
        _ => Ordering::Equal,
    }
}

fn compare_native_nodes(left: &NativeBinaryJsonNode, right: &NativeBinaryJsonNode) -> Ordering {
    match (left, right) {
        (
            NativeJsonNode::Scalar((left_type, left)),
            NativeJsonNode::Scalar((right_type, right)),
        ) => compare_native_binary_json(*left_type, left, *right_type, right),
        (NativeJsonNode::Array(_), NativeJsonNode::Array(_))
        | (NativeJsonNode::Object(_), NativeJsonNode::Object(_)) => {
            compare_native_container_nodes(left, right)
        }
        _ => {
            // The original path re-encoded successfully decoded nodes solely
            // to recover their rank. Decoding already bounds depth and keys
            // (u16 lengths); scalar encoding just clones. No encode failure
            // or numeric transformation is possible here, so inspect the tag.
            let rank = |node: &NativeBinaryJsonNode| match node {
                NativeJsonNode::Scalar((kind, value)) => native_json_precedence(*kind, value),
                NativeJsonNode::Array(_) => 7,
                NativeJsonNode::Object(_) => 6,
            };
            rank(left).cmp(&rank(right))
        }
    }
}

/// Native SDK containment over lossless binary nodes, not serde-value policy.
pub fn contains_native_binary_json(
    object_type: u8,
    object_raw: &[u8],
    target_type: u8,
    target_raw: &[u8],
) -> Result<bool, NativeBinaryJsonError> {
    let object = decode_native_binary_json_node(object_type, object_raw)?;
    let target = decode_native_binary_json_node(target_type, target_raw)?;
    native_contains_node(&object, &target)
}

/// Native SDK overlap retains fallible whole-value equality one level down.
pub fn overlaps_native_binary_json(
    object_type: u8,
    object_raw: &[u8],
    target_type: u8,
    target_raw: &[u8],
) -> Result<bool, NativeBinaryJsonError> {
    let object = decode_native_binary_json_node(object_type, object_raw)?;
    let target = decode_native_binary_json_node(target_type, target_raw)?;
    native_overlaps_node(&object, &target)
}

fn native_contains_node(
    object: &NativeBinaryJsonNode,
    target: &NativeBinaryJsonNode,
) -> Result<bool, NativeBinaryJsonError> {
    Ok(match (object, target) {
        (NativeJsonNode::Object(object), NativeJsonNode::Object(target)) => {
            target.iter().all(|(key, target)| {
                object
                    .iter()
                    .find(|(name, _)| name == key)
                    .is_some_and(|(_, object)| {
                        native_contains_node(object, target).unwrap_or(false)
                    })
            })
        }
        (NativeJsonNode::Array(object), NativeJsonNode::Array(target)) => {
            target.iter().all(|target| {
                object
                    .iter()
                    .any(|object| native_contains_node(object, target).unwrap_or(false))
            })
        }
        (NativeJsonNode::Array(object), target) => object
            .iter()
            .any(|object| native_contains_node(object, target).unwrap_or(false)),
        _ => native_predicate_nodes_equal(object, target)?,
    })
}

fn native_overlaps_node(
    left: &NativeBinaryJsonNode,
    right: &NativeBinaryJsonNode,
) -> Result<bool, NativeBinaryJsonError> {
    let (object, target) = match (left, right) {
        (
            left @ (NativeJsonNode::Object(_) | NativeJsonNode::Scalar(_)),
            right @ NativeJsonNode::Array(_),
        ) => (right, left),
        _ => (left, right),
    };
    Ok(match (object, target) {
        (NativeJsonNode::Object(object), NativeJsonNode::Object(target)) => {
            target.iter().try_fold(false, |found, (key, value)| {
                if found {
                    return Ok(true);
                }
                match object.iter().find(|(name, _)| name == key) {
                    Some((_, existing)) => native_predicate_nodes_equal(existing, value),
                    None => Ok(false),
                }
            })?
        }
        (NativeJsonNode::Object(_), _) => false,
        (NativeJsonNode::Array(object), NativeJsonNode::Array(target)) => {
            object.iter().try_fold(false, |found, element| {
                if found {
                    return Ok(true);
                }
                target.iter().try_fold(false, |found, other| {
                    if found {
                        return Ok(true);
                    }
                    native_predicate_nodes_equal(element, other)
                })
            })?
        }
        (NativeJsonNode::Array(object), target) => {
            object.iter().try_fold(false, |found, element| {
                if found {
                    return Ok(true);
                }
                native_predicate_nodes_equal(element, target)
            })?
        }
        (object, target) => native_predicate_nodes_equal(object, target)?,
    })
}

fn native_predicate_nodes_equal(
    left: &NativeBinaryJsonNode,
    right: &NativeBinaryJsonNode,
) -> Result<bool, NativeBinaryJsonError> {
    let (left, _) = normalize_native_predicate_node(left, 0)?;
    let (right, _) = normalize_native_predicate_node(right, 0)?;
    Ok(compare_native_nodes(&left, &right).is_eq())
}

// The original SDK equality leaves called from_node before comparing. Preserve
// its sorted (not deduplicated) objects and representability checks, but do not
// emit binary bytes merely to decode them again. Overlapping source offsets can
// expand on re-encoding, so successfully decoded input alone is not sufficient.
fn normalize_native_predicate_node(
    node: &NativeBinaryJsonNode,
    depth: usize,
) -> Result<(NativeBinaryJsonNode, usize), NativeBinaryJsonError> {
    if depth > MAX_NATIVE_JSON_DEPTH {
        return Err(NativeBinaryJsonError::TooDeep);
    }
    let invalid = || NativeBinaryJsonError::InvalidBinary;
    let add = |left: usize, right: usize| left.checked_add(right).ok_or_else(invalid);
    let finish_size = |size: usize| -> Result<usize, NativeBinaryJsonError> {
        u32::try_from(size).map_err(|_| NativeBinaryJsonError::InvalidBinary)?;
        Ok(size)
    };
    let out_of_line = |node: &NativeBinaryJsonNode, size| match node {
        NativeJsonNode::Scalar((JSON_TYPE_CODE_LITERAL, _)) => 0,
        _ => size,
    };
    match node {
        NativeJsonNode::Scalar((kind, value)) => {
            Ok((NativeJsonNode::Scalar((*kind, value.clone())), value.len()))
        }
        NativeJsonNode::Array(values) => {
            // Source encode_node visits all children before encoding a header.
            let children = values
                .iter()
                .map(|value| normalize_native_predicate_node(value, depth + 1))
                .collect::<Result<Vec<_>, _>>()?;
            let mut size = add(
                HEADER_SIZE,
                values
                    .len()
                    .checked_mul(VALUE_ENTRY_SIZE)
                    .ok_or_else(invalid)?,
            )?;
            for (child, length) in &children {
                size = add(size, out_of_line(child, *length))?;
            }
            u32::try_from(values.len()).map_err(|_| invalid())?;
            let size = finish_size(size)?;
            Ok((
                NativeJsonNode::Array(children.into_iter().map(|(child, _)| child).collect()),
                size,
            ))
        }
        NativeJsonNode::Object(values) => {
            let mut children = values
                .iter()
                .map(|(key, value)| {
                    normalize_native_predicate_node(value, depth + 1)
                        .map(|(value, length)| (key.clone(), value, length))
                })
                .collect::<Result<Vec<_>, _>>()?;
            children.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
            let mut size = add(
                HEADER_SIZE,
                values
                    .len()
                    .checked_mul(KEY_ENTRY_SIZE + VALUE_ENTRY_SIZE)
                    .ok_or_else(invalid)?,
            )?;
            for (key, ..) in &children {
                // The original decoder already restricts each key length to u16.
                u16::try_from(key.len()).map_err(|_| invalid())?;
                size = add(size, key.len())?;
            }
            for (_, child, length) in &children {
                size = add(size, out_of_line(child, *length))?;
            }
            u32::try_from(values.len()).map_err(|_| invalid())?;
            let size = finish_size(size)?;
            Ok((
                NativeJsonNode::Object(
                    children
                        .into_iter()
                        .map(|(key, value, _)| (key, value))
                        .collect(),
                ),
                size,
            ))
        }
    }
}

/// Legacy MEMBER OF. The original element_count fully decoded the array before
/// scanning it; each array_get then re-encoded a child and skipped its failure.
/// Scalar documents instead take the original raw comparator's fallbacks.
pub fn member_of_native_binary_json(
    candidate_type: u8,
    candidate_raw: &[u8],
    document_type: u8,
    document_raw: &[u8],
) -> Result<bool, NativeBinaryJsonError> {
    if document_type != JSON_TYPE_CODE_ARRAY {
        return Ok(compare_native_binary_json(
            document_type,
            document_raw,
            candidate_type,
            candidate_raw,
        )
        .is_eq());
    }
    let NativeJsonNode::Array(values) =
        decode_native_binary_json_node(document_type, document_raw)?
    else {
        unreachable!("array tag decoded to non-array node");
    };
    for value in &values {
        let Ok((value, _)) = normalize_native_predicate_node(value, 0) else {
            continue;
        };
        let ordering = match &value {
            NativeJsonNode::Scalar((kind, raw)) => {
                compare_native_binary_json(*kind, raw, candidate_type, candidate_raw)
            }
            NativeJsonNode::Array(_) | NativeJsonNode::Object(_) => {
                let rank = if matches!(value, NativeJsonNode::Array(_)) {
                    7
                } else {
                    6
                };
                let candidate_rank = native_json_precedence(candidate_type, candidate_raw);
                if rank != candidate_rank {
                    rank.cmp(&candidate_rank)
                } else {
                    match decode_native_binary_json_node(candidate_type, candidate_raw) {
                        Ok(candidate) => compare_native_nodes(&value, &candidate),
                        Err(_) => Ordering::Equal,
                    }
                }
            }
        };
        if ordering.is_eq() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn native_json_precedence(type_code: u8, value: &[u8]) -> i8 {
    match type_code {
        JSON_TYPE_CODE_OPAQUE => match native_binary_json_type_name(type_code, value) {
            Ok(b"BLOB") => 14,
            Ok(b"BIT") => 13,
            _ => 12,
        },
        JSON_TYPE_CODE_DATETIME | JSON_TYPE_CODE_TIMESTAMP => 11,
        JSON_TYPE_CODE_DURATION => 10,
        JSON_TYPE_CODE_DATE => 9,
        JSON_TYPE_CODE_LITERAL if value != [JSON_LITERAL_NULL] => 8,
        JSON_TYPE_CODE_ARRAY => 7,
        JSON_TYPE_CODE_OBJECT => 6,
        JSON_TYPE_CODE_STRING => 5,
        JSON_TYPE_CODE_INT64 | JSON_TYPE_CODE_UINT64 | JSON_TYPE_CODE_FLOAT64 => 4,
        JSON_TYPE_CODE_LITERAL => 3,
        _ => 0,
    }
}

fn compare_json_value(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        (Value::Number(left), Value::Number(right)) => compare_json_number(left, right),
        (Value::String(left), Value::String(right)) => left.as_bytes().cmp(right.as_bytes()),
        (Value::Array(left), Value::Array(right)) => left
            .iter()
            .zip(right)
            .map(|(left, right)| compare_json_value(left, right))
            .find(|ordering| !ordering.is_eq())
            .unwrap_or_else(|| left.len().cmp(&right.len())),
        (Value::Object(left), Value::Object(right)) => {
            let count = left.len().cmp(&right.len());
            if !count.is_eq() {
                return count;
            }
            let mut left = left.iter().collect::<Vec<_>>();
            let mut right = right.iter().collect::<Vec<_>>();
            left.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            right.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            left.into_iter()
                .zip(right)
                .find_map(|((left_key, left_value), (right_key, right_value))| {
                    let key = left_key.as_bytes().cmp(right_key.as_bytes());
                    if !key.is_eq() {
                        return Some(key);
                    }
                    let value = compare_json_value(left_value, right_value);
                    (!value.is_eq()).then_some(value)
                })
                .unwrap_or(Ordering::Equal)
        }
        _ => value_precedence(left).cmp(&value_precedence(right)),
    }
}

fn value_precedence(value: &Value) -> i8 {
    match value {
        Value::Null => 3,
        Value::Number(_) => 4,
        Value::String(_) => 5,
        Value::Object(_) => 6,
        Value::Array(_) => 7,
        Value::Bool(_) => 8,
    }
}

fn compare_json_number(left: &Number, right: &Number) -> Ordering {
    match (
        left.as_i64(),
        left.as_u64(),
        left.as_f64(),
        right.as_i64(),
        right.as_u64(),
        right.as_f64(),
    ) {
        (Some(left), _, _, Some(right), ..) => left.cmp(&right),
        (Some(left), _, _, None, Some(right), _) => {
            if left < 0 {
                Ordering::Less
            } else {
                (left as u64).cmp(&right)
            }
        }
        (None, Some(left), _, Some(right), ..) => {
            if right < 0 {
                Ordering::Greater
            } else {
                left.cmp(&(right as u64))
            }
        }
        (None, Some(left), _, None, Some(right), _) => left.cmp(&right),
        // Two doubles compare exactly; epsilon only admits precision loss
        // when widening an integer for comparison with a double.
        (None, None, Some(left), None, None, Some(right)) => {
            left.partial_cmp(&right).unwrap_or(Ordering::Greater)
        }
        (_, _, Some(left), _, _, Some(right)) => {
            if (left - right).abs() < FLOAT_EPSILON {
                Ordering::Equal
            } else {
                left.partial_cmp(&right).unwrap_or(Ordering::Greater)
            }
        }
        _ => Ordering::Equal,
    }
}

fn decode_native_node(
    type_code: u8,
    value: &[u8],
    depth: usize,
) -> Result<NativeBinaryJsonNode, NativeBinaryJsonError> {
    if depth > MAX_NATIVE_JSON_DEPTH {
        return Err(NativeBinaryJsonError::TooDeep);
    }
    match type_code {
        JSON_TYPE_CODE_ARRAY => {
            let (count, size) = read_native_header(value)?;
            if size != value.len() || HEADER_SIZE + count * VALUE_ENTRY_SIZE > size {
                return Err(NativeBinaryJsonError::InvalidBinary);
            }
            let mut values = Vec::with_capacity(count);
            for index in 0..count {
                let entry = HEADER_SIZE + index * VALUE_ENTRY_SIZE;
                let (kind, child) =
                    decode_native_binary_entry(value[entry], &value[entry + 1..entry + 5], value)?;
                values.push(decode_native_node(kind, &child, depth + 1)?);
            }
            Ok(NativeJsonNode::Array(values))
        }
        JSON_TYPE_CODE_OBJECT => {
            let (count, size) = read_native_header(value)?;
            let value_entries = HEADER_SIZE + count * KEY_ENTRY_SIZE;
            if size != value.len() || value_entries + count * VALUE_ENTRY_SIZE > size {
                return Err(NativeBinaryJsonError::InvalidBinary);
            }
            let mut values = Vec::with_capacity(count);
            for index in 0..count {
                let key_entry = HEADER_SIZE + index * KEY_ENTRY_SIZE;
                let key_offset =
                    u32::from_le_bytes(value[key_entry..key_entry + 4].try_into().unwrap())
                        as usize;
                let key_length =
                    u16::from_le_bytes(value[key_entry + 4..key_entry + 6].try_into().unwrap())
                        as usize;
                let key = std::str::from_utf8(
                    value
                        .get(key_offset..key_offset + key_length)
                        .ok_or(NativeBinaryJsonError::InvalidBinary)?,
                )
                .map_err(|_| NativeBinaryJsonError::InvalidBinary)?
                .to_owned();
                let entry = value_entries + index * VALUE_ENTRY_SIZE;
                let (kind, child) =
                    decode_native_binary_entry(value[entry], &value[entry + 1..entry + 5], value)?;
                values.push((key, decode_native_node(kind, &child, depth + 1)?));
            }
            Ok(NativeJsonNode::Object(values))
        }
        _ => {
            validate_native_scalar(type_code, value)?;
            Ok(NativeJsonNode::Scalar((type_code, value.to_vec())))
        }
    }
}

fn validate_native_scalar(type_code: u8, value: &[u8]) -> Result<(), NativeBinaryJsonError> {
    match type_code {
        JSON_TYPE_CODE_OPAQUE => {
            native_json_opaque(type_code, value)
                .map_err(|_| NativeBinaryJsonError::InvalidBinary)?;
            Ok(())
        }
        JSON_TYPE_CODE_DATE | JSON_TYPE_CODE_DATETIME | JSON_TYPE_CODE_TIMESTAMP
            if value.len() == 8 =>
        {
            Ok(())
        }
        JSON_TYPE_CODE_DURATION if value.len() == 12 => Ok(()),
        JSON_TYPE_CODE_OBJECT | JSON_TYPE_CODE_ARRAY => Err(NativeBinaryJsonError::InvalidBinary),
        _ => decode_native_binary_json_value(type_code, value).map(|_| ()),
    }
}

fn decode_native_binary_entry(
    type_code: u8,
    entry: &[u8],
    container: &[u8],
) -> Result<(u8, Vec<u8>), NativeBinaryJsonError> {
    let value = if type_code == JSON_TYPE_CODE_LITERAL {
        vec![entry[0]]
    } else {
        let offset = u32::from_le_bytes(
            entry
                .try_into()
                .map_err(|_| NativeBinaryJsonError::InvalidBinary)?,
        ) as usize;
        let value = container
            .get(offset..)
            .ok_or(NativeBinaryJsonError::InvalidBinary)?;
        let length = native_value_length(type_code, value)?;
        value
            .get(..length)
            .ok_or(NativeBinaryJsonError::InvalidBinary)?
            .to_vec()
    };
    // Original BinaryJSON::from_raw validated this child at depth zero before
    // the caller decoded it again at its outer depth. Preserve both passes.
    decode_native_node(type_code, &value, 0)?;
    Ok((type_code, value))
}

fn decode_native_value(
    type_code: u8,
    bytes: &[u8],
    depth: usize,
) -> Result<Value, NativeBinaryJsonError> {
    if depth > MAX_NATIVE_JSON_DEPTH {
        return Err(NativeBinaryJsonError::TooDeep);
    }
    match type_code {
        JSON_TYPE_CODE_LITERAL if bytes == [JSON_LITERAL_NULL] => Ok(Value::Null),
        JSON_TYPE_CODE_LITERAL if bytes == [JSON_LITERAL_TRUE] => Ok(Value::Bool(true)),
        JSON_TYPE_CODE_LITERAL if bytes == [JSON_LITERAL_FALSE] => Ok(Value::Bool(false)),
        JSON_TYPE_CODE_INT64 if bytes.len() == 8 => Ok(Value::Number(
            i64::from_le_bytes(bytes.try_into().expect("length checked")).into(),
        )),
        JSON_TYPE_CODE_UINT64 if bytes.len() == 8 => Ok(Value::Number(
            u64::from_le_bytes(bytes.try_into().expect("length checked")).into(),
        )),
        JSON_TYPE_CODE_FLOAT64 if bytes.len() == 8 => {
            let value = f64::from_bits(u64::from_le_bytes(
                bytes.try_into().expect("length checked"),
            ));
            Number::from_f64(value)
                .map(Value::Number)
                .ok_or(NativeBinaryJsonError::InvalidBinary)
        }
        JSON_TYPE_CODE_STRING => {
            let (length, prefix) = native_binary_uvarint(bytes)?;
            let text = std::str::from_utf8(
                bytes
                    .get(prefix..prefix + length)
                    .ok_or(NativeBinaryJsonError::InvalidBinary)?,
            )
            .map_err(|_| NativeBinaryJsonError::InvalidBinary)?;
            if prefix + length != bytes.len() {
                return Err(NativeBinaryJsonError::InvalidBinary);
            }
            Ok(Value::String(text.to_owned()))
        }
        JSON_TYPE_CODE_ARRAY => decode_native_array(bytes, depth + 1),
        JSON_TYPE_CODE_OBJECT => decode_native_object(bytes, depth + 1),
        _ => Err(NativeBinaryJsonError::InvalidBinary),
    }
}

fn decode_native_array(bytes: &[u8], depth: usize) -> Result<Value, NativeBinaryJsonError> {
    let (count, size) = read_native_header(bytes)?;
    if size != bytes.len() || HEADER_SIZE + count * VALUE_ENTRY_SIZE > bytes.len() {
        return Err(NativeBinaryJsonError::InvalidBinary);
    }
    let mut values = Vec::with_capacity(count);
    for index in 0..count {
        let entry = HEADER_SIZE + index * VALUE_ENTRY_SIZE;
        values.push(decode_native_entry(
            bytes[entry],
            &bytes[entry + 1..entry + 5],
            bytes,
            depth,
        )?);
    }
    Ok(Value::Array(values))
}

fn decode_native_object(bytes: &[u8], depth: usize) -> Result<Value, NativeBinaryJsonError> {
    let (count, size) = read_native_header(bytes)?;
    let value_entries = HEADER_SIZE + count * KEY_ENTRY_SIZE;
    if size != bytes.len() || value_entries + count * VALUE_ENTRY_SIZE > bytes.len() {
        return Err(NativeBinaryJsonError::InvalidBinary);
    }
    let mut values = Map::new();
    for index in 0..count {
        let key_entry = HEADER_SIZE + index * KEY_ENTRY_SIZE;
        let key_offset =
            u32::from_le_bytes(bytes[key_entry..key_entry + 4].try_into().unwrap()) as usize;
        let key_length =
            u16::from_le_bytes(bytes[key_entry + 4..key_entry + 6].try_into().unwrap()) as usize;
        let key = std::str::from_utf8(
            bytes
                .get(key_offset..key_offset + key_length)
                .ok_or(NativeBinaryJsonError::InvalidBinary)?,
        )
        .map_err(|_| NativeBinaryJsonError::InvalidBinary)?;
        let entry = value_entries + index * VALUE_ENTRY_SIZE;
        values.insert(
            key.to_owned(),
            decode_native_entry(bytes[entry], &bytes[entry + 1..entry + 5], bytes, depth)?,
        );
    }
    Ok(Value::Object(values))
}

fn decode_native_entry(
    type_code: u8,
    entry: &[u8],
    container: &[u8],
    depth: usize,
) -> Result<Value, NativeBinaryJsonError> {
    if type_code == JSON_TYPE_CODE_LITERAL {
        return decode_native_value(type_code, &entry[..1], depth);
    }
    let offset = u32::from_le_bytes(entry.try_into().unwrap()) as usize;
    let value = container
        .get(offset..)
        .ok_or(NativeBinaryJsonError::InvalidBinary)?;
    let length = native_value_length(type_code, value)?;
    // Intentionally unchecked: the original serde path panics for an entry
    // whose claimed payload extends past the container; the node path refuses.
    decode_native_value(type_code, &value[..length], depth)
}

fn native_value_length(type_code: u8, bytes: &[u8]) -> Result<usize, NativeBinaryJsonError> {
    match type_code {
        JSON_TYPE_CODE_OBJECT | JSON_TYPE_CODE_ARRAY => {
            let (_, size) = read_native_header(bytes)?;
            Ok(size)
        }
        JSON_TYPE_CODE_INT64 | JSON_TYPE_CODE_UINT64 | JSON_TYPE_CODE_FLOAT64 => Ok(8),
        JSON_TYPE_CODE_DATE | JSON_TYPE_CODE_DATETIME | JSON_TYPE_CODE_TIMESTAMP => Ok(8),
        JSON_TYPE_CODE_DURATION => Ok(12),
        JSON_TYPE_CODE_OPAQUE => {
            let payload = bytes.get(1..).ok_or(NativeBinaryJsonError::InvalidBinary)?;
            let (length, prefix) = native_binary_uvarint(payload)?;
            Ok(1 + prefix + length)
        }
        JSON_TYPE_CODE_STRING => {
            let (length, prefix) = native_binary_uvarint(bytes)?;
            Ok(prefix + length)
        }
        _ => Err(NativeBinaryJsonError::InvalidBinary),
    }
}

fn read_native_header(bytes: &[u8]) -> Result<(usize, usize), NativeBinaryJsonError> {
    let header = bytes
        .get(..HEADER_SIZE)
        .ok_or(NativeBinaryJsonError::InvalidBinary)?;
    let count = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    Ok((count, size))
}

fn native_binary_uvarint(bytes: &[u8]) -> Result<(usize, usize), NativeBinaryJsonError> {
    decode_native_json_uvarint(bytes).map_err(|_| NativeBinaryJsonError::InvalidBinary)
}

#[cfg(test)]
mod native_binary_tests {
    use super::*;

    #[test]
    fn native_binary_comparison_keeps_scalar_fallbacks_and_precision() {
        let integer = 10_i64.to_le_bytes();
        let near = (10.0_f64 + 5e-9).to_le_bytes();
        assert_eq!(
            compare_native_binary_json(0x09, &integer, 0x0b, &near),
            Ordering::Equal
        );
        assert_eq!(
            compare_native_binary_json(0x0b, &10.0_f64.to_le_bytes(), 0x0b, &near),
            Ordering::Less
        );
        assert_eq!(
            compare_native_binary_json(
                0x09,
                &(-1_i64).to_le_bytes(),
                0x0a,
                &u64::MAX.to_le_bytes()
            ),
            Ordering::Less
        );
        assert_eq!(
            compare_native_binary_json(0x09, &[1], 0x0a, &[2]),
            Ordering::Less
        );
        assert_eq!(
            compare_native_binary_json(0x04, &[], 0x04, &[0]),
            Ordering::Greater
        );
        assert_eq!(
            compare_native_binary_json(0x0d, &[0xfc, 1, b'x'], 0x0d, &[0xfd, 1, b'x']),
            Ordering::Equal
        );
        assert_eq!(
            compare_native_binary_json(0x0d, &[], 0x0d, &[0, 1, b'x']),
            Ordering::Equal
        );
        assert_eq!(
            compare_native_binary_json(0x0e, &16_u64.to_le_bytes(), 0x0e, &31_u64.to_le_bytes()),
            Ordering::Equal
        );
        assert_eq!(
            compare_native_binary_json(0x0f, &16_u64.to_le_bytes(), 0x10, &32_u64.to_le_bytes()),
            Ordering::Less
        );
        assert_eq!(
            compare_native_binary_json(0x0f, &[], 0x10, &32_u64.to_le_bytes()),
            Ordering::Equal
        );
        let mut duration = (-7_i64).to_le_bytes().to_vec();
        duration.extend_from_slice(&0_u32.to_le_bytes());
        let mut other_fsp = (-7_i64).to_le_bytes().to_vec();
        other_fsp.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            compare_native_binary_json(0x11, &duration, 0x11, &other_fsp),
            Ordering::Equal
        );
    }

    #[test]
    fn native_binary_decoders_keep_duplicate_keys_and_distinct_bad_entry_policy() {
        // Two literal members in encoded order b:true,a:false. Object compare
        // sorts byte keys, while the lossless decoder keeps encoded order.
        let unsorted = [
            2, 0, 0, 0, 32, 0, 0, 0, 30, 0, 0, 0, 1, 0, 31, 0, 0, 0, 1, 0, 4, 1, 0, 0, 0, 4, 2, 0,
            0, 0, b'b', b'a',
        ];
        let mut sorted = unsorted;
        sorted[21] = 2;
        sorted[26] = 1;
        sorted[30] = b'a';
        sorted[31] = b'b';
        assert_eq!(
            compare_native_binary_json(0x01, &unsorted, 0x01, &sorted),
            Ordering::Equal
        );
        let mut duplicate = unsorted;
        duplicate[30] = b'a';
        let node = decode_native_binary_json_node(0x01, &duplicate).unwrap();
        let NativeJsonNode::Object(entries) = node else {
            panic!("expected object")
        };
        assert_eq!(entries.len(), 2);
        assert_eq!((entries[0].0.as_str(), entries[1].0.as_str()), ("a", "a"));
        let value = decode_native_binary_json_value(0x01, &duplicate).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 1);
        assert_eq!(value["a"], Value::Bool(false));
        let single = [
            1, 0, 0, 0, 20, 0, 0, 0, 19, 0, 0, 0, 1, 0, 4, 2, 0, 0, 0, b'a',
        ];
        assert_eq!(
            compare_native_binary_json(0x01, &duplicate, 0x01, &single),
            Ordering::Greater
        );
        let object_child = [
            1, 0, 0, 0, 21, 0, 0, 0, 1, 13, 0, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0,
        ];
        let mut array_child = object_child;
        array_child[8] = 3;
        assert_eq!(
            compare_native_binary_json(0x03, &object_child, 0x03, &array_child),
            Ordering::Less
        );
        // Claimed i64 starts at the end: lossless admission refuses, but the
        // original serde decoder's payload slice panics. Do not merge paths.
        let short_entry = [1, 0, 0, 0, 13, 0, 0, 0, 9, 13, 0, 0, 0];
        assert_eq!(
            decode_native_binary_json_node(0x03, &short_entry),
            Err(NativeBinaryJsonError::InvalidBinary)
        );
        assert!(
            std::panic::catch_unwind(|| decode_native_binary_json_value(0x03, &short_entry))
                .is_err()
        );
        assert_eq!(
            compare_native_binary_json(0x03, &short_entry, 0x03, &[0, 0, 0, 0, 8, 0, 0, 0]),
            Ordering::Equal
        );
    }
}

#[cfg(test)]
mod native_predicate_tests {
    use super::*;

    #[test]
    fn raw_predicates_keep_epsilon_duplicates_temporal_and_malformed_policies() {
        let integer = 1_i64.to_le_bytes();
        let nearby = 1.000000005_f64.to_le_bytes();
        assert_eq!(
            contains_native_binary_json(
                JSON_TYPE_CODE_INT64,
                &integer,
                JSON_TYPE_CODE_FLOAT64,
                &nearby
            ),
            Ok(true)
        );
        assert_eq!(
            overlaps_native_binary_json(
                JSON_TYPE_CODE_INT64,
                &integer,
                JSON_TYPE_CODE_FLOAT64,
                &nearby
            ),
            Ok(true)
        );
        let time = 0x1234_5678_0000_0000_u64.to_le_bytes();
        let flags = 0x1234_5678_0000_000f_u64.to_le_bytes();
        assert_eq!(
            contains_native_binary_json(
                JSON_TYPE_CODE_DATETIME,
                &time,
                JSON_TYPE_CODE_DATETIME,
                &flags
            ),
            Ok(true)
        );
        assert_eq!(
            overlaps_native_binary_json(JSON_TYPE_CODE_DATE, &time, JSON_TYPE_CODE_DATETIME, &time),
            Ok(false)
        );
        assert_eq!(
            contains_native_binary_json(JSON_TYPE_CODE_OPAQUE, &[], JSON_TYPE_CODE_OPAQUE, &[]),
            Err(NativeBinaryJsonError::InvalidBinary)
        );
        assert_eq!(
            member_of_native_binary_json(JSON_TYPE_CODE_OPAQUE, &[], JSON_TYPE_CODE_OPAQUE, &[]),
            Ok(true)
        );
        let malformed_array = [
            2,
            0,
            0,
            0,
            18,
            0,
            0,
            0,
            JSON_TYPE_CODE_LITERAL,
            JSON_LITERAL_TRUE,
            0,
            0,
            0,
            0xff,
            0,
            0,
            0,
            0,
        ];
        // element_count used full decoding, so even an earlier match cannot
        // bypass a malformed later array element.
        assert_eq!(
            member_of_native_binary_json(
                JSON_TYPE_CODE_LITERAL,
                &[JSON_LITERAL_TRUE],
                JSON_TYPE_CODE_ARRAY,
                &malformed_array
            ),
            Err(NativeBinaryJsonError::InvalidBinary)
        );
        let duplicate_object = [
            2,
            0,
            0,
            0,
            32,
            0,
            0,
            0,
            30,
            0,
            0,
            0,
            1,
            0,
            31,
            0,
            0,
            0,
            1,
            0,
            JSON_TYPE_CODE_LITERAL,
            JSON_LITERAL_TRUE,
            0,
            0,
            0,
            JSON_TYPE_CODE_LITERAL,
            JSON_LITERAL_FALSE,
            0,
            0,
            0,
            b'a',
            b'a',
        ];
        let last_key_value = [
            1,
            0,
            0,
            0,
            20,
            0,
            0,
            0,
            19,
            0,
            0,
            0,
            1,
            0,
            JSON_TYPE_CODE_LITERAL,
            JSON_LITERAL_FALSE,
            0,
            0,
            0,
            b'a',
        ];
        // Lossless SDK containment and overlap use the FIRST matching key,
        // unlike the serde preparation's last-key-wins value map.
        assert_eq!(
            contains_native_binary_json(
                JSON_TYPE_CODE_OBJECT,
                &duplicate_object,
                JSON_TYPE_CODE_OBJECT,
                &last_key_value
            ),
            Ok(false)
        );
        assert_eq!(
            overlaps_native_binary_json(
                JSON_TYPE_CODE_OBJECT,
                &duplicate_object,
                JSON_TYPE_CODE_OBJECT,
                &last_key_value
            ),
            Ok(false)
        );
        assert_eq!(
            member_of_native_binary_json(
                JSON_TYPE_CODE_OBJECT,
                &last_key_value,
                JSON_TYPE_CODE_OBJECT,
                &duplicate_object
            ),
            Ok(false)
        );
        let mut object_array = vec![1, 0, 0, 0, 45, 0, 0, 0, JSON_TYPE_CODE_OBJECT, 13, 0, 0, 0];
        object_array.extend_from_slice(&duplicate_object);
        assert_eq!(
            member_of_native_binary_json(
                JSON_TYPE_CODE_OBJECT,
                &duplicate_object,
                JSON_TYPE_CODE_ARRAY,
                &object_array
            ),
            Ok(true)
        );
        // A malformed equal-rank candidate retains the raw comparator's equal
        // fallback; only the document's array is unconditionally decoded.
        assert_eq!(
            member_of_native_binary_json(
                JSON_TYPE_CODE_OBJECT,
                &[],
                JSON_TYPE_CODE_ARRAY,
                &object_array
            ),
            Ok(true)
        );
    }
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
