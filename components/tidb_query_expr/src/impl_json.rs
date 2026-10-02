// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::collections::BTreeMap;

use serde::de::IgnoredAny;
use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::{
    EvalType,
    codec::{data_type::*, mysql::json::*},
};

// Native introspection keeps its SQL signature and parse policy separate from
// wire Json evaluation. Only the datatype's shared parser/type/depth primitives
// compute these answers; the native frontend does not pre-parse the document.
fn native_json_report_parse(
    bytes: &[u8],
) -> std::result::Result<serde_json::Value, NativeJsonError> {
    let text = std::str::from_utf8(bytes).map_err(|_| NativeJsonError::InvalidText)?;
    parse_native_json_document(text)
}

fn native_json_report_error(error: NativeJsonError) -> Bytes {
    vec![match error {
        NativeJsonError::EmptyText => 1,
        NativeJsonError::InvalidText | NativeJsonError::InvalidBinary => 2,
    }]
}

// Closed transport: tag 0 carries a type name or i64 LE8 value, selected by the
// operation; exact one-byte tags 1/2 carry the computed parse error. Allocation
// failures remain transport errors rather than fabricated JSON dispositions.
fn native_json_report_value(payload: &[u8]) -> Result<Bytes> {
    let length = payload
        .len()
        .checked_add(1)
        .ok_or_else(|| other_err!("Native JSON report envelope length overflow"))?;
    let mut envelope = Vec::new();
    envelope.try_reserve_exact(length).map_err(|source| {
        other_err!("Unable to allocate native JSON report envelope: {}", source)
    })?;
    envelope.push(0);
    envelope.extend_from_slice(payload);
    Ok(envelope)
}

#[rpn_fn(nullable)]
fn json_valid_text_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    Ok(arg.map(|bytes| i64::from(native_json_report_parse(bytes).is_ok())))
}

#[rpn_fn(nullable)]
fn json_valid_binary_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    // The SQL JSON signature accepts every typed JSON value, without validating
    // even its type-code/payload pair. NULL still executes this nullable body.
    Ok(arg.map(|_| 1))
}

#[rpn_fn]
fn json_valid_other_native() -> Result<Option<Int>> {
    // The SQL Others signature ignores its payload, not a synthetic 0/1 input.
    Ok(Some(0))
}

#[rpn_fn(nullable)]
fn json_type_text_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    let Some(bytes) = arg else {
        return Ok(None);
    };
    match native_json_report_parse(bytes) {
        Ok(document) => native_json_report_value(native_json_type_name(&document)).map(Some),
        Err(error) => Ok(Some(native_json_report_error(error))),
    }
}

#[rpn_fn(nullable)]
fn json_type_binary_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    let Some(bytes) = arg else {
        return Ok(None);
    };
    let (&type_code, payload) = bytes
        .split_first()
        .ok_or_else(|| other_err!("Native JSON_TYPE transport is missing its SQL type code"))?;
    match native_binary_json_type_name(type_code, payload) {
        Ok(name) => native_json_report_value(name).map(Some),
        Err(error) => Ok(Some(native_json_report_error(error))),
    }
}

#[rpn_fn(nullable)]
fn json_depth_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    let Some(bytes) = arg else {
        return Ok(None);
    };
    match native_json_report_parse(bytes) {
        Ok(document) => {
            // A real depth-helper error is not a JSON text-error disposition.
            let depth = native_json_depth(&document)?;
            native_json_report_value(&depth.to_le_bytes()).map(Some)
        }
        Err(error) => Ok(Some(native_json_report_error(error))),
    }
}

#[rpn_fn(nullable)]
fn json_storage_free_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    let Some(bytes) = arg else {
        return Ok(None);
    };
    // The worker must parse the actual document before reporting zero.
    match native_json_report_parse(bytes) {
        Ok(_) => native_json_report_value(&0i64.to_le_bytes()).map(Some),
        Err(error) => Ok(Some(native_json_report_error(error))),
    }
}

#[rpn_fn(nullable)]
fn json_storage_size_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    let Some(bytes) = arg else {
        return Ok(None);
    };
    match native_json_report_parse(bytes) {
        Ok(document) => {
            // The shared size includes the root byte; retain the native cast.
            let size = native_json_storage_size(&document) as i64;
            native_json_report_value(&size.to_le_bytes()).map(Some)
        }
        Err(error) => Ok(Some(native_json_report_error(error))),
    }
}

#[rpn_fn(nullable)]
fn json_quote_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    let Some(bytes) = arg else {
        return Ok(None);
    };
    let text = std::str::from_utf8(bytes).map_err(|source| {
        other_err!("Native JSON_QUOTE transport is not valid UTF-8: {}", source)
    })?;
    native_quote(text).map(Some)
}

// These packets contain prepared serde values and original UTF-8 paths, not
// binary JSON and not a frontend-computed predicate or path-match result.
fn json_serde_native_value(bytes: &[u8]) -> Result<serde_json::Value> {
    serde_json::from_slice(bytes)
        .map_err(|error| other_err!("Invalid prepared serde JSON transport: {}", error))
}

fn json_serde_native_path(bytes: &[u8], allow_multiple: bool) -> Result<crate::NativeJsonPath> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| other_err!("Invalid native JSON path UTF-8 transport: {}", error))?;
    let path = crate::parse_native_json_path(text).map_err(|error| {
        other_err!(
            "Invalid native JSON path transport at rune {}",
            error.position
        )
    })?;
    if !allow_multiple && path.could_match_multiple {
        return Err(other_err!(
            "Native JSON path transport must select a single value"
        ));
    }
    Ok(path)
}

/// Validates actual prepared values/path transport for closed predicate calls.
/// Callers separately validate each fixed role's arity and non-NULL presence.
/// Only the path-existence profile permits multiple-match paths.
pub fn json_serde_native_args_valid(
    first: &[u8],
    second: Option<&[u8]>,
    path: Option<(&[u8], bool)>,
) -> bool {
    json_serde_native_value(first).is_ok()
        && second.is_none_or(|bytes| json_serde_native_value(bytes).is_ok())
        && path.is_none_or(|(bytes, multiple)| json_serde_native_path(bytes, multiple).is_ok())
}

/// Legacy member-of compares original binary payloads. Array documents must
/// fully decode, exactly as native element_count/array_get did before lookup;
/// non-array documents and the target retain the raw comparator's fallback.
pub fn json_member_binary_legacy_args_valid(target: &[u8], document: &[u8]) -> bool {
    let (Some(_), Some((&document_type, document_raw))) =
        (target.split_first(), document.split_first())
    else {
        return false;
    };
    document_type != JsonType::Array as u8
        || decode_native_binary_json_node(document_type, document_raw).is_ok()
}

#[rpn_fn]
fn json_contains_serde_native(document: BytesRef, candidate: BytesRef) -> Result<Option<Int>> {
    let document = json_serde_native_value(document)?;
    let candidate = json_serde_native_value(candidate)?;
    Ok(Some(Int::from(crate::native_json_contains(
        &document, &candidate,
    ))))
}

#[rpn_fn]
fn json_contains_path_serde_native(
    document: BytesRef,
    candidate: BytesRef,
    path: BytesRef,
) -> Result<Option<Int>> {
    let document = json_serde_native_value(document)?;
    let candidate = json_serde_native_value(candidate)?;
    let path = json_serde_native_path(path, false)?;
    Ok(crate::native_json_extract(&document, &[path])
        .map(|selected| Int::from(crate::native_json_contains(&selected, &candidate))))
}

#[rpn_fn]
fn json_overlaps_serde_native(left: BytesRef, right: BytesRef) -> Result<Option<Int>> {
    let left = json_serde_native_value(left)?;
    let right = json_serde_native_value(right)?;
    Ok(Some(Int::from(crate::native_json_overlaps(&left, &right))))
}

#[rpn_fn]
fn json_member_of_serde_native(candidate: BytesRef, document: BytesRef) -> Result<Option<Int>> {
    let candidate = json_serde_native_value(candidate)?;
    let document = json_serde_native_value(document)?;
    Ok(Some(Int::from(crate::native_json_member_of(
        &candidate, &document,
    ))))
}

#[rpn_fn]
fn json_length_serde_native(document: BytesRef) -> Result<Option<Int>> {
    let document = json_serde_native_value(document)?;
    Ok(Some(crate::native_json_length(&document)))
}

#[rpn_fn]
fn json_length_path_serde_native(document: BytesRef, path: BytesRef) -> Result<Option<Int>> {
    let document = json_serde_native_value(document)?;
    let path = json_serde_native_path(path, false)?;
    Ok(crate::native_json_extract(&document, &[path])
        .map(|selected| crate::native_json_length(&selected)))
}

#[rpn_fn]
fn json_path_exists_serde_native(document: BytesRef, path: BytesRef) -> Result<Option<Int>> {
    let document = json_serde_native_value(document)?;
    let path = json_serde_native_path(path, true)?;
    Ok(Some(Int::from(
        crate::native_json_extract(&document, &[path]).is_some(),
    )))
}

#[rpn_fn]
fn json_member_of_binary_legacy(candidate: BytesRef, document: BytesRef) -> Result<Option<Int>> {
    let (&candidate_type, candidate_raw) = candidate
        .split_first()
        .ok_or_else(|| other_err!("Legacy JSON member target transport has no type byte"))?;
    let (&document_type, document_raw) = document
        .split_first()
        .ok_or_else(|| other_err!("Legacy JSON member document transport has no type byte"))?;
    member_of_native_binary_json(candidate_type, candidate_raw, document_type, document_raw)
        .map(|value| Some(Int::from(value)))
        .map_err(|error| other_err!("Invalid legacy JSON member document transport: {:?}", error))
}

#[rpn_fn(nullable)]
fn json_predicate_null_native(witness: Option<&Int>) -> Result<Option<Int>> {
    if witness.is_some() {
        return Err(other_err!(
            "Native JSON predicate NULL witness contains a value"
        ));
    }
    Ok(None)
}

#[rpn_fn]
fn json_predicate_missing_legacy() -> Result<Option<Int>> {
    Ok(None)
}

fn json_serde_packet_word(remaining: &mut &[u8]) -> Result<usize> {
    if remaining.len() < 8 {
        return Err(other_err!("Truncated native JSON packet word"));
    }
    let (word, tail) = remaining.split_at(8);
    *remaining = tail;
    usize::try_from(u64::from_le_bytes(word.try_into().unwrap()))
        .map_err(|_| other_err!("Native JSON packet extent does not fit usize"))
}

fn json_serde_packet_bytes<'a>(remaining: &mut &'a [u8]) -> Result<&'a [u8]> {
    let length = json_serde_packet_word(remaining)?;
    if length > remaining.len() {
        return Err(other_err!("Truncated native JSON packet value"));
    }
    let (value, tail) = remaining.split_at(length);
    *remaining = tail;
    Ok(value)
}

// One bounded walker serves admission and execution. The fixed operation, not
// any packet opcode, determines whether each entry also has an actual key.
// The count is checked against physical framing before iteration or allocation.
fn walk_json_serde_packet(
    packet: &[u8],
    object_pairs: bool,
    mut consume: impl FnMut(Option<&str>, serde_json::Value) -> Result<()>,
) -> Result<()> {
    let mut remaining = packet;
    let count = json_serde_packet_word(&mut remaining)?;
    let minimum_entry_bytes = if object_pairs { 16 } else { 8 };
    if count > remaining.len() / minimum_entry_bytes {
        return Err(other_err!("Native JSON packet count exceeds its framing"));
    }
    for _ in 0..count {
        let key = if object_pairs {
            Some(
                std::str::from_utf8(json_serde_packet_bytes(&mut remaining)?).map_err(|error| {
                    other_err!("Invalid native JSON object key UTF-8: {}", error)
                })?,
            )
        } else {
            None
        };
        let value = json_serde_native_value(json_serde_packet_bytes(&mut remaining)?)?;
        consume(key, value)?;
    }
    if !remaining.is_empty() {
        return Err(other_err!("Native JSON packet has trailing bytes"));
    }
    Ok(())
}

/// An actual argument-list packet, including count zero, is not a JSON array
/// prepared by the caller. Only the worker constructs the resulting array.
pub fn json_array_serde_args_valid(packet: &[u8]) -> bool {
    walk_json_serde_packet(packet, false, |_, _| Ok(())).is_ok()
}

/// Ordered actual key/value pairs; duplicates remain in order for worker-side
/// last-wins construction, rather than a caller-precomputed JSON object.
pub fn json_object_serde_args_valid(packet: &[u8]) -> bool {
    walk_json_serde_packet(packet, true, |_, _| Ok(())).is_ok()
}

#[rpn_fn]
fn json_array_serde_native(packet: BytesRef) -> Result<Option<Bytes>> {
    let mut values = Vec::new();
    walk_json_serde_packet(packet, false, |_, value| {
        values.try_reserve(1).map_err(|error| {
            other_err!("Unable to allocate native JSON array inputs: {}", error)
        })?;
        values.push(value);
        Ok(())
    })?;
    let value = crate::native_json_array(values);
    Ok(Some(crate::native_json_format(&value).into_bytes()))
}

#[rpn_fn]
fn json_object_serde_native(packet: BytesRef) -> Result<Option<Bytes>> {
    let mut pairs = Vec::new();
    walk_json_serde_packet(packet, true, |key, value| {
        let key = key.ok_or_else(|| other_err!("Native JSON object packet lacks a key"))?;
        let mut owned_key = String::new();
        owned_key
            .try_reserve_exact(key.len())
            .map_err(|error| other_err!("Unable to allocate native JSON object key: {}", error))?;
        owned_key.push_str(key);
        pairs.try_reserve(1).map_err(|error| {
            other_err!("Unable to allocate native JSON object inputs: {}", error)
        })?;
        pairs.push((owned_key, value));
        Ok(())
    })?;
    let value = crate::native_json_object(pairs);
    Ok(Some(crate::native_json_format(&value).into_bytes()))
}

#[rpn_fn]
fn json_keys_serde_native(document: BytesRef) -> Result<Option<Bytes>> {
    let document = json_serde_native_value(document)?;
    Ok(crate::native_json_keys(&document)
        .map(|value| crate::native_json_format(&value).into_bytes()))
}

#[rpn_fn]
fn json_keys_path_serde_native(document: BytesRef, path: BytesRef) -> Result<Option<Bytes>> {
    let document = json_serde_native_value(document)?;
    let path = json_serde_native_path(path, false)?;
    Ok(crate::native_json_extract(&document, &[path])
        .and_then(|value| crate::native_json_keys(&value))
        .map(|value| crate::native_json_format(&value).into_bytes()))
}

#[rpn_fn]
fn json_pretty_serde_native(document: BytesRef) -> Result<Option<Bytes>> {
    let document = json_serde_native_value(document)?;
    Ok(Some(crate::native_json_pretty(&document).into_bytes()))
}

#[rpn_fn(nullable)]
fn json_output_null_native(witness: Option<&Int>) -> Result<Option<Bytes>> {
    if witness.is_some() {
        return Err(other_err!(
            "Native JSON output NULL witness contains a value"
        ));
    }
    Ok(None)
}

#[rpn_fn]
#[inline]
fn json_depth(arg: JsonRef) -> Result<Option<i64>> {
    Ok(Some(arg.depth()?))
}

#[rpn_fn(writer)]
#[inline]
fn json_type(arg: JsonRef, writer: BytesWriter) -> Result<BytesGuard> {
    Ok(writer.write(Some(Bytes::from(arg.json_type()))))
}

#[rpn_fn(nullable, raw_varg, min_args = 2, extra_validator = json_modify_validator)]
#[inline]
fn json_set(args: &[ScalarValueRef]) -> Result<Option<Json>> {
    json_modify(args, ModifyType::Set)
}

#[rpn_fn(nullable, raw_varg, min_args = 2, extra_validator = json_modify_validator)]
#[inline]
fn json_insert(args: &[ScalarValueRef]) -> Result<Option<Json>> {
    json_modify(args, ModifyType::Insert)
}

#[rpn_fn(nullable, raw_varg, min_args = 2, extra_validator = json_modify_validator)]
#[inline]
fn json_replace(args: &[ScalarValueRef]) -> Result<Option<Json>> {
    json_modify(args, ModifyType::Replace)
}

#[inline]
fn json_modify(args: &[ScalarValueRef], mt: ModifyType) -> Result<Option<Json>> {
    assert!(args.len() >= 2);
    // base Json argument
    let base: Option<JsonRef> = args[0].as_json();
    let base = base.map_or(Json::none(), |json| Ok(json.to_owned()))?;

    let buf_size = args.len() / 2;

    let mut path_expr_list = Vec::with_capacity(buf_size);
    let mut values = Vec::with_capacity(buf_size);

    for chunk in args[1..].chunks(2) {
        let path: Option<BytesRef> = chunk[0].as_bytes();
        let value: Option<JsonRef> = chunk[1].as_json();

        path_expr_list.push(try_opt!(parse_json_path(path)));

        let value = value
            .as_ref()
            .map_or(Json::none(), |json| Ok(json.to_owned()))?;
        values.push(value);
    }
    Ok(Some(base.as_ref().modify(&path_expr_list, values, mt)?))
}

#[rpn_fn(nullable, raw_varg, min_args = 2, extra_validator = json_modify_validator)]
#[inline]
fn json_array_append(args: &[ScalarValueRef]) -> Result<Option<Json>> {
    assert!(args.len() >= 2);
    // Returns None if Base is None
    if args[0].to_owned().is_none() {
        return Ok(None);
    }
    // base Json argument
    let base: Option<JsonRef> = args[0].as_json();
    let mut base = base.map_or(Json::none(), |json| Ok(json.to_owned()))?;

    for chunk in args[1..].chunks(2) {
        let path: Option<BytesRef> = chunk[0].as_bytes();
        let value: Option<JsonRef> = chunk[1].as_json();

        let value = value
            .as_ref()
            .map_or(Json::none(), |json| Ok(json.to_owned()))?;
        // extract the element from the path, then merge the value into the element
        // 1. extrace the element from the path
        let tmp_path_expr_list = vec![try_opt!(parse_json_path(path))];
        let element: Option<Json> = base.as_ref().extract(&tmp_path_expr_list)?;
        // 2. merge the value into the element
        if let Some(elem) = element {
            // if both elem and value are json object, wrap elem into a vector
            if elem.get_type() == JsonType::Object && value.get_type() == JsonType::Object {
                let array_json: Json = Json::from_array(vec![elem.clone()])?;
                let tmp_values = vec![array_json.as_ref(), value.as_ref()];
                let tmp_value = Json::merge(tmp_values)?;
                base =
                    base.as_ref()
                        .modify(&tmp_path_expr_list, vec![tmp_value], ModifyType::Set)?;
            } else {
                let tmp_values = vec![elem.as_ref(), value.as_ref()];
                let tmp_value = Json::merge(tmp_values)?;
                base =
                    base.as_ref()
                        .modify(&tmp_path_expr_list, vec![tmp_value], ModifyType::Set)?;
            }
        }
    }
    Ok(Some(base))
}

/// validate the arguments are `(Option<JsonRef>, &[(Option<Bytes>,
/// Option<Json>)])`
fn json_modify_validator(expr: &crate::types::function::CallShape) -> Result<()> {
    let children = expr.args();
    assert!(children.len() >= 2);
    if children.len() % 2 != 1 {
        return Err(other_err!(
            "Incorrect parameter count in the call to native function 'JSON_OBJECT'"
        ));
    }
    super::function::validate_field_type(children[0].field_type(), EvalType::Json)?;
    for chunk in children[1..].chunks(2) {
        super::function::validate_field_type(chunk[0].field_type(), EvalType::Bytes)?;
        super::function::validate_field_type(chunk[1].field_type(), EvalType::Json)?;
    }
    Ok(())
}

#[rpn_fn(nullable, varg)]
#[inline]
fn json_array(args: &[Option<JsonRef>]) -> Result<Option<Json>> {
    let mut jsons = vec![];
    for arg in args {
        match arg {
            None => jsons.push(Json::none()?),
            Some(j) => jsons.push((*j).to_owned()),
        }
    }
    Ok(Some(Json::from_array(jsons)?))
}

fn json_object_validator(expr: &crate::types::function::CallShape) -> Result<()> {
    let chunks = expr.args();
    if chunks.len() % 2 == 1 {
        return Err(other_err!(
            "Incorrect parameter count in the call to native function 'JSON_OBJECT'"
        ));
    }
    for chunk in chunks.chunks(2) {
        super::function::validate_field_type(chunk[0].field_type(), EvalType::Bytes)?;
        super::function::validate_field_type(chunk[1].field_type(), EvalType::Json)?;
    }
    Ok(())
}

/// Required args like `&[(Option<&Byte>, Option<JsonRef>)]`.
#[rpn_fn(nullable, raw_varg, extra_validator = json_object_validator)]
#[inline]
fn json_object(raw_args: &[ScalarValueRef]) -> Result<Option<Json>> {
    let mut pairs = BTreeMap::new();
    for chunk in raw_args.chunks(2) {
        assert_eq!(chunk.len(), 2);
        let key: Option<BytesRef> = chunk[0].as_bytes();
        if key.is_none() {
            return Err(other_err!(
                "Data truncation: JSON documents may not contain NULL member names."
            ));
        }
        let key = String::from_utf8(key.unwrap().to_owned())
            .map_err(tidb_query_datatype::codec::Error::from)?;

        let value: Option<JsonRef> = chunk[1].as_json();
        let value = match value {
            None => Json::none()?,
            Some(v) => v.to_owned(),
        };

        pairs.insert(key, value);
    }
    Ok(Some(Json::from_object(pairs)?))
}

// According to mysql 5.7,
// arguments of json_merge should not be less than 2.
#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn json_merge(args: &[Option<JsonRef>]) -> Result<Option<Json>> {
    // min_args = 2, so it's ok to call args[0]
    if args[0].is_none() {
        return Ok(None);
    }
    let mut jsons: Vec<JsonRef> = vec![];
    let json_none = Json::none()?;
    for arg in args {
        match arg {
            None => jsons.push(json_none.as_ref()),
            Some(j) => jsons.push(*j),
        }
    }
    Ok(Some(Json::merge(jsons)?))
}

// `json_merge_patch` is the implementation for JSON_MERGE_PATCH in mysql
// <https://dev.mysql.com/doc/refman/8.3/en/json-modification-functions.html#function_json-merge-patch>
//
// The json_merge_patch rules are listed as following:
// 1. If the first argument is not an object, the result of the merge is the
//    same as if an empty object had been merged with the second argument.
// 2. If the second argument is not an object, the result of the merge is the
//    second argument.
// 3. If both arguments are objects, the result of the merge is an object with
//    the following members: 3.1. All members of the first object which do not
//    have a corresponding member with the same key in the second object. 3.2.
//    All members of the second object which do not have a corresponding key in
//    the first object, and whose value is not the JSON null literal. 3.3. All
//    members with a key that exists in both the first and the second object,
//    and whose value in the second object is not the JSON null literal. The
//    values of these members are the results of recursively merging the value
//    in the first object with the value in the second object.
// See `MergePatchBinaryJSON()` in TiDB
// `pkg/types/json_binary_functions.go`
// arguments of json_merge_patch should not be less than 2.
#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn json_merge_patch(args: &[Option<JsonRef>]) -> Result<Option<Json>> {
    let mut jsons: Vec<Option<JsonRef>> = vec![];
    let mut index = 0;
    // according to the implements of RFC7396
    // when the last item is not object
    // we can return the last item directly
    for i in (0..=args.len() - 1).rev() {
        if args[i].is_none() || args[i].unwrap().get_type() != JsonType::Object {
            index = i;
            break;
        }
    }

    if args[index].is_none() {
        return Ok(None);
    }

    jsons.extend(&args[index..]);
    let mut target = jsons[0].unwrap().to_owned();

    if jsons.len() > 1 {
        for i in 1..jsons.len() {
            target = Json::merge_patch(target.as_ref(), jsons[i].unwrap())?;
        }
    }
    Ok(Some(target.to_owned()))
}

#[rpn_fn(writer)]
#[inline]
fn json_quote(input: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    Ok(writer.write(quote(input)?))
}

#[derive(Clone, Copy)]
enum JsonQuotePolicy {
    Wire,
    Native,
}

fn quote(bytes: BytesRef) -> Result<Option<Bytes>> {
    let mut result = Vec::with_capacity(bytes.len() * 2 + 2);
    quote_with_policy(bytes, JsonQuotePolicy::Wire, &mut result);
    Ok(Some(result))
}

fn native_quote(text: &str) -> Result<Bytes> {
    // Reserve a checked worst-case byte count, not serde's original allocation
    // strategy or a guarantee about whole-call allocation peaks/OOM behavior.
    let capacity = text
        .len()
        .checked_mul(6)
        .and_then(|length| length.checked_add(2))
        .ok_or_else(|| other_err!("Native JSON_QUOTE capacity overflow"))?;
    let mut result = Vec::new();
    result
        .try_reserve_exact(capacity)
        .map_err(|source| other_err!("Unable to allocate native JSON_QUOTE output: {}", source))?;
    quote_with_policy(text.as_bytes(), JsonQuotePolicy::Native, &mut result);
    Ok(result)
}

fn quote_with_policy(bytes: BytesRef, policy: JsonQuotePolicy, result: &mut Bytes) {
    const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";
    result.push(b'"');
    for &byte in bytes {
        let escaped = match byte {
            b'"' | b'\\' => Some(byte),
            b'\x07' if matches!(policy, JsonQuotePolicy::Wire) => Some(b'a'),
            b'\x0b' if matches!(policy, JsonQuotePolicy::Wire) => Some(b'v'),
            b'\x08' => Some(b'b'),
            b'\x0c' => Some(b'f'),
            b'\t' => Some(b't'),
            b'\n' => Some(b'n'),
            b'\r' => Some(b'r'),
            b'\x00'..=b'\x1f' if matches!(policy, JsonQuotePolicy::Native) => {
                result.extend_from_slice(b"\\u00");
                result.push(LOWER_HEX[(byte >> 4) as usize]);
                result.push(LOWER_HEX[(byte & 0x0f) as usize]);
                continue;
            }
            _ => None,
        };
        if let Some(escaped) = escaped {
            result.push(b'\\');
            result.push(escaped);
        } else {
            result.push(byte);
        }
    }
    result.push(b'"');
}

#[rpn_fn(nullable, raw_varg, min_args = 1, max_args = 1)]
#[inline]
fn json_valid(args: &[ScalarValueRef]) -> Result<Option<Int>> {
    assert_eq!(args.len(), 1);
    let received_et = args[0].eval_type();
    let r = match args[0].to_owned().is_none() {
        true => None,
        _ => match received_et {
            EvalType::Json => args[0].as_json().and(Some(1)),
            EvalType::Bytes => match args[0].as_bytes() {
                Some(p) => {
                    let tmp_str =
                        std::str::from_utf8(p).map_err(tidb_query_datatype::codec::Error::from)?;
                    let json: serde_json::error::Result<Json> = serde_json::from_str(tmp_str);
                    Some(json.is_ok() as Int)
                }
                _ => Some(0),
            },
            _ => Some(0),
        },
    };

    Ok(r)
}

#[rpn_fn]
#[inline]
fn json_unquote(arg: BytesRef) -> Result<Option<Bytes>> {
    let tmp_str = std::str::from_utf8(arg)?;
    let first_char = tmp_str.chars().next();
    let last_char = tmp_str.chars().last();
    if tmp_str.len() >= 2 && first_char == Some('"') && last_char == Some('"') {
        let _: IgnoredAny = serde_json::from_str(tmp_str)?;
    }
    Ok(Some(Bytes::from(self::unquote_string(tmp_str)?)))
}

// Args should be like `(Option<JsonRef> , &[Option<BytesRef>])`.
fn json_with_paths_validator(expr: &crate::types::function::CallShape) -> Result<()> {
    assert!(expr.args().len() >= 2);
    // args should be like `Option<JsonRef> , &[Option<BytesRef>]`.
    valid_paths(expr)
}

fn valid_paths(expr: &crate::types::function::CallShape) -> Result<()> {
    let children = expr.args();
    super::function::validate_field_type(children[0].field_type(), EvalType::Json)?;
    for child in children.iter().skip(1) {
        super::function::validate_field_type(child.field_type(), EvalType::Bytes)?;
    }
    Ok(())
}

fn unquote_string(s: &str) -> Result<String> {
    let first_char = s.chars().next();
    let last_char = s.chars().last();
    if s.len() >= 2 && first_char == Some('"') && last_char == Some('"') {
        Ok(json_unquote::unquote_string(&s[1..s.len() - 1])?)
    } else {
        Ok(String::from(s))
    }
}

#[rpn_fn(nullable, raw_varg, min_args = 2, extra_validator = json_with_paths_validator)]
#[inline]
fn json_extract(args: &[ScalarValueRef]) -> Result<Option<Json>> {
    assert!(args.len() >= 2);
    let j: Option<JsonRef> = args[0].as_json();
    let j = match j {
        None => return Ok(None),
        Some(j) => j.to_owned(),
    };

    let path_expr_list = try_opt!(parse_json_path_list(&args[1..]));

    Ok(j.as_ref().extract(&path_expr_list)?)
}

// Args should be like `(Option<JsonRef> , &[Option<BytesRef>])`.
fn json_with_path_validator(expr: &crate::types::function::CallShape) -> Result<()> {
    assert!(expr.args().len() == 2 || expr.args().len() == 1);
    valid_paths(expr)
}

#[rpn_fn(nullable, raw_varg,min_args= 1, max_args = 2, extra_validator = json_with_path_validator)]
#[inline]
fn json_keys(args: &[ScalarValueRef]) -> Result<Option<Json>> {
    assert!(!args.is_empty() && args.len() <= 2);
    if let Some(j) = args[0].as_json() {
        if let Some(list) = parse_json_path_list(&args[1..])? {
            return Ok(j.keys(&list)?);
        }
    }
    Ok(None)
}

#[rpn_fn(nullable, raw_varg,min_args= 1, max_args = 2, extra_validator = json_with_path_validator)]
#[inline]
fn json_length(args: &[ScalarValueRef]) -> Result<Option<Int>> {
    assert!(!args.is_empty() && args.len() <= 2);
    let j: Option<JsonRef> = args[0].as_json();
    let j = match j {
        None => return Ok(None),
        Some(j) => j.to_owned(),
    };
    Ok(match parse_json_path_list(&args[1..])? {
        Some(path_expr_list) => j.as_ref().json_length(&path_expr_list)?,
        None => None,
    })
}

// Args should be like `(Option<JsonRef> , Option<JsonRef>,
// &[Option<BytesRef>])`. or `(Option<JsonRef> , Option<JsonRef>)`
fn json_contains_validator(expr: &crate::types::function::CallShape) -> Result<()> {
    assert!(expr.args().len() == 2 || expr.args().len() == 3);
    let children = expr.args();
    super::function::validate_field_type(children[0].field_type(), EvalType::Json)?;
    super::function::validate_field_type(children[1].field_type(), EvalType::Json)?;
    if expr.args().len() == 3 {
        super::function::validate_field_type(children[2].field_type(), EvalType::Bytes)?;
    }
    Ok(())
}

#[rpn_fn(nullable, raw_varg,min_args= 2, max_args = 3, extra_validator = json_contains_validator)]
#[inline]
fn json_contains(args: &[ScalarValueRef]) -> Result<Option<i64>> {
    assert!(args.len() == 2 || args.len() == 3);
    let j: Option<JsonRef> = args[0].as_json();
    let mut j = match j {
        None => return Ok(None),
        Some(j) => j.to_owned(),
    };
    let target: Option<JsonRef> = args[1].as_json();
    let target = match target {
        None => return Ok(None),
        Some(target) => target,
    };

    if args.len() == 3 {
        match parse_json_path_list(&args[2..])? {
            Some(path_expr_list) => {
                if path_expr_list.len() == 1 && path_expr_list[0].contains_any_asterisk() {
                    return Ok(None);
                }
                match j.as_ref().extract(&path_expr_list)? {
                    Some(json) => {
                        j = json;
                    }
                    _ => return Ok(None),
                }
            }
            None => return Ok(None),
        };
    }
    Ok(Some(j.as_ref().json_contains(target)? as i64))
}

// Args should be like `(Option<JsonRef> , Option<JsonRef>)`
fn member_of_validator(expr: &crate::types::function::CallShape) -> Result<()> {
    assert!(expr.args().len() == 2);
    let children = expr.args();
    super::function::validate_field_type(children[0].field_type(), EvalType::Json)?;
    super::function::validate_field_type(children[1].field_type(), EvalType::Json)?;
    Ok(())
}

#[rpn_fn(nullable, raw_varg,min_args= 2, max_args = 2, extra_validator = member_of_validator)]
#[inline]
fn member_of(args: &[ScalarValueRef]) -> Result<Option<i64>> {
    assert!(args.len() == 2);
    let value: Option<JsonRef> = args[0].as_json();
    let value = match value {
        None => return Ok(None),
        Some(value) => value.to_owned(),
    };

    let json_array: Option<JsonRef> = args[1].as_json();
    let json_array = match json_array {
        None => return Ok(None),
        Some(json_array) => json_array,
    };

    Ok(Some(value.as_ref().member_of(json_array)? as i64))
}

#[rpn_fn(nullable, raw_varg, min_args = 2, extra_validator = json_with_paths_validator)]
#[inline]
fn json_remove(args: &[ScalarValueRef]) -> Result<Option<Json>> {
    assert!(args.len() >= 2);
    let j: Option<JsonRef> = args[0].as_json();
    let j = match j {
        None => return Ok(None),
        Some(j) => j.to_owned(),
    };

    let path_expr_list = try_opt!(parse_json_path_list(&args[1..]));

    Ok(Some(j.as_ref().remove(&path_expr_list)?))
}

fn parse_json_path_list(args: &[ScalarValueRef]) -> Result<Option<Vec<PathExpression>>> {
    let mut path_expr_list = Vec::with_capacity(args.len());
    for arg in args {
        let json_path: Option<BytesRef> = arg.as_bytes();

        path_expr_list.push(try_opt!(parse_json_path(json_path)));
    }
    Ok(Some(path_expr_list))
}

#[inline]
fn parse_json_path(path: Option<BytesRef>) -> Result<Option<PathExpression>> {
    let json_path = match path {
        None => return Ok(None),
        Some(p) => std::str::from_utf8(p).map_err(tidb_query_datatype::codec::Error::from),
    }?;

    Ok(Some(parse_json_path_expr(json_path)?))
}

#[cfg(test)]
mod native_json_output_tests {
    use super::*;

    fn array_packet(values: &[&[u8]]) -> Vec<u8> {
        let mut packet = (values.len() as u64).to_le_bytes().to_vec();
        for value in values {
            packet.extend_from_slice(&(value.len() as u64).to_le_bytes());
            packet.extend_from_slice(value);
        }
        packet
    }

    fn object_packet(pairs: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut packet = (pairs.len() as u64).to_le_bytes().to_vec();
        for (key, value) in pairs {
            packet.extend_from_slice(&(key.len() as u64).to_le_bytes());
            packet.extend_from_slice(key);
            packet.extend_from_slice(&(value.len() as u64).to_le_bytes());
            packet.extend_from_slice(value);
        }
        packet
    }

    #[test]
    fn fixed_native_json_outputs_construct_ordered_values_and_exact_native_text() {
        for (meta, name) in [
            (json_array_serde_native_fn_meta(), "json_array_serde_native"),
            (
                json_object_serde_native_fn_meta(),
                "json_object_serde_native",
            ),
            (json_keys_serde_native_fn_meta(), "json_keys_serde_native"),
            (
                json_keys_path_serde_native_fn_meta(),
                "json_keys_path_serde_native",
            ),
            (
                json_pretty_serde_native_fn_meta(),
                "json_pretty_serde_native",
            ),
            (json_output_null_native_fn_meta(), "json_output_null_native"),
        ] {
            assert_eq!(meta.name, name);
        }
        let empty_array = array_packet(&[]);
        let empty_object = object_packet(&[]);
        assert!(json_array_serde_args_valid(&empty_array));
        assert!(json_object_serde_args_valid(&empty_object));
        assert_eq!(
            json_array_serde_native(&empty_array).unwrap(),
            Some(b"[]".to_vec())
        );
        assert_eq!(
            json_object_serde_native(&empty_object).unwrap(),
            Some(b"{}".to_vec())
        );
        let array = array_packet(&[
            b"null",
            b"1",
            b"1.0",
            b"-0.0",
            b"1e15",
            b"1e-16",
            b"18446744073709551615",
            br#"{"b":2,"a":[true,null]}"#,
        ]);
        assert!(json_array_serde_args_valid(&array));
        assert_eq!(json_array_serde_native(&array).unwrap(), Some(
            br#"[null, 1, 1.0, -0.0, 1e15, 1e-16, 18446744073709551615, {"a": [true, null], "b": 2}]"#.to_vec()
        ));
        let object = object_packet(&[
            (b"z", b"1"),
            (b"a", b"1.0"),
            (b"z", b"null"),
            (b"", br#""<>&""#),
            (b"a", b"2"),
        ]);
        assert!(json_object_serde_args_valid(&object));
        assert_eq!(
            json_object_serde_native(&object).unwrap(),
            Some(br#"{"": "<>&", "a": 2, "z": null}"#.to_vec())
        );
        assert_eq!(
            json_keys_serde_native(br#"{"z":1,"a":2}"#).unwrap(),
            Some(br#"["a", "z"]"#.to_vec())
        );
        assert_eq!(json_keys_serde_native(b"{}").unwrap(), Some(b"[]".to_vec()));
        assert_eq!(json_keys_serde_native(b"[]").unwrap(), None);
        assert_eq!(json_keys_serde_native(b"null").unwrap(), None);
        let document = br#"{"a":{"y":2,"x":1},"n":null}"#;
        assert_eq!(
            json_keys_path_serde_native(document, b"$.a").unwrap(),
            Some(br#"["x", "y"]"#.to_vec())
        );
        assert_eq!(
            json_keys_path_serde_native(document, b"$.missing").unwrap(),
            None
        );
        assert_eq!(json_keys_path_serde_native(document, b"$.n").unwrap(), None);
        assert_eq!(
            json_pretty_serde_native(br#"{"b":[1,2],"a":true}"#).unwrap(),
            Some(b"{\n  \"a\": true,\n  \"b\": [\n    1,\n    2\n  ]\n}".to_vec())
        );
        assert_eq!(
            json_pretty_serde_native(b"null").unwrap(),
            Some(b"null".to_vec())
        );
        assert_eq!(
            json_pretty_serde_native(b"1.0").unwrap(),
            Some(b"1.0".to_vec())
        );
        assert_eq!(json_output_null_native(None).unwrap(), None);
    }

    #[test]
    fn fixed_native_json_outputs_reject_malformed_packets_paths_and_false_nulls() {
        for packet in [
            Vec::new(),
            vec![0; 7],
            u64::MAX.to_le_bytes().to_vec(),
            1u64.to_le_bytes().to_vec(),
        ] {
            assert!(!json_array_serde_args_valid(&packet));
            assert!(!json_object_serde_args_valid(&packet));
            assert!(json_array_serde_native(&packet).is_err());
            assert!(json_object_serde_native(&packet).is_err());
        }
        let mut oversized_length = 1u64.to_le_bytes().to_vec();
        oversized_length.extend_from_slice(&u64::MAX.to_le_bytes());
        oversized_length.extend_from_slice(&0u64.to_le_bytes());
        assert!(!json_array_serde_args_valid(&oversized_length));
        assert!(!json_object_serde_args_valid(&oversized_length));
        assert!(json_array_serde_native(&oversized_length).is_err());
        assert!(json_object_serde_native(&oversized_length).is_err());
        let mut extra = array_packet(&[]);
        extra.push(0);
        assert!(!json_array_serde_args_valid(&extra));
        assert!(!json_object_serde_args_valid(&extra));
        assert!(json_array_serde_native(&extra).is_err());
        assert!(json_object_serde_native(&extra).is_err());
        for value in [b"".as_slice(), b"[".as_slice(), &[0xff]] {
            let array = array_packet(&[value]);
            let object = object_packet(&[(b"k", value)]);
            assert!(!json_array_serde_args_valid(&array));
            assert!(!json_object_serde_args_valid(&object));
            assert!(json_array_serde_native(&array).is_err());
            assert!(json_object_serde_native(&object).is_err());
        }
        let invalid_key = object_packet(&[(&[0xff], b"1")]);
        assert!(!json_object_serde_args_valid(&invalid_key));
        assert!(json_object_serde_native(&invalid_key).is_err());
        let mut truncated_array = array_packet(&[b"null"]);
        truncated_array.pop();
        let mut truncated_object = object_packet(&[(b"k", b"null")]);
        truncated_object.pop();
        assert!(!json_array_serde_args_valid(&truncated_array));
        assert!(!json_object_serde_args_valid(&truncated_object));
        assert!(json_array_serde_native(&truncated_array).is_err());
        assert!(json_object_serde_native(&truncated_object).is_err());
        assert!(json_keys_serde_native(b"[").is_err());
        assert!(json_pretty_serde_native(b"[").is_err());
        assert!(json_keys_path_serde_native(b"[{}]", b"$[*]").is_err());
        assert!(json_keys_path_serde_native(b"{}", b"not-a-path").is_err());
        assert!(json_keys_path_serde_native(b"{}", &[0xff]).is_err());
        assert!(json_output_null_native(Some(&0)).is_err());
    }
}

#[cfg(test)]
mod native_json_predicate_tests {
    use super::*;

    #[test]
    fn fixed_native_json_predicates_keep_serde_values_and_path_results() {
        let metadata = [
            (
                json_contains_serde_native_fn_meta(),
                "json_contains_serde_native",
            ),
            (
                json_contains_path_serde_native_fn_meta(),
                "json_contains_path_serde_native",
            ),
            (
                json_overlaps_serde_native_fn_meta(),
                "json_overlaps_serde_native",
            ),
            (
                json_member_of_serde_native_fn_meta(),
                "json_member_of_serde_native",
            ),
            (
                json_length_serde_native_fn_meta(),
                "json_length_serde_native",
            ),
            (
                json_length_path_serde_native_fn_meta(),
                "json_length_path_serde_native",
            ),
            (
                json_path_exists_serde_native_fn_meta(),
                "json_path_exists_serde_native",
            ),
            (
                json_member_of_binary_legacy_fn_meta(),
                "json_member_of_binary_legacy",
            ),
            (
                json_predicate_null_native_fn_meta(),
                "json_predicate_null_native",
            ),
            (
                json_predicate_missing_legacy_fn_meta(),
                "json_predicate_missing_legacy",
            ),
        ];
        for (meta, expected_name) in metadata {
            assert_eq!(meta.name, expected_name);
        }
        // serde_json's test_roundtrip_f64 records this literal as an old
        // non-float_roundtrip deserializer regression; prepared values must
        // preserve its exact bits, not drift before predicate evaluation.
        let exact = 51.24817837550540_4f64;
        let prepared = serde_json::to_vec(&exact).unwrap();
        assert_eq!(
            json_serde_native_value(&prepared)
                .unwrap()
                .as_f64()
                .unwrap()
                .to_bits(),
            exact.to_bits()
        );
        assert_eq!(
            json_contains_serde_native(br#"{"a":[1,2]}"#, br#"{"a":[2]}"#).unwrap(),
            Some(1)
        );
        assert_eq!(
            json_contains_serde_native(b"[1]", b"[1,1]").unwrap(),
            Some(1)
        );
        assert_eq!(json_contains_serde_native(b"0", b"1e-17").unwrap(), Some(0));
        assert_eq!(
            json_overlaps_serde_native(br#"{"k":1}"#, br#"{"k":1.0,"x":2}"#).unwrap(),
            Some(1)
        );
        assert_eq!(
            json_overlaps_serde_native(b"[0]", b"[1e-17]").unwrap(),
            Some(0)
        );
        assert_eq!(
            json_member_of_serde_native(br#""1""#, b"[1]").unwrap(),
            Some(0)
        );
        assert_eq!(
            json_member_of_serde_native(b"1.0", b"[1]").unwrap(),
            Some(1)
        );
        assert_eq!(
            json_member_of_serde_native(b"[1]", b"[[1],2]").unwrap(),
            Some(1)
        );
        assert_eq!(
            json_member_of_serde_native(b"[1]", b"[1]").unwrap(),
            Some(0)
        );
        assert_eq!(
            json_member_of_serde_native(b"18446744073709551615", b"[18446744073709551615]")
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            json_member_of_serde_native(b"9223372036854775808", b"[9223372036854775809]").unwrap(),
            Some(0)
        );
        assert_eq!(json_length_serde_native(b"null").unwrap(), Some(1));
        assert_eq!(json_length_serde_native(b"[]").unwrap(), Some(0));
        assert_eq!(
            json_length_serde_native(br#"{"a":1,"b":2}"#).unwrap(),
            Some(2)
        );
        let document = br#"{"a":[1,2],"n":null}"#;
        assert_eq!(
            json_contains_path_serde_native(document, b"2", b"$.a").unwrap(),
            Some(1)
        );
        assert_eq!(
            json_contains_path_serde_native(document, b"null", b"$.n").unwrap(),
            Some(1)
        );
        assert_eq!(
            json_contains_path_serde_native(document, b"2", b"$.missing").unwrap(),
            None
        );
        assert_eq!(
            json_length_path_serde_native(document, b"$.a").unwrap(),
            Some(2)
        );
        assert_eq!(
            json_length_path_serde_native(document, b"$.a[last]").unwrap(),
            Some(1)
        );
        assert_eq!(
            json_length_path_serde_native(document, b"$.missing").unwrap(),
            None
        );
        assert_eq!(
            json_path_exists_serde_native(document, b"$.n").unwrap(),
            Some(1)
        );
        assert_eq!(
            json_path_exists_serde_native(document, b"$.a[*]").unwrap(),
            Some(1)
        );
        assert_eq!(
            json_path_exists_serde_native(b"[]", b"$[*]").unwrap(),
            Some(0)
        );
        assert_eq!(
            json_path_exists_serde_native(document, b"$.missing").unwrap(),
            Some(0)
        );
        assert_eq!(json_predicate_null_native(None).unwrap(), None);
        assert_eq!(json_predicate_missing_legacy().unwrap(), None);
    }

    #[test]
    fn fixed_native_json_predicates_reject_transport_and_validate_whole_legacy_arrays() {
        assert!(json_serde_native_args_valid(b"null", Some(b"1"), None));
        assert!(!json_serde_native_args_valid(b"[", None, None));
        assert!(!json_serde_native_args_valid(b"null", Some(b""), None));
        assert!(!json_serde_native_args_valid(
            b"null",
            None,
            Some((&[0xff], true))
        ));
        assert!(!json_serde_native_args_valid(
            b"null",
            None,
            Some((b"not-a-path", true))
        ));
        assert!(!json_serde_native_args_valid(
            b"[]",
            None,
            Some((b"$[*]", false))
        ));
        assert!(json_serde_native_args_valid(
            b"[]",
            None,
            Some((b"$[*]", true))
        ));
        assert!(json_contains_serde_native(b"[", b"0").is_err());
        assert!(json_contains_path_serde_native(b"[]", b"0", b"$[*]").is_err());
        assert!(json_length_path_serde_native(b"[]", b"$[*]").is_err());
        assert!(json_path_exists_serde_native(b"[]", b"not-a-path").is_err());
        assert!(json_predicate_null_native(Some(&0)).is_err());

        let mut zero = vec![JsonType::I64 as u8];
        zero.extend_from_slice(&0i64.to_le_bytes());
        let mut tiny = vec![JsonType::Double as u8];
        tiny.extend_from_slice(&1e-17f64.to_le_bytes());
        // One actual int64 array element: header8 + entry5 + payload8.
        let mut array = vec![JsonType::Array as u8];
        array.extend_from_slice(&1u32.to_le_bytes());
        array.extend_from_slice(&21u32.to_le_bytes());
        array.push(JsonType::I64 as u8);
        array.extend_from_slice(&13u32.to_le_bytes());
        array.extend_from_slice(&0i64.to_le_bytes());
        assert!(json_member_binary_legacy_args_valid(&zero, &array));
        assert_eq!(
            json_member_of_binary_legacy(&zero, &array).unwrap(),
            Some(1)
        );
        // Legacy's raw mixed-number epsilon is intentionally NOT serde equality.
        assert_eq!(
            json_member_of_binary_legacy(&tiny, &array).unwrap(),
            Some(1)
        );
        assert_eq!(
            json_member_of_serde_native(b"1e-17", b"[0]").unwrap(),
            Some(0)
        );

        // A matching first element must not hide an invalid later child. Native
        // element_count decoded the entire node before even starting lookup.
        let mut malformed = vec![JsonType::Array as u8];
        malformed.extend_from_slice(&2u32.to_le_bytes());
        malformed.extend_from_slice(&26u32.to_le_bytes());
        malformed.push(JsonType::I64 as u8);
        malformed.extend_from_slice(&18u32.to_le_bytes());
        malformed.push(JsonType::I64 as u8);
        malformed.extend_from_slice(&26u32.to_le_bytes());
        malformed.extend_from_slice(&0i64.to_le_bytes());
        assert!(!json_member_binary_legacy_args_valid(&zero, &malformed));
        assert!(json_member_of_binary_legacy(&zero, &malformed).is_err());
        assert!(!json_member_binary_legacy_args_valid(b"", &array));
        assert!(!json_member_binary_legacy_args_valid(&zero, b""));
        assert!(json_member_of_binary_legacy(b"", &array).is_err());
        assert!(json_member_of_binary_legacy(&zero, b"").is_err());
        let scalar_fallback = [JsonType::I64 as u8];
        assert!(json_member_binary_legacy_args_valid(
            &scalar_fallback,
            &scalar_fallback
        ));
        assert_eq!(
            json_member_of_binary_legacy(&scalar_fallback, &scalar_fallback).unwrap(),
            Some(1)
        );
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tipb::ScalarFuncSig;

    use super::*;
    use crate::types::test_util::RpnFnScalarEvaluator;

    #[test]
    fn test_json_storage_native_statuses() {
        assert_eq!(json_storage_free_native(None).unwrap(), None);
        assert_eq!(json_storage_size_native(None).unwrap(), None);
        assert_eq!(json_storage_free_native(Some(b" ")).unwrap(), Some(vec![1]));
        assert_eq!(json_storage_size_native(Some(b" ")).unwrap(), Some(vec![1]));
        assert_eq!(json_storage_free_native(Some(b"[")).unwrap(), Some(vec![2]));
        assert_eq!(json_storage_size_native(Some(b"[")).unwrap(), Some(vec![2]));
        assert_eq!(
            json_storage_free_native(Some(&[0xff])).unwrap(),
            Some(vec![2])
        );
        assert_eq!(
            json_storage_size_native(Some(&[0xff])).unwrap(),
            Some(vec![2])
        );
        assert_eq!(
            json_storage_free_native(Some(b"null")).unwrap(),
            Some(vec![0; 9])
        );
        let mut size = vec![0];
        size.extend_from_slice(&2i64.to_le_bytes());
        assert_eq!(json_storage_size_native(Some(b"null")).unwrap(), Some(size));
    }

    #[test]
    fn test_json_quote_native_and_wire_policies() {
        assert_eq!(json_quote_native(None).unwrap(), None);
        assert_eq!(
            json_quote_native(Some(b"")).unwrap(),
            Some(b"\"\"".to_vec())
        );
        assert!(json_quote_native(Some(&[0xff])).is_err());
        let controls = b"\x00\x07\x0b\x1f\x08\x0c\t\n\r";
        assert_eq!(
            json_quote_native(Some(controls)).unwrap(),
            Some(br#""\u0000\u0007\u000b\u001f\b\f\t\n\r""#.to_vec()),
        );
        assert_eq!(
            quote(controls).unwrap(),
            Some(b"\"\x00\\a\\v\x1f\\b\\f\\t\\n\\r\"".to_vec()),
        );
        let text = "\"\\<>&/\u{2028}\u{2029}中";
        let expected = "\"\\\"\\\\<>&/\u{2028}\u{2029}中\"".as_bytes().to_vec();
        assert_eq!(
            json_quote_native(Some(text.as_bytes())).unwrap(),
            Some(expected.clone())
        );
        assert_eq!(quote(text.as_bytes()).unwrap(), Some(expected));
    }

    #[test]
    fn test_json_valid_native_signatures_and_nulls() {
        assert_eq!(json_valid_text_native(None).unwrap(), None);
        assert_eq!(json_valid_text_native(Some(b"null")).unwrap(), Some(1));
        assert_eq!(json_valid_text_native(Some(b" ")).unwrap(), Some(0));
        assert_eq!(json_valid_text_native(Some(b"[")).unwrap(), Some(0));
        assert_eq!(json_valid_text_native(Some(&[0xff])).unwrap(), Some(0));
        assert_eq!(json_valid_binary_native(None).unwrap(), None);
        assert_eq!(
            json_valid_binary_native(Some(&[JsonType::Literal as u8])).unwrap(),
            Some(1),
        );
        assert_eq!(json_valid_other_native().unwrap(), Some(0));
    }

    #[test]
    fn test_json_report_native_values_and_parse_errors() {
        assert_eq!(json_type_text_native(None).unwrap(), None);
        assert_eq!(json_type_binary_native(None).unwrap(), None);
        assert_eq!(json_depth_native(None).unwrap(), None);
        assert_eq!(
            json_type_text_native(Some(b"9223372036854775807")).unwrap(),
            Some(b"\0INTEGER".to_vec()),
        );
        assert_eq!(json_type_text_native(Some(b" ")).unwrap(), Some(vec![1]));
        assert_eq!(json_type_text_native(Some(&[0xff])).unwrap(), Some(vec![2]));
        // Native TYPE treats a malformed literal payload as BOOLEAN, rather
        // than introducing broader validation through the wire Json decoder.
        assert_eq!(
            json_type_binary_native(Some(&[JsonType::Literal as u8])).unwrap(),
            Some(b"\0BOOLEAN".to_vec()),
        );
        assert_eq!(
            json_type_binary_native(Some(&[JsonType::Opaque as u8])).unwrap(),
            Some(vec![2]),
        );
        assert!(json_type_binary_native(Some(b"")).is_err());
        let mut depth = vec![0];
        depth.extend_from_slice(&3i64.to_le_bytes());
        assert_eq!(
            json_depth_native(Some(br#"{"a":[1]}"#)).unwrap(),
            Some(depth)
        );
        assert_eq!(json_depth_native(Some(b"")).unwrap(), Some(vec![1]));
        assert_eq!(json_depth_native(Some(b"[")).unwrap(), Some(vec![2]));
    }

    #[test]
    fn test_json_depth() {
        let cases = vec![
            (None, None),
            (Some("null"), Some(1)),
            (Some("[true, 2017]"), Some(2)),
            (
                Some(r#"{"a": {"a1": [3]}, "b": {"b1": {"c": {"d": [5]}}}}"#),
                Some(6),
            ),
            (Some("{}"), Some(1)),
            (Some("[]"), Some(1)),
            (Some("true"), Some(1)),
            (Some("1"), Some(1)),
            (Some("-1"), Some(1)),
            (Some(r#""a""#), Some(1)),
            (Some(r#"[10, 20]"#), Some(2)),
            (Some(r#"[[], {}]"#), Some(2)),
            (Some(r#"[10, {"a": 20}]"#), Some(3)),
            (Some(r#"[[2], 3, [[[4]]]]"#), Some(5)),
            (Some(r#"{"Name": "Homer"}"#), Some(2)),
            (Some(r#"[10, {"a": 20}]"#), Some(3)),
            (
                Some(
                    r#"{"Person": {"Name": "Homer", "Age": 39, "Hobbies": ["Eating", "Sleeping"]} }"#,
                ),
                Some(4),
            ),
            (Some(r#"{"a":1}"#), Some(2)),
            (Some(r#"{"a":[1]}"#), Some(3)),
            (Some(r#"{"b":2, "c":3}"#), Some(2)),
            (Some(r#"[1]"#), Some(2)),
            (Some(r#"[1,2]"#), Some(2)),
            (Some(r#"[1,2,[1,3]]"#), Some(3)),
            (Some(r#"[1,2,[1,[5,[3]]]]"#), Some(5)),
            (Some(r#"[1,2,[1,[5,{"a":[2,3]}]]]"#), Some(6)),
            (Some(r#"[{"a":1}]"#), Some(3)),
            (Some(r#"[{"a":1,"b":2}]"#), Some(3)),
            (Some(r#"[{"a":{"a":1},"b":2}]"#), Some(4)),
        ];
        for (arg, expect_output) in cases {
            let arg = arg.map(|input| Json::from_str(input).unwrap());

            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.clone())
                .evaluate(ScalarFuncSig::JsonDepthSig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg);
        }
    }

    #[test]
    fn test_json_type() {
        let cases = vec![
            (None, None),
            (Some(r#"true"#), Some("BOOLEAN")),
            (Some(r#"null"#), Some("NULL")),
            (Some(r#"-3"#), Some("INTEGER")),
            (Some(r#"3"#), Some("INTEGER")),
            (Some(r#"9223372036854775808"#), Some("UNSIGNED INTEGER")),
            (Some(r#"3.14"#), Some("DOUBLE")),
            (Some(r#"[1, 2, 3]"#), Some("ARRAY")),
            (Some(r#"{"name": 123}"#), Some("OBJECT")),
        ];

        for (arg, expect_output) in cases {
            let arg = arg.map(|input| Json::from_str(input).unwrap());
            let expect_output = expect_output.map(Bytes::from);

            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.clone())
                .evaluate(ScalarFuncSig::JsonTypeSig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg);
        }
    }

    #[test]
    fn test_json_modify() {
        let cases: Vec<(_, Vec<ScalarValue>, _)> = vec![
            (
                ScalarFuncSig::JsonSetSig,
                vec![
                    None::<Json>.into(),
                    None::<Bytes>.into(),
                    None::<Json>.into(),
                ],
                None::<Json>,
            ),
            (
                ScalarFuncSig::JsonSetSig,
                vec![
                    Some(Json::from_i64(9).unwrap()).into(),
                    Some(b"$[1]".to_vec()).into(),
                    Some(Json::from_u64(3).unwrap()).into(),
                ],
                Some(r#"[9,3]"#.parse().unwrap()),
            ),
            (
                ScalarFuncSig::JsonInsertSig,
                vec![
                    Some(Json::from_i64(9).unwrap()).into(),
                    Some(b"$[1]".to_vec()).into(),
                    Some(Json::from_u64(3).unwrap()).into(),
                ],
                Some(r#"[9,3]"#.parse().unwrap()),
            ),
            (
                ScalarFuncSig::JsonReplaceSig,
                vec![
                    Some(Json::from_i64(9).unwrap()).into(),
                    Some(b"$[1]".to_vec()).into(),
                    Some(Json::from_u64(3).unwrap()).into(),
                ],
                Some(r#"9"#.parse().unwrap()),
            ),
            (
                ScalarFuncSig::JsonSetSig,
                vec![
                    Some(Json::from_str(r#"{"a":"x"}"#).unwrap()).into(),
                    Some(b"$.a".to_vec()).into(),
                    None::<Json>.into(),
                ],
                Some(r#"{"a":null}"#.parse().unwrap()),
            ),
        ];
        for (sig, args, expect_output) in cases {
            let output: Option<Json> = RpnFnScalarEvaluator::new()
                .push_params(args.clone())
                .evaluate(sig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", args);
        }
    }

    #[test]
    fn test_json_array() {
        let cases = vec![
            (vec![], Some(r#"[]"#)),
            (vec![Some(r#"1"#), None], Some(r#"[1, null]"#)),
            (
                vec![
                    Some(r#"1"#),
                    None,
                    Some(r#"2"#),
                    Some(r#""sdf""#),
                    Some(r#""k1""#),
                    Some(r#""v1""#),
                ],
                Some(r#"[1, null, 2, "sdf", "k1", "v1"]"#),
            ),
        ];

        for (vargs, expected) in cases {
            let vargs = vargs
                .into_iter()
                .map(|input| input.map(|s| Json::from_str(s).unwrap()))
                .collect::<Vec<_>>();
            let expected = expected.map(|s| Json::from_str(s).unwrap());

            let output = RpnFnScalarEvaluator::new()
                .push_params(vargs.clone())
                .evaluate(ScalarFuncSig::JsonArraySig)
                .unwrap();
            assert_eq!(output, expected, "{:?}", vargs);
        }
    }

    #[test]
    fn test_json_merge() {
        let cases = vec![
            (vec![None, None], None),
            (vec![Some("{}"), Some("[]")], Some("[{}]")),
            (
                vec![Some(r#"{}"#), Some(r#"[]"#), Some(r#"3"#), Some(r#""4""#)],
                Some(r#"[{}, 3, "4"]"#),
            ),
            (
                vec![Some("[1, 2]"), Some("[3, 4]")],
                Some(r#"[1, 2, 3, 4]"#),
            ),
        ];

        for (vargs, expected) in cases {
            let vargs = vargs
                .into_iter()
                .map(|input| input.map(|s| Json::from_str(s).unwrap()))
                .collect::<Vec<_>>();
            let expected = expected.map(|s| Json::from_str(s).unwrap());

            let output = RpnFnScalarEvaluator::new()
                .push_params(vargs.clone())
                .evaluate(ScalarFuncSig::JsonMergeSig)
                .unwrap();
            assert_eq!(output, expected, "{:?}", vargs);
        }
    }

    #[test]
    fn test_json_object() {
        let cases = vec![
            (vec![], r#"{}"#),
            (vec![("1", None)], r#"{"1":null}"#),
            (
                vec![
                    ("1", None),
                    ("2", Some(r#""sdf""#)),
                    ("k1", Some(r#""v1""#)),
                ],
                r#"{"1":null,"2":"sdf","k1":"v1"}"#,
            ),
        ];

        for (vargs, expected) in cases {
            let mut new_vargs: Vec<ScalarValue> = vec![];
            for (key, value) in vargs
                .into_iter()
                .map(|(key, value)| (Bytes::from(key), value.map(|s| Json::from_str(s).unwrap())))
            {
                new_vargs.push(ScalarValue::from(key));
                new_vargs.push(ScalarValue::from(value));
            }

            let expected = Json::from_str(expected).unwrap();

            let output: Json = RpnFnScalarEvaluator::new()
                .push_params(new_vargs)
                .evaluate(ScalarFuncSig::JsonObjectSig)
                .unwrap()
                .unwrap();
            assert_eq!(output, expected);
        }

        let err_cases = vec![
            vec![
                ScalarValue::from(Bytes::from("1")),
                ScalarValue::from(None::<Json>),
                ScalarValue::from(Bytes::from("1")),
            ],
            vec![
                ScalarValue::from(None::<Bytes>),
                ScalarValue::from(Json::from_str("1").unwrap()),
            ],
        ];

        for err_args in err_cases {
            let output: Result<Option<Json>> = RpnFnScalarEvaluator::new()
                .push_params(err_args)
                .evaluate(ScalarFuncSig::JsonObjectSig);

            output.unwrap_err();
        }
    }

    #[test]
    fn test_json_quote() {
        let cases = vec![
            (None, None),
            (Some(""), Some(r#""""#)),
            (Some(r#""""#), Some(r#""\"\"""#)),
            (Some(r#"a"#), Some(r#""a""#)),
            (Some(r#"3"#), Some(r#""3""#)),
            (Some(r#"{"a": "b"}"#), Some(r#""{\"a\": \"b\"}""#)),
            (Some(r#"{"a":     "b"}"#), Some(r#""{\"a\":     \"b\"}""#)),
            (
                Some(r#"hello,"quoted string",world"#),
                Some(r#""hello,\"quoted string\",world""#),
            ),
            (
                Some(r#"hello,"宽字符",world"#),
                Some(r#""hello,\"宽字符\",world""#),
            ),
            (
                Some(r#""Invalid Json string	is OK"#),
                Some(r#""\"Invalid Json string\tis OK""#),
            ),
            (Some(r#"1\u2232\u22322"#), Some(r#""1\\u2232\\u22322""#)),
            (
                Some("new line \"\r\n\" is ok"),
                Some(r#""new line \"\r\n\" is ok""#),
            ),
        ];

        for (arg, expect_output) in cases {
            let arg = arg.map(Bytes::from);
            let expect_output = expect_output.map(Bytes::from);

            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.clone())
                .evaluate(ScalarFuncSig::JsonQuoteSig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg);
        }
    }

    #[test]
    fn test_json_unquote() {
        let cases = vec![
            (None, None, true),
            (Some(r#"""#), Some(r#"""#), true),
            (Some(r"a"), Some("a"), true),
            (Some(r#""3"#), Some(r#""3"#), true),
            (Some(r#"{"a":  "b"}"#), Some(r#"{"a":  "b"}"#), true),
            (
                Some(r#""hello,\"quoted string\",world""#),
                Some(r#"hello,"quoted string",world"#),
                true,
            ),
            (Some(r#"A中\\\"文B"#), Some(r#"A中\\\"文B"#), true),
            (Some(r#""A中\\\"文B""#), Some(r#"A中\"文B"#), true),
            (Some(r#""\u00E0A中\\\"文B""#), Some(r#"àA中\"文B"#), true),
            (Some(r#""a""#), Some(r#"a"#), true),
            (Some(r#"""a"""#), None, false),
            (Some(r#""""a""""#), None, false),
        ];

        for (arg, expect, success) in cases {
            let arg = arg.map(Bytes::from);
            let expect = expect.map(Bytes::from);
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.clone())
                .evaluate(ScalarFuncSig::JsonUnquoteSig);
            match output {
                Ok(s) => {
                    assert_eq!(s, expect, "{:?}", arg);
                    assert_eq!(success, true);
                }
                Err(_) => {
                    assert_eq!(success, false);
                }
            }
        }
    }

    #[test]
    fn test_json_extract() {
        let cases: Vec<(Vec<ScalarValue>, _)> = vec![
            (vec![None::<Json>.into(), None::<Bytes>.into()], None),
            (
                vec![
                    Some(Json::from_str("[10, 20, [30, 40]]").unwrap()).into(),
                    Some(b"$[1]".to_vec()).into(),
                ],
                Some("20"),
            ),
            (
                vec![
                    Some(Json::from_str("[10, 20, [30, 40]]").unwrap()).into(),
                    Some(b"$[1]".to_vec()).into(),
                    Some(b"$[0]".to_vec()).into(),
                ],
                Some("[20, 10]"),
            ),
            (
                vec![
                    Some(Json::from_str("[10, 20, [30, 40]]").unwrap()).into(),
                    Some(b"$[2][*]".to_vec()).into(),
                ],
                Some("[30, 40]"),
            ),
            (
                vec![
                    Some(Json::from_str("[10, 20, [30, 40]]").unwrap()).into(),
                    Some(b"$[2][*]".to_vec()).into(),
                    None::<Bytes>.into(),
                ],
                None,
            ),
        ];

        for (vargs, expected) in cases {
            let expected = expected.map(|s| Json::from_str(s).unwrap());

            let output = RpnFnScalarEvaluator::new()
                .push_params(vargs.clone())
                .evaluate(ScalarFuncSig::JsonExtractSig)
                .unwrap();
            assert_eq!(output, expected, "{:?}", vargs);
        }
    }

    #[test]
    fn test_json_remove() {
        let cases: Vec<(Vec<ScalarValue>, _)> = vec![(
            vec![
                Some(Json::from_str(r#"["a", ["b", "c"], "d"]"#).unwrap()).into(),
                Some(b"$[1]".to_vec()).into(),
            ],
            Some(r#"["a", "d"]"#),
        )];

        for (vargs, expected) in cases {
            let expected = expected.map(|s| Json::from_str(s).unwrap());

            let output = RpnFnScalarEvaluator::new()
                .push_params(vargs.clone())
                .evaluate(ScalarFuncSig::JsonRemoveSig)
                .unwrap();
            assert_eq!(output, expected, "{:?}", vargs);
        }
    }

    #[test]
    fn test_json_length() {
        let cases: Vec<(Vec<ScalarValue>, Option<i64>)> = vec![
            (
                vec![
                    Some(Json::from_str("null").unwrap()).into(),
                    None::<Bytes>.into(),
                ],
                None,
            ),
            (
                vec![
                    Some(Json::from_str("false").unwrap()).into(),
                    None::<Bytes>.into(),
                ],
                None,
            ),
            (vec![Some(Json::from_str("1").unwrap()).into()], Some(1)),
            (
                vec![
                    Some(Json::from_str(r#"{"a": [1, 2, {"aa": "xx"}]}"#).unwrap()).into(),
                    Some(b"$.*".to_vec()).into(),
                ],
                None,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a":{"a":1},"b":2}"#).unwrap()).into(),
                    Some(b"$".to_vec()).into(),
                ],
                Some(2),
            ),
        ];

        for (vargs, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(vargs.clone())
                .evaluate(ScalarFuncSig::JsonLengthSig)
                .unwrap();
            assert_eq!(output, expected, "{:?}", vargs);
        }
    }

    #[test]
    fn test_json_valid() {
        let cases: Vec<(Vec<ScalarValue>, Option<i64>)> = vec![
            (
                vec![Some(Json::from_str(r#"{"a":1}"#).unwrap()).into()],
                Some(1),
            ),
            (vec![Some(b"hello".to_vec()).into()], Some(0)),
            (vec![Some(b"\"hello\"".to_vec()).into()], Some(1)),
            (vec![Some(b"null".to_vec()).into()], Some(1)),
            (vec![Some(Json::from_str(r#"{}"#).unwrap()).into()], Some(1)),
            (vec![Some(Json::from_str(r#"[]"#).unwrap()).into()], Some(1)),
            (vec![Some(b"2".to_vec()).into()], Some(1)),
            (vec![Some(b"2.5".to_vec()).into()], Some(1)),
            (vec![Some(b"2019-8-19".to_vec()).into()], Some(0)),
            (vec![Some(b"\"2019-8-19\"".to_vec()).into()], Some(1)),
            (vec![Some(2).into()], Some(0)),
            (vec![Some(2.5).into()], Some(0)),
            (vec![None::<Json>.into()], None),
            (vec![None::<Bytes>.into()], None),
            (vec![None::<Int>.into()], None),
        ];

        for (vargs, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(vargs.clone())
                .evaluate(ScalarFuncSig::JsonValidJsonSig)
                .unwrap();
            assert_eq!(output, expected, "{:?}", vargs);
        }
    }

    #[test]
    fn test_json_contains() {
        let cases: Vec<(Vec<ScalarValue>, Option<i64>)> = vec![
            (
                vec![
                    Some(Json::from_str(r#"{"a":{"a":1},"b":2}"#).unwrap()).into(),
                    Some(Json::from_str(r#"2"#).unwrap()).into(),
                    Some(b"$.b".to_vec()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a":{"a":1},"b":2}"#).unwrap()).into(),
                    Some(Json::from_str(r#"3"#).unwrap()).into(),
                    Some(b"$.b".to_vec()).into(),
                ],
                Some(0),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a":{"a":1},"b":2}"#).unwrap()).into(),
                    Some(Json::from_str(r#"{"b":3}"#).unwrap()).into(),
                ],
                Some(0),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a":{"a":1},"b":2}"#).unwrap()).into(),
                    Some(Json::from_str(r#"{"b":2}"#).unwrap()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a":{"a":1},"b":2}"#).unwrap()).into(),
                    Some(Json::from_str(r#"{"a":1}"#).unwrap()).into(),
                    Some(b"$.a".to_vec()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[{"optUid": 10, "value": "admin"}]"#).unwrap()).into(),
                    Some(Json::from_str(r#"10"#).unwrap()).into(),
                    Some(b"$[0].optUid".to_vec()).into(),
                ],
                Some(1),
            ),
            // copy from tidb  Tests None arguments
            (vec![None::<Json>.into(), None::<Json>.into()], None),
            (
                vec![
                    Some(Json::from_str(r#"{"a":1}"#).unwrap()).into(),
                    None::<Json>.into(),
                ],
                None,
            ),
            (
                vec![
                    None::<Json>.into(),
                    Some(Json::from_str(r#"1"#).unwrap()).into(),
                ],
                None,
            ),
            (
                vec![
                    None::<Json>.into(),
                    Some(Json::from_str(r#"1"#).unwrap()).into(),
                    Some(b"$.c".to_vec()).into(),
                ],
                None,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": [1, 2, {"aa": "xx"}]}"#).unwrap()).into(),
                    None::<Json>.into(),
                    Some(b"$.a[3]".to_vec()).into(),
                ],
                None,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": [1, 2, {"aa": "xx"}]}"#).unwrap()).into(),
                    Some(Json::from_str(r#"1"#).unwrap()).into(),
                    None::<Bytes>.into(),
                ],
                None,
            ),
            //  Tests with path expression
            (
                vec![
                    Some(Json::from_str(r#"[1,2,[1,[5,[3]]]]"#).unwrap()).into(),
                    Some(Json::from_str(r#"[1,3]"#).unwrap()).into(),
                    Some(b"$[2]".to_vec()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[1,2,[1,[5,{"a":[2,3]}]]]"#).unwrap()).into(),
                    Some(Json::from_str(r#"[1,{"a":[3]}]"#).unwrap()).into(),
                    Some(b"$[2]".to_vec()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[{"a":1}]"#).unwrap()).into(),
                    Some(Json::from_str(r#"{"a":1}"#).unwrap()).into(),
                    Some(b"$".to_vec()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[{"a":1,"b":2}]"#).unwrap()).into(),
                    Some(Json::from_str(r#"{"a":1,"b":2}"#).unwrap()).into(),
                    Some(b"$".to_vec()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[{"a":{"a":1},"b":2}]"#).unwrap()).into(),
                    Some(Json::from_str(r#"{"a":1}"#).unwrap()).into(),
                    Some(b"$.a".to_vec()).into(),
                ],
                None,
            ),
            // Tests without path expression
            // 		{[]interface{}{`{}`, `{}`}, 1, nil},
            // 		{[]interface{}{`{"a":1}`, `{}`}, 1, nil},
            // 		{[]interface{}{`{"a":1}`, `1`}, 0, nil},
            (
                vec![
                    Some(Json::from_str(r#"{}"#).unwrap()).into(),
                    Some(Json::from_str(r#"{}"#).unwrap()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a":1}"#).unwrap()).into(),
                    Some(Json::from_str(r#"{}"#).unwrap()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a":1}"#).unwrap()).into(),
                    Some(Json::from_str(r#"1"#).unwrap()).into(),
                ],
                Some(0),
            ),
            // 		{[]interface{}{`{"a":[1]}`, `[1]`}, 0, nil},
            // 		{[]interface{}{`{"b":2, "c":3}`, `{"c":3}`}, 1, nil},
            // 		{[]interface{}{`1`, `1`}, 1, nil},
            // 		{[]interface{}{`[1]`, `1`}, 1, nil},
            (
                vec![
                    Some(Json::from_str(r#"{"a":[1]}"#).unwrap()).into(),
                    Some(Json::from_str(r#"[1]"#).unwrap()).into(),
                ],
                Some(0),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"b":2, "c":3}"#).unwrap()).into(),
                    Some(Json::from_str(r#"{"c":3}"#).unwrap()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"1"#).unwrap()).into(),
                    Some(Json::from_str(r#"1"#).unwrap()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[1]"#).unwrap()).into(),
                    Some(Json::from_str(r#"1"#).unwrap()).into(),
                ],
                Some(1),
            ),
            // 		{[]interface{}{`[1,2]`, `[1]`}, 1, nil},
            // 		{[]interface{}{`[1,2]`, `[1,3]`}, 0, nil},
            // 		{[]interface{}{`[1,2]`, `["1"]`}, 0, nil},
            // 		{[]interface{}{`[1,2,[1,3]]`, `[1,3]`}, 1, nil},
            (
                vec![
                    Some(Json::from_str(r#"[1,2]"#).unwrap()).into(),
                    Some(Json::from_str(r#"[1]"#).unwrap()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[1,2]"#).unwrap()).into(),
                    Some(Json::from_str(r#"[1,3]"#).unwrap()).into(),
                ],
                Some(0),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[1,2]"#).unwrap()).into(),
                    Some(Json::from_str(r#"["1"]"#).unwrap()).into(),
                ],
                Some(0),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[1,2,[1,3]]"#).unwrap()).into(),
                    Some(Json::from_str(r#"[1,3]"#).unwrap()).into(),
                ],
                Some(1),
            ),
            // 		{[]interface{}{`[1,2,[1,3]]`, `[1,      3]`}, 1, nil},
            // 		{[]interface{}{`[1,2,[1,[5,[3]]]]`, `[1,3]`}, 1, nil},
            // 		{[]interface{}{`[1,2,[1,[5,{"a":[2,3]}]]]`, `[1,{"a":[3]}]`}, 1, nil},
            (
                vec![
                    Some(Json::from_str(r#"[1,2,[1,3]]"#).unwrap()).into(),
                    Some(Json::from_str(r#"[1,      3]"#).unwrap()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[1,2,[1,[5,[3]]]]"#).unwrap()).into(),
                    Some(Json::from_str(r#"[1,3]"#).unwrap()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[1,2,[1,[5,{"a":[2,3]}]]]"#).unwrap()).into(),
                    Some(Json::from_str(r#"[1,{"a":[3]}]"#).unwrap()).into(),
                ],
                Some(1),
            ),
            // 		{[]interface{}{`[{"a":1}]`, `{"a":1}`}, 1, nil},
            // 		{[]interface{}{`[{"a":1,"b":2}]`, `{"a":1}`}, 1, nil},
            // 		{[]interface{}{`[{"a":{"a":1},"b":2}]`, `{"a":1}`}, 0, nil},
            (
                vec![
                    Some(Json::from_str(r#"[{"a":1}]"#).unwrap()).into(),
                    Some(Json::from_str(r#"{"a":1}"#).unwrap()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[{"a":1,"b":2}]"#).unwrap()).into(),
                    Some(Json::from_str(r#"{"a":1}"#).unwrap()).into(),
                ],
                Some(1),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[{"a":{"a":1},"b":2}]"#).unwrap()).into(),
                    Some(Json::from_str(r#"{"a":1}"#).unwrap()).into(),
                ],
                Some(0),
            ),
            // Tests path expression contains any asterisk
            //      {[]interface{}{`{"a": [1, 2, {"aa": "xx"}]}`, `1`, "$.*"}, nil,
            // json.ErrInvalidJSONPathWildcard}, 		{[]interface{}{`{"a": [1, 2, {"aa":
            // "xx"}]}`, `1`, "$[*]"}, nil, json.ErrInvalidJSONPathWildcard},
            // 		{[]interface{}{`{"a": [1, 2, {"aa": "xx"}]}`, `1`, "$**.a"}, nil,
            // json.ErrInvalidJSONPathWildcard},
            (
                vec![
                    Some(Json::from_str(r#"{"a": [1, 2, {"aa": "xx"}]}"#).unwrap()).into(),
                    Some(Json::from_str(r#"1"#).unwrap()).into(),
                    Some(b"$.*".to_vec()).into(),
                ],
                None,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": [1, 2, {"aa": "xx"}]}"#).unwrap()).into(),
                    Some(Json::from_str(r#"1"#).unwrap()).into(),
                    Some(b"$[*]".to_vec()).into(),
                ],
                None,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": [1, 2, {"aa": "xx"}]}"#).unwrap()).into(),
                    Some(Json::from_str(r#"1"#).unwrap()).into(),
                    Some(b"$**.a".to_vec()).into(),
                ],
                None,
            ),
            // Tests path expression does not identify a section of the target document
            //      {[]interface{}{`{"a": [1, 2, {"aa": "xx"}]}`, `1`, "$.c"}, nil, nil},
            // 		{[]interface{}{`{"a": [1, 2, {"aa": "xx"}]}`, `1`, "$.a[3]"}, nil, nil},
            // 		{[]interface{}{`{"a": [1, 2, {"aa": "xx"}]}`, `1`, "$.a[2].b"}, nil, nil},
            (
                vec![
                    Some(Json::from_str(r#"{"a": [1, 2, {"aa": "xx"}]}"#).unwrap()).into(),
                    Some(Json::from_str(r#"1"#).unwrap()).into(),
                    Some(b"$.c".to_vec()).into(),
                ],
                None,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": [1, 2, {"aa": "xx"}]}"#).unwrap()).into(),
                    Some(Json::from_str(r#"1"#).unwrap()).into(),
                    Some(b"$.a[3]".to_vec()).into(),
                ],
                None,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": [1, 2, {"aa": "xx"}]}"#).unwrap()).into(),
                    Some(Json::from_str(r#"1"#).unwrap()).into(),
                    Some(b"$.a[2].b".to_vec()).into(),
                ],
                None,
            ),
        ];

        for (vargs, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(vargs.clone())
                .evaluate(ScalarFuncSig::JsonContainsSig)
                .unwrap();
            assert_eq!(output, expected, "{:?}", vargs);
        }
    }

    #[test]
    fn test_json_keys() {
        let cases: Vec<(Vec<ScalarValue>, Option<Json>, bool)> = vec![
            // Tests nil arguments
            (vec![None::<Json>.into(), None::<Bytes>.into()], None, true),
            (
                vec![None::<Json>.into(), Some(b"$.c".to_vec()).into()],
                None,
                true,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": 1}"#).unwrap()).into(),
                    None::<Bytes>.into(),
                ],
                None,
                true,
            ),
            (vec![None::<Json>.into()], None, true),
            // Tests with other type
            (vec![Some(Json::from_str("1").unwrap()).into()], None, true),
            (
                vec![Some(Json::from_str(r#""str""#).unwrap()).into()],
                None,
                true,
            ),
            (
                vec![Some(Json::from_str(r#"true"#).unwrap()).into()],
                None,
                true,
            ),
            (
                vec![Some(Json::from_str("null").unwrap()).into()],
                None,
                true,
            ),
            (
                vec![Some(Json::from_str(r#"[1, 2]"#).unwrap()).into()],
                None,
                true,
            ),
            (
                vec![Some(Json::from_str(r#"["1", "2"]"#).unwrap()).into()],
                None,
                true,
            ),
            // Tests without path expression
            (
                vec![Some(Json::from_str(r#"{}"#).unwrap()).into()],
                Some(Json::from_str("[]").unwrap()),
                true,
            ),
            (
                vec![Some(Json::from_str(r#"{"a": 1}"#).unwrap()).into()],
                Some(Json::from_str(r#"["a"]"#).unwrap()),
                true,
            ),
            (
                vec![Some(Json::from_str(r#"{"a": 1, "b": 2}"#).unwrap()).into()],
                Some(Json::from_str(r#"["a", "b"]"#).unwrap()),
                true,
            ),
            (
                vec![Some(Json::from_str(r#"{"a": {"c": 3}, "b": 2}"#).unwrap()).into()],
                Some(Json::from_str(r#"["a", "b"]"#).unwrap()),
                true,
            ),
            // Tests with path expression
            (
                vec![
                    Some(Json::from_str(r#"{"a": 1}"#).unwrap()).into(),
                    Some(b"$.a".to_vec()).into(),
                ],
                None,
                true,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": {"c": 3}, "b": 2}"#).unwrap()).into(),
                    Some(b"$.a".to_vec()).into(),
                ],
                Some(Json::from_str(r#"["c"]"#).unwrap()),
                true,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": {"c": 3}, "b": 2}"#).unwrap()).into(),
                    None::<Bytes>.into(),
                ],
                None,
                true,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": {"c": 3}, "b": 2}"#).unwrap()).into(),
                    Some(b"$.a.c".to_vec()).into(),
                ],
                None,
                true,
            ),
            // Tests path expression contains any asterisk
            (
                vec![
                    Some(Json::from_str(r#"{}"#).unwrap()).into(),
                    Some(b"$.*".to_vec()).into(),
                ],
                None,
                false,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": 1}"#).unwrap()).into(),
                    Some(b"$.*".to_vec()).into(),
                ],
                None,
                false,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": {"c": 3}, "b": 2}"#).unwrap()).into(),
                    Some(b"$.*".to_vec()).into(),
                ],
                None,
                false,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": {"c": 3}, "b": 2}"#).unwrap()).into(),
                    Some(b"$.a.*".to_vec()).into(),
                ],
                None,
                false,
            ),
            // Tests path expression does not identify a section of the target document
            (
                vec![
                    Some(Json::from_str(r#"{"a": 1}"#).unwrap()).into(),
                    Some(b"$.b".to_vec()).into(),
                ],
                None,
                true,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": {"c": 3}, "b": 2}"#).unwrap()).into(),
                    Some(b"$.c".to_vec()).into(),
                ],
                None,
                true,
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": {"c": 3}, "b": 2}"#).unwrap()).into(),
                    Some(b"$.a.d".to_vec()).into(),
                ],
                None,
                true,
            ),
        ];
        for (vargs, expected, is_success) in cases {
            let output = RpnFnScalarEvaluator::new().push_params(vargs.clone());
            let output = if vargs.len() == 1 {
                output.evaluate(ScalarFuncSig::JsonKeysSig)
            } else {
                output.evaluate(ScalarFuncSig::JsonKeys2ArgsSig)
            };
            if is_success {
                assert_eq!(output.unwrap(), expected, "{:?}", vargs);
            } else {
                output.unwrap_err();
            }
        }
    }

    #[test]
    fn test_json_member_of() {
        let test_cases = vec![
            (Some(r#"1"#), Some(r#"[1,2]"#), Some(1)),
            (Some(r#"1"#), Some(r#"[1]"#), Some(1)),
            (Some(r#"1"#), Some(r#"[0]"#), Some(0)),
            (Some(r#"1"#), Some(r#"[[1]]"#), Some(0)),
            (Some(r#""1""#), Some(r#"[1]"#), Some(0)),
            (Some(r#""1""#), Some(r#"["1"]"#), Some(1)),
            (Some(r#""{\"a\":1}""#), Some(r#"{"a":1}"#), Some(0)),
            (Some(r#""{\"a\":1}""#), Some(r#"[{"a":1}]"#), Some(0)),
            (Some(r#""{\"a\":1}""#), Some(r#"[{"a":1}, 1]"#), Some(0)),
            (Some(r#""{\"a\":1}""#), Some(r#"["{\"a\":1}"]"#), Some(1)),
            (Some(r#""{\"a\":1}""#), Some(r#"["{\"a\":1}",1]"#), Some(1)),
            (Some(r#"1"#), Some(r#"1"#), Some(1)),
            (Some(r#"[4,5]"#), Some(r#"[[3,4],[4,5]]"#), Some(1)),
            (Some(r#""[4,5]""#), Some(r#"[[3,4],"[4,5]"]"#), Some(1)),
            (Some(r#"{"a":1}"#), Some(r#"{"a":1}"#), Some(1)),
            (Some(r#"{"a":1}"#), Some(r#"{"a":1, "b":2}"#), Some(0)),
            (Some(r#"{"a":1}"#), Some(r#"[{"a":1}]"#), Some(1)),
            (Some(r#"{"a":1}"#), Some(r#"{"b": {"a":1}}"#), Some(0)),
            (Some(r#"1"#), Some(r#"1"#), Some(1)),
            (Some(r#"[1,2]"#), Some(r#"[1,2]"#), Some(0)),
            (Some(r#"[1,2]"#), Some(r#"[[1,2]]"#), Some(1)),
            (Some(r#"[[1,2]]"#), Some(r#"[[1,2]]"#), Some(0)),
            (Some(r#"[[1,2]]"#), Some(r#"[[[1,2]]]"#), Some(1)),
            (None, Some(r#"[[[1,2]]]"#), None),
            (Some(r#"[[1,2]]"#), None, None),
            (None, None, None),
        ];
        for (js, value, expected) in test_cases {
            let args: Vec<ScalarValue> = vec![
                js.map(|js| Json::from_str(js).unwrap()).into(),
                value.map(|value| Json::from_str(value).unwrap()).into(),
            ];
            let output = RpnFnScalarEvaluator::new()
                .push_params(args.clone())
                .evaluate(ScalarFuncSig::JsonMemberOfSig)
                .unwrap();
            assert_eq!(output, expected, "{:?}", args);
        }
    }

    #[test]
    fn test_json_array_append() {
        let cases: Vec<(Vec<ScalarValue>, _)> = vec![
            // use exact testcase from TiDB repo
            (
                vec![
                    Some(Json::from_str(r#"{"a": 1, "b": [2, 3], "c": 4}"#).unwrap()).into(),
                    Some(b"$.d".to_vec()).into(),
                    Some(Json::from_str(r#""z""#).unwrap()).into(),
                ],
                Some(r#"{"a": 1, "b": [2, 3], "c": 4}"#.parse().unwrap()),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": 1, "b": [2, 3], "c": 4}"#).unwrap()).into(),
                    Some(b"$".to_vec()).into(),
                    Some(Json::from_str(r#""w""#).unwrap()).into(),
                ],
                Some(r#"[{"a": 1, "b": [2, 3], "c": 4}, "w"]"#.parse().unwrap()),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": 1, "b": [2, 3], "c": 4}"#).unwrap()).into(),
                    Some(b"$".to_vec()).into(),
                    None::<Json>.into(),
                ],
                Some(r#"[{"a": 1, "b": [2, 3], "c": 4}, null]"#.parse().unwrap()),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": 1}"#).unwrap()).into(),
                    Some(b"$".to_vec()).into(),
                    Some(Json::from_str(r#"{"b": 2}"#).unwrap()).into(),
                ],
                Some(r#"[{"a": 1}, {"b": 2}]"#.parse().unwrap()),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": 1}"#).unwrap()).into(),
                    Some(b"$".to_vec()).into(),
                    Some(Json::from_str(r#"{"b": 2}"#).unwrap()).into(),
                ],
                Some(r#"[{"a": 1}, {"b": 2}]"#.parse().unwrap()),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": 1}"#).unwrap()).into(),
                    Some(b"$.a".to_vec()).into(),
                    Some(Json::from_str(r#"{"b": 2}"#).unwrap()).into(),
                ],
                Some(r#"{"a": [1, {"b": 2}]}"#.parse().unwrap()),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": 1}"#).unwrap()).into(),
                    Some(b"$.a".to_vec()).into(),
                    Some(Json::from_str(r#"{"b": 2}"#).unwrap()).into(),
                    Some(b"$.a[1]".to_vec()).into(),
                    Some(Json::from_str(r#"{"b": 2}"#).unwrap()).into(),
                ],
                Some(r#"{"a": [1, [{"b": 2}, {"b": 2}]]}"#.parse().unwrap()),
            ),
            (
                vec![
                    None::<Json>.into(),
                    Some(b"$".to_vec()).into(),
                    None::<Json>.into(),
                ],
                None::<Json>,
            ),
            (
                vec![
                    None::<Json>.into(),
                    Some(b"$".to_vec()).into(),
                    Some(Json::from_str(r#""a""#).unwrap()).into(),
                ],
                None::<Json>,
            ),
            (
                vec![
                    Some(Json::from_str(r#"null"#).unwrap()).into(),
                    Some(b"$".to_vec()).into(),
                    None::<Json>.into(),
                ],
                Some(r#"[null, null]"#.parse().unwrap()),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[]"#).unwrap()).into(),
                    Some(b"$".to_vec()).into(),
                    None::<Json>.into(),
                ],
                Some(r#"[null]"#.parse().unwrap()),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{}"#).unwrap()).into(),
                    Some(b"$".to_vec()).into(),
                    None::<Json>.into(),
                ],
                Some(r#"[{}, null]"#.parse().unwrap()),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": 1, "b": [2, 3], "c": 4}"#).unwrap()).into(),
                    None::<Bytes>.into(),
                    None::<Json>.into(),
                ],
                None::<Json>,
            ),
            // Following tests come from MySQL doc.
            (
                vec![
                    Some(Json::from_str(r#"["a", ["b", "c"], "d"]"#).unwrap()).into(),
                    Some(b"$[1]".to_vec()).into(),
                    Some(Json::from_u64(1).unwrap()).into(),
                ],
                Some(r#"["a", ["b", "c", 1], "d"]"#.parse().unwrap()),
            ),
            (
                vec![
                    Some(Json::from_str(r#"["a", ["b", "c"], "d"]"#).unwrap()).into(),
                    Some(b"$[0]".to_vec()).into(),
                    Some(Json::from_u64(2).unwrap()).into(),
                ],
                Some(r#"[["a", 2], ["b", "c"], "d"]"#.parse().unwrap()),
            ),
            (
                vec![
                    Some(Json::from_str(r#"["a", ["b", "c"], "d"]"#).unwrap()).into(),
                    Some(b"$[1][0]".to_vec()).into(),
                    Some(Json::from_u64(3).unwrap()).into(),
                ],
                Some(r#"["a", [["b", 3], "c"], "d"]"#.parse().unwrap()),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": 1, "b": [2, 3], "c": 4}"#).unwrap()).into(),
                    Some(b"$.b".to_vec()).into(),
                    Some(Json::from_str(r#""x""#).unwrap()).into(),
                ],
                Some(r#"{"a": 1, "b": [2, 3, "x"], "c": 4}"#.parse().unwrap()),
            ),
            (
                vec![
                    Some(Json::from_str(r#"{"a": 1, "b": [2, 3], "c": 4}"#).unwrap()).into(),
                    Some(b"$.c".to_vec()).into(),
                    Some(Json::from_str(r#""y""#).unwrap()).into(),
                ],
                Some(r#"{"a": 1, "b": [2, 3], "c": [4, "y"]}"#.parse().unwrap()),
            ),
            // Following tests come from MySQL test.
            (
                vec![
                    Some(Json::from_str(r#"[1,2,3, {"a":[4,5,6]}]"#).unwrap()).into(),
                    Some(b"$".to_vec()).into(),
                    Some(Json::from_u64(7).unwrap()).into(),
                ],
                Some(r#"[1, 2, 3, {"a": [4, 5, 6]}, 7]"#.parse().unwrap()),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[1,2,3, {"a":[4,5,6]}]"#).unwrap()).into(),
                    Some(b"$".to_vec()).into(),
                    Some(Json::from_u64(7).unwrap()).into(),
                    Some(b"$[3].a".to_vec()).into(),
                    Some(Json::from_f64(3.15).unwrap()).into(),
                ],
                Some(r#"[1, 2, 3, {"a": [4, 5, 6, 3.15]}, 7]"#.parse().unwrap()),
            ),
            (
                vec![
                    Some(Json::from_str(r#"[1,2,3, {"a":[4,5,6]}]"#).unwrap()).into(),
                    Some(b"$".to_vec()).into(),
                    Some(Json::from_u64(7).unwrap()).into(),
                    Some(b"$[3].b".to_vec()).into(),
                    Some(Json::from_u64(8).unwrap()).into(),
                ],
                Some(r#"[1, 2, 3, {"a": [4, 5, 6]}, 7]"#.parse().unwrap()),
            ),
        ];
        for (args, expect_output) in cases {
            let output: Option<Json> = RpnFnScalarEvaluator::new()
                .push_params(args.clone())
                .evaluate(ScalarFuncSig::JsonArrayAppendSig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", args);
        }
    }

    #[test]
    fn test_json_merge_patch() {
        let cases = vec![
            // RFC 7396 document: https://datatracker.ietf.org/doc/html/rfc7396
            // RFC 7396 Example Test Cases
            (
                vec![Some(r#"{"a":"b"}"#), Some(r#"{"a":"c"}"#)],
                Some(r#"{"a": "c"}"#),
            ),
            (
                vec![Some(r#"{"a":"b"}"#), Some(r#"{"b":"c"}"#)],
                Some(r#"{"a": "b","b": "c"}"#),
            ),
            (
                vec![Some(r#"{"a":"b"}"#), Some(r#"{"a":null}"#)],
                Some(r#"{}"#),
            ),
            (
                vec![Some(r#"{"a":"b", "b":"c"}"#), Some(r#"{"a":null}"#)],
                Some(r#"{"b": "c"}"#),
            ),
            (
                vec![Some(r#"{"a":["b"]}"#), Some(r#"{"a":"c"}"#)],
                Some(r#"{"a": "c"}"#),
            ),
            (
                vec![Some(r#"{"a":"c"}"#), Some(r#"{"a":["b"]}"#)],
                Some(r#"{"a": ["b"]}"#),
            ),
            (
                vec![
                    Some(r#"{"a":{"b":"c"}}"#),
                    Some(r#"{"a":{"b":"d","c":null}}"#),
                ],
                Some(r#"{"a": {"b": "d"}}"#),
            ),
            (
                vec![Some(r#"{"a":[{"b":"c"}]}"#), Some(r#"{"a": [1]}"#)],
                Some(r#"{"a": [1]}"#),
            ),
            (
                vec![Some(r#"["a","b"]"#), Some(r#"["c","d"]"#)],
                Some(r#"["c", "d"]"#),
            ),
            (
                vec![Some(r#"{"a":"b"}"#), Some(r#"["c"]"#)],
                Some(r#"["c"]"#),
            ),
            (
                vec![Some(r#"{"a":"foo"}"#), Some(r#"null"#)],
                Some(r#"null"#),
            ),
            (
                vec![Some(r#"{"a":"foo"}"#), Some(r#""bar""#)],
                Some(r#""bar""#),
            ),
            (
                vec![Some(r#"{"e":null}"#), Some(r#"{"a":1}"#)],
                Some(r#"{"e": null,"a": 1}"#),
            ),
            (
                vec![Some(r#"[1,2]"#), Some(r#"{"a":"b","c":null}"#)],
                Some(r#"{"a":"b"}"#),
            ),
            (
                vec![Some(r#"{}"#), Some(r#"{"a":{"bb":{"ccc":null}}}"#)],
                Some(r#"{"a":{"bb": {}}}"#),
            ),
            // RFC 7396 Example Document
            (
                vec![
                    Some(
                        r#"{"title":"Goodbye!","author":{"givenName":"John","familyName":"Doe"},"tags":["example","sample"],"content":"This will be unchanged"}"#,
                    ),
                    Some(
                        r#"{"title":"Hello!","phoneNumber":"+01-123-456-7890","author":{"familyName":null},"tags":["example"]}"#,
                    ),
                ],
                Some(
                    r#"{"title":"Hello!","author":{"givenName":"John"},"tags":["example"],"content":"This will be unchanged","phoneNumber":"+01-123-456-7890"}"#,
                ),
            ),
            // From mysql Example Test Cases
            (
                vec![
                    None,
                    Some(r#"null"#),
                    Some(r#"[1,2,3]"#),
                    Some(r#"{"a":1}"#),
                ],
                Some(r#"{"a": 1}"#),
            ),
            (
                vec![
                    Some(r#"null"#),
                    None,
                    Some(r#"[1,2,3]"#),
                    Some(r#"{"a":1}"#),
                ],
                Some(r#"{"a": 1}"#),
            ),
            (
                vec![
                    Some(r#"null"#),
                    Some(r#"[1,2,3]"#),
                    None,
                    Some(r#"{"a":1}"#),
                ],
                None,
            ),
            (
                vec![
                    Some(r#"null"#),
                    Some(r#"[1,2,3]"#),
                    Some(r#"{"a":1}"#),
                    None,
                ],
                None,
            ),
            (
                vec![
                    None,
                    Some(r#"null"#),
                    Some(r#"{"a":1}"#),
                    Some(r#"[1,2,3]"#),
                ],
                Some(r#"[1,2,3]"#),
            ),
            (
                vec![
                    Some(r#"null"#),
                    None,
                    Some(r#"{"a":1}"#),
                    Some(r#"[1,2,3]"#),
                ],
                Some(r#"[1,2,3]"#),
            ),
            (
                vec![
                    Some(r#"null"#),
                    Some(r#"{"a":1}"#),
                    None,
                    Some(r#"[1,2,3]"#),
                ],
                Some(r#"[1,2,3]"#),
            ),
            (
                vec![
                    Some(r#"null"#),
                    Some(r#"{"a":1}"#),
                    Some(r#"[1,2,3]"#),
                    None,
                ],
                None,
            ),
            (
                vec![None, Some(r#"null"#), Some(r#"{"a":1}"#), Some(r#"true"#)],
                Some(r#"true"#),
            ),
            (
                vec![Some(r#"null"#), None, Some(r#"{"a":1}"#), Some(r#"true"#)],
                Some(r#"true"#),
            ),
            (
                vec![Some(r#"null"#), Some(r#"{"a":1}"#), None, Some(r#"true"#)],
                Some(r#"true"#),
            ),
            (
                vec![Some(r#"null"#), Some(r#"{"a":1}"#), Some(r#"true"#), None],
                None,
            ),
            // non-object last item
            (
                vec![
                    Some("true"),
                    Some("false"),
                    Some("[]"),
                    Some("{}"),
                    Some("null"),
                ],
                Some("null"),
            ),
            (
                vec![
                    Some("false"),
                    Some("[]"),
                    Some("{}"),
                    Some("null"),
                    Some("true"),
                ],
                Some("true"),
            ),
            (
                vec![
                    Some("true"),
                    Some("[]"),
                    Some("{}"),
                    Some("null"),
                    Some("false"),
                ],
                Some("false"),
            ),
            (
                vec![
                    Some("true"),
                    Some("false"),
                    Some("{}"),
                    Some("null"),
                    Some("[]"),
                ],
                Some("[]"),
            ),
            (
                vec![
                    Some("true"),
                    Some("false"),
                    Some("{}"),
                    Some("null"),
                    Some("1"),
                ],
                Some("1"),
            ),
            (
                vec![
                    Some("true"),
                    Some("false"),
                    Some("{}"),
                    Some("null"),
                    Some("1.8"),
                ],
                Some("1.8"),
            ),
            (
                vec![
                    Some("true"),
                    Some("false"),
                    Some("{}"),
                    Some("null"),
                    Some("112"),
                ],
                Some("112"),
            ),
            (vec![Some(r#"{"a":"foo"}"#), None], None),
            (vec![None, Some(r#"{"a":"foo"}"#)], None),
            (
                vec![Some(r#"{"a":"foo"}"#), Some(r#"false"#)],
                Some(r#"false"#),
            ),
            (vec![Some(r#"{"a":"foo"}"#), Some(r#"123"#)], Some(r#"123"#)),
            (
                vec![Some(r#"{"a":"foo"}"#), Some(r#"123.1"#)],
                Some(r#"123.1"#),
            ),
            (
                vec![Some(r#"{"a":"foo"}"#), Some(r#"[1,2,3]"#)],
                Some(r#"[1,2,3]"#),
            ),
            (
                vec![Some(r#"null"#), Some(r#"{"a":1}"#)],
                Some(r#"{"a":1}"#),
            ),
            (vec![Some(r#"{"a":1}"#), Some(r#"null"#)], Some(r#"null"#)),
            (
                vec![
                    Some(r#"{"a":"foo"}"#),
                    Some(r#"{"a":null}"#),
                    Some(r#"{"b":"123"}"#),
                    Some(r#"{"c":1}"#),
                ],
                Some(r#"{"b":"123","c":1}"#),
            ),
            (
                vec![
                    Some(r#"{"a":"foo"}"#),
                    Some(r#"{"a":null}"#),
                    Some(r#"{"c":1}"#),
                ],
                Some(r#"{"c":1}"#),
            ),
            (
                vec![
                    Some(r#"{"a":"foo"}"#),
                    Some(r#"{"a":null}"#),
                    Some(r#"true"#),
                ],
                Some(r#"true"#),
            ),
            (
                vec![
                    Some(r#"{"a":"foo"}"#),
                    Some(r#"{"d":1}"#),
                    Some(r#"{"a":{"bb":{"ccc":null}}}"#),
                ],
                Some(r#"{"a":{"bb":{}},"d":1}"#),
            ),
        ];

        for (vargs, expected) in cases {
            let vargs: Vec<Option<Json>> = vargs
                .into_iter()
                .map(|input| input.map(|s| Json::from_str(s).unwrap()))
                .collect::<Vec<_>>();
            let expected = expected.map(|s| Json::from_str(s).unwrap());

            let output = RpnFnScalarEvaluator::new()
                .push_params(vargs.clone())
                .evaluate(ScalarFuncSig::JsonMergePatchSig)
                .unwrap();
            assert_eq!(output, expected, "{:?}", vargs);
        }
    }
}
