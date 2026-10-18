// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Actual JSON checksum documents and scalars, without serialization/report
//! frames.
use serde_json::{Number, Value as Json};
use tidb_query_datatype::codec::{
    mysql::Decimal, native_sql_string::NativeSqlStringInput as Input,
};

use crate::{NativeJsonCoercionError, NativeJsonSumCrc32Result as Value};

/// Original JSON admission errors stay separate from finite work refusal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeJsonSumCrc32ValueError {
    /// Original document admission, parsing or scalar conversion failure.
    Coercion(NativeJsonCoercionError),
    /// Actual demanded parse/display/number work exceeds a finite allowance.
    ResourceLimit(&'static str),
}
use NativeJsonSumCrc32ValueError as Error;
fn check(bytes: usize, maximum: usize) -> Result<(), Error> {
    if maximum == usize::MAX || bytes > maximum {
        Err(Error::ResourceLimit(
            "JSON checksum work allowance exceeded",
        ))
    } else {
        Ok(())
    }
}
fn number_text(number: &Number) -> String {
    if let Some(value) = number.as_i64() {
        return value.to_string();
    }
    if let Some(value) = number.as_u64() {
        return value.to_string();
    }
    Decimal::native_format_json_sum_float(
        number
            .as_f64()
            .expect("serde JSON numbers are finite f64 here"),
    )
}
// None is used only by the existing ordinary wire owner, whose original parser
// and report bound remain unchanged. Native value callers must supply a finite
// allowance. Strings are borrowed and never formatted or copied here.
pub(crate) fn compute(document: &Json, maximum: Option<usize>) -> Result<Value, Error> {
    let Json::Array(values) = document else {
        return Ok(Value::RequiresArray);
    };
    let (mut strings, mut numbers, mut sum) = (false, false, 0_i64);
    for value in values {
        let formatted;
        let text = match value {
            Json::String(value) if !numbers => {
                strings = true;
                value.as_str()
            }
            Json::Number(value) if !strings => {
                numbers = true;
                // Integer text is short. Float conversion FIRST renders fixed
                // decimal Display (up to 327 chars for signed subnormals), then
                // retains a second digits String and short Go-g output. Bound
                // these live temporary capacities BEFORE that conversion.
                if let Some(maximum) = maximum {
                    let work = if value.is_i64() || value.is_u64() {
                        128
                    } else {
                        4096
                    };
                    check(work, maximum)?;
                }
                formatted = number_text(value);
                if let Some(maximum) = maximum {
                    check(formatted.capacity(), maximum)?;
                }
                formatted.as_str()
            }
            Json::Bool(_) | Json::Null | Json::Array(_) | Json::Object(_) => {
                return Ok(Value::RequiresScalar);
            }
            Json::String(_) | Json::Number(_) => return Ok(Value::RequiresHomogeneous),
        };
        sum = sum.wrapping_add(i64::from(file_system::calc_crc32_bytes(text.as_bytes())));
    }
    Ok(Value::Value(sum))
}
/// Compute from the actual already-parsed document, borrowing all string bytes.
/// No host-provided class, formatted member, checksum or sum is accepted.
pub fn native_json_sum_crc32_document_value(
    document: &Json,
    maximum: usize,
) -> Result<Value, Error> {
    check(0, maximum)?;
    compute(document, Some(maximum))
}
/// Original ordinary-document admission and actual checksum, with demanded
/// display/parse work bounded before entering the existing parser. SQL NULL and
/// unsupported kinds do no parsing; invalid UTF-8 precedes parser work refusal.
pub fn native_json_sum_crc32_value(
    input: Input<'_>,
    maximum: usize,
) -> Result<Option<Value>, Error> {
    let extent = match input {
        Input::Null => return Ok(None),
        Input::String(bytes) | Input::Bytes(bytes) => {
            crate::native_json_sql_string(input).map_err(Error::Coercion)?;
            Some(bytes.len())
        }
        Input::Json { value, .. } => Some(value.len()),
        Input::Decimal(value) => Some(
            value
                .digits
                .len()
                .checked_add(value.scale.max(value.storage_scale) as usize)
                .and_then(|n| n.checked_add(4))
                .ok_or(Error::ResourceLimit(
                    "JSON checksum Decimal display extent overflow",
                ))?,
        ),
        Input::Int(_) | Input::UInt(_) | Input::Real(_) => None,
        // Original admission rejects these before any display or payload copy.
        _ => {
            return crate::native_parse_json_document_argument(input)
                .map(|_| unreachable!("unsupported ordinary JSON input admitted"))
                .map_err(Error::Coercion);
        }
    };
    let owned = match extent {
        Some(extent) => extent
            .checked_mul(128)
            .and_then(|n| n.checked_add(1024))
            .ok_or(Error::ResourceLimit(
                "JSON checksum document work extent overflow",
            ))?,
        None => 0,
    };
    check(owned, maximum)?;
    let document = crate::native_parse_json_document_argument(input).map_err(Error::Coercion)?;
    document
        .as_ref()
        .map(|document| native_json_sum_crc32_document_value(document, maximum - owned))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_documents_keep_first_error_null_and_number_spelling() {
        assert_eq!(native_json_sum_crc32_value(Input::Null, 0).unwrap(), None);
        for (text, expected) in [
            ("[]", Value::Value(0)),
            ("[1,2,3]", Value::Value(4505025631)),
            ("[1,true,\"x\"]", Value::RequiresScalar),
            ("[1,\"x\",true]", Value::RequiresHomogeneous),
            ("null", Value::RequiresArray),
        ] {
            assert_eq!(
                native_json_sum_crc32_value(Input::String(text.as_bytes()), 8192).unwrap(),
                Some(expected)
            );
        }
        for (text, spelling) in [
            ("[-0.0]", "0"),
            ("[1.0]", "1"),
            ("[1e6]", "1e+06"),
            ("[0.00001]", "1e-05"),
            ("[18446744073709551615]", "18446744073709551615"),
        ] {
            assert_eq!(
                native_json_sum_crc32_value(Input::String(text.as_bytes()), 8192).unwrap(),
                Some(Value::Value(i64::from(file_system::calc_crc32_bytes(
                    spelling.as_bytes()
                ))))
            );
        }
    }
    #[test]
    fn borrowed_strings_and_skipped_numeric_work_need_no_report_charge() {
        for (text, expected) in [
            ("[]", Value::Value(0)),
            ("[null,1]", Value::RequiresScalar),
            ("[\"a\",1]", Value::RequiresHomogeneous),
            (
                "[\"a\"]",
                Value::Value(i64::from(file_system::calc_crc32_bytes(b"a"))),
            ),
        ] {
            let doc: Json = serde_json::from_str(text).unwrap();
            assert_eq!(
                native_json_sum_crc32_document_value(&doc, 0).unwrap(),
                expected
            );
        }
        let doc: Json = serde_json::from_str("[1,null]").unwrap();
        assert!(matches!(
            native_json_sum_crc32_document_value(&doc, 0),
            Err(Error::ResourceLimit(_))
        ));
        let big = vec![b' '; 16384];
        assert!(matches!(
            native_json_sum_crc32_value(Input::Bytes(&big), 8192),
            Err(Error::ResourceLimit(_))
        ));
        let invalid = vec![255; 16384];
        assert!(matches!(
            native_json_sum_crc32_value(Input::Bytes(&invalid), 0),
            Err(Error::Coercion(NativeJsonCoercionError::Unsupported(
                "invalid UTF-8 string datum"
            )))
        ));
        assert!(matches!(
            native_json_sum_crc32_value(Input::Raw(&big), 0),
            Err(Error::Coercion(NativeJsonCoercionError::Unsupported(
                "JSON document requires JSON or string"
            )))
        ));
        for value in [f64::MAX, -f64::MAX, f64::from_bits(1), -f64::from_bits(1)] {
            assert!(
                value.to_string().len() > 128,
                "fixed Display intermediate really exceeds final Go-g length"
            );
            let doc = Json::Array(vec![Json::Number(Number::from_f64(value).unwrap())]);
            assert!(matches!(
                native_json_sum_crc32_document_value(&doc, 128),
                Err(Error::ResourceLimit(_))
            ));
            assert!(native_json_sum_crc32_document_value(&doc, 4096).is_ok());
        }
        let doc = Json::Array(vec![]);
        assert!(matches!(
            native_json_sum_crc32_document_value(&doc, usize::MAX),
            Err(Error::ResourceLimit(_))
        ));
    }
}
