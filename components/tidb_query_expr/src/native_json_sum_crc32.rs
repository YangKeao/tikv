// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! JSON_SUM_CRC32's frozen implemented serde scalar-array domain. This does not
//! implement or admit SQL ARRAY target casts, width/signedness conversions, or
//! new PB/legacy routes. The operand is a whole actual prepared JSON document,
//! not formatted members, a class flag, per-member checksums, or a computed
//! sum.

use serde_json::{Number, Value as Json};
use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::codec::{
    data_type::{Bytes, BytesRef, ChunkedVec, EvaluableRef, EvaluableRet},
    mysql::Decimal,
};

use crate::NativeIdentityFrameError;

type FrameResult<T> = std::result::Result<T, NativeIdentityFrameError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeJsonSumCrc32Result {
    Value(i64),
    RequiresArray,
    RequiresScalar,
    RequiresHomogeneous,
}

pub fn decode_native_json_sum_crc32_result(report: &[u8]) -> Option<NativeJsonSumCrc32Result> {
    match report {
        [0, payload @ ..] if payload.len() == 8 => Some(NativeJsonSumCrc32Result::Value(
            i64::from_le_bytes(payload.try_into().ok()?),
        )),
        [1] => Some(NativeJsonSumCrc32Result::RequiresArray),
        [2] => Some(NativeJsonSumCrc32Result::RequiresScalar),
        [3] => Some(NativeJsonSumCrc32Result::RequiresHomogeneous),
        _ => None,
    }
}

/// Structural serde transport validation only. Root/type/homogeneity failures
/// remain real worker results, not refusal predicates in the caller or planner.
pub fn json_sum_crc32_serde_native_args_valid(input: Option<&[u8]>) -> bool {
    input.map_or(true, |bytes| {
        crate::json_serde_native_args_valid(bytes, None, None)
    })
}

/// Fixed retained report bound. Serde parsing/number formatting allocations are
/// existing codec temporaries, not covered by a peak-allocation guarantee.
pub(crate) fn native_json_sum_crc32_output_bound(input: Option<&[u8]>) -> Option<usize> {
    Some(if input.is_some() { 9 } else { 0 })
}

fn format_number(number: &Number) -> String {
    if let Some(integer) = number.as_i64() {
        return integer.to_string();
    }
    if let Some(integer) = number.as_u64() {
        return integer.to_string();
    }
    let value = number
        .as_f64()
        .expect("serde JSON numbers are finite f64 here");
    Decimal::native_format_json_sum_float(value)
}

fn compute(document: Json) -> NativeJsonSumCrc32Result {
    let Json::Array(values) = document else {
        return NativeJsonSumCrc32Result::RequiresArray;
    };
    let mut saw_string = false;
    let mut saw_number = false;
    let mut sum = 0_i64;
    for value in values {
        let text = match value {
            Json::String(value) if !saw_number => {
                saw_string = true;
                value
            }
            Json::Number(value) if !saw_string => {
                saw_number = true;
                format_number(&value)
            }
            Json::Bool(_) | Json::Null | Json::Array(_) | Json::Object(_) => {
                return NativeJsonSumCrc32Result::RequiresScalar;
            }
            Json::String(_) | Json::Number(_) => {
                return NativeJsonSumCrc32Result::RequiresHomogeneous;
            }
        };
        // Shared IEEE implementation already used by the CRC32 builtin. Keep
        // the sum's explicit wrapping rather than saturating or checked addition.
        sum = sum.wrapping_add(i64::from(file_system::calc_crc32_bytes(text.as_bytes())));
    }
    NativeJsonSumCrc32Result::Value(sum)
}

fn encode_report(result: NativeJsonSumCrc32Result) -> FrameResult<Vec<u8>> {
    let (tag, sum) = match result {
        NativeJsonSumCrc32Result::Value(value) => (0, Some(value)),
        NativeJsonSumCrc32Result::RequiresArray => (1, None),
        NativeJsonSumCrc32Result::RequiresScalar => (2, None),
        NativeJsonSumCrc32Result::RequiresHomogeneous => (3, None),
    };
    let length = if sum.is_some() { 9 } else { 1 };
    let mut report = Vec::new();
    report
        .try_reserve_exact(length)
        .map_err(|_| NativeIdentityFrameError::Capacity)?;
    if report.capacity() > 9 {
        return Err(NativeIdentityFrameError::Capacity);
    }
    report.push(tag);
    if let Some(value) = sum {
        report.extend_from_slice(&value.to_le_bytes());
    }
    Ok(report)
}

pub(crate) fn evaluate_json_sum_crc32_serde_native(
    input: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let Some(input) = input else {
        return Ok(None);
    };
    // Same serde prepared-value decoder as the existing JSON profiles. The
    // expression crate's float_roundtrip feature preserves serialized f64 bits.
    let document: Json =
        serde_json::from_slice(input).map_err(|_| NativeIdentityFrameError::Invalid)?;
    encode_report(compute(document)).map(Some)
}

#[rpn_fn(nullable)]
fn json_sum_crc32_serde_native(input: Option<BytesRef>) -> Result<Option<Bytes>> {
    evaluate_json_sum_crc32_serde_native(input)
        .map_err(|error| other_err!("Invalid native JSON_SUM_CRC32 transport: {:?}", error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_checksum_keeps_number_spelling_first_error_null_and_fixed_report() {
        let run = |input: &[u8]| {
            let report = evaluate_json_sum_crc32_serde_native(Some(input))
                .unwrap()
                .unwrap();
            assert!(report.capacity() <= native_json_sum_crc32_output_bound(Some(input)).unwrap());
            decode_native_json_sum_crc32_result(&report).unwrap()
        };
        for (input, expected) in [
            (b"[-1,2,3]".as_slice(), 3_101_005_010),
            (b"[1,2,3]", 4_505_025_631),
            (br#"["a","b","c"]"#, 5_925_539_243),
            (b"[1.1,1,3.3]", 6_204_045_883),
            (b"[1.1,2.2,3.3]", 4_453_038_788),
            (b"[]", 0),
            (br#"["123456789"]"#, 0xcbf4_3926),
        ] {
            assert_eq!(run(input), NativeJsonSumCrc32Result::Value(expected));
        }
        for (input, spelling) in [
            (b"[-0.0]".as_slice(), b"0".as_slice()),
            (b"[1.0]", b"1"),
            (b"[1e6]", b"1e+06"),
            (b"[0.00001]", b"1e-05"),
            (b"[18446744073709551615]", b"18446744073709551615"),
        ] {
            assert_eq!(
                run(input),
                NativeJsonSumCrc32Result::Value(i64::from(file_system::calc_crc32_bytes(spelling)))
            );
        }
        for input in [b"null".as_slice(), b"1", b"{}", b"true"] {
            assert!(json_sum_crc32_serde_native_args_valid(Some(input)));
            assert_eq!(run(input), NativeJsonSumCrc32Result::RequiresArray);
        }
        assert_eq!(
            run(br#"[1,true,"x"]"#),
            NativeJsonSumCrc32Result::RequiresScalar
        );
        assert_eq!(
            run(br#"[1,"x",true]"#),
            NativeJsonSumCrc32Result::RequiresHomogeneous
        );
        assert_eq!(
            run(br#"["x",1]"#),
            NativeJsonSumCrc32Result::RequiresHomogeneous
        );
        for input in [b"[null]".as_slice(), b"[[1]]", b"[{}]"] {
            assert_eq!(run(input), NativeJsonSumCrc32Result::RequiresScalar);
        }
        assert_eq!(evaluate_json_sum_crc32_serde_native(None).unwrap(), None);
        assert_eq!(native_json_sum_crc32_output_bound(None), Some(0));
        assert!(json_sum_crc32_serde_native_args_valid(None));
        assert!(!json_sum_crc32_serde_native_args_valid(Some(b"[")));
        assert!(evaluate_json_sum_crc32_serde_native(Some(b"[")).is_err());
        for invalid in [
            b"".as_slice(),
            &[0],
            &[1, 0],
            &[2, 0],
            &[3, 0],
            &[4],
            &[0; 10],
        ] {
            assert!(decode_native_json_sum_crc32_result(invalid).is_none());
        }
        assert_eq!(
            json_sum_crc32_serde_native_fn_meta().name,
            "json_sum_crc32_serde_native"
        );
    }
}
