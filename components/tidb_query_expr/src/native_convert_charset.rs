// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native charset runtime decisions over the shared encoding policy. Reports
//! contain computed bytes, never collation metadata: RetagString's target
//! default collation must still be observed by the caller AFTER this worker
//! completes.

use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::codec::{
    collation::native_encoding::{TransformOp, find_encoding, is_supported_encoding},
    data_type::{Bytes, BytesRef, ChunkedVec, EvaluableRef, EvaluableRet},
};

use crate::NativeIdentityFrameError;

type FrameResult<T> = std::result::Result<T, NativeIdentityFrameError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeConvertCharsetResult<'a> {
    Bytes(&'a [u8]),
    RetagString(&'a [u8]),
    InvalidCharacter,
    UnknownCharset,
}

/// Payload bytes need not be UTF-8. Only the two error reports have fixed
/// width.
pub fn decode_native_convert_charset_result(
    bytes: &[u8],
) -> Option<NativeConvertCharsetResult<'_>> {
    match bytes {
        [0, payload @ ..] => Some(NativeConvertCharsetResult::Bytes(payload)),
        [1, payload @ ..] => Some(NativeConvertCharsetResult::RetagString(payload)),
        [2] => Some(NativeConvertCharsetResult::InvalidCharacter),
        [3] => Some(NativeConvertCharsetResult::UnknownCharset),
        _ => None,
    }
}

fn name(value: Option<&[u8]>) -> Option<&str> {
    value.and_then(|value| std::str::from_utf8(value).ok())
}

pub fn to_binary_native_args_valid(_value: Option<&[u8]>, charset: Option<&[u8]>) -> bool {
    name(charset).is_some()
}

pub fn from_binary_native_args_valid(value: Option<&[u8]>, charset: Option<&[u8]>) -> bool {
    to_binary_native_args_valid(value, charset)
}

pub fn convert_using_native_args_valid(
    _value: Option<&[u8]>,
    source_exact: Option<&[u8]>,
    source_effective: Option<&[u8]>,
    target: Option<&[u8]>,
) -> bool {
    name(source_exact).is_some()
        && name(source_effective).is_some_and(is_supported_encoding)
        && name(target).is_some()
}

/// Conservative retained-report bound, NOT a codec temporary-allocation or peak
/// memory claim. GB encoding's per-group output buffer is eight bytes; every
/// visited group consumes at least one source byte. Native decode is smaller.
/// The common ready path charges this before invoking the same pure producers.
pub(crate) fn native_convert_charset_output_bound(value: Option<&[u8]>) -> Option<usize> {
    value.map_or(0, <[u8]>::len).checked_mul(8)?.checked_add(1)
}

fn encode_report(result: NativeConvertCharsetResult<'_>, bound: usize) -> FrameResult<Vec<u8>> {
    let (tag, payload) = match result {
        NativeConvertCharsetResult::Bytes(bytes) => (0, bytes),
        NativeConvertCharsetResult::RetagString(bytes) => (1, bytes),
        NativeConvertCharsetResult::InvalidCharacter => (2, &[][..]),
        NativeConvertCharsetResult::UnknownCharset => (3, &[][..]),
    };
    let length = payload
        .len()
        .checked_add(1)
        .ok_or(NativeIdentityFrameError::Capacity)?;
    if length > bound {
        return Err(NativeIdentityFrameError::Capacity);
    }
    // Do not insert a header into a codec Vec whose grown capacity is unrelated
    // to the logical result. The retained report has its own exact reservation.
    let mut report = Vec::new();
    report
        .try_reserve_exact(length)
        .map_err(|_| NativeIdentityFrameError::Capacity)?;
    if report.capacity() > bound {
        return Err(NativeIdentityFrameError::Capacity);
    }
    report.push(tag);
    report.extend_from_slice(payload);
    Ok(report)
}

fn binary_transform(
    value: Option<&[u8]>,
    charset: Option<&[u8]>,
    operation: TransformOp,
) -> FrameResult<Option<Vec<u8>>> {
    let charset = name(charset).ok_or(NativeIdentityFrameError::Invalid)?;
    let bound =
        native_convert_charset_output_bound(value).ok_or(NativeIdentityFrameError::Capacity)?;
    let (bytes, error) =
        find_encoding(charset).transform(value.unwrap_or_default(), operation, |_| ());
    let result = if error.is_some() {
        NativeConvertCharsetResult::InvalidCharacter
    } else {
        NativeConvertCharsetResult::Bytes(&bytes)
    };
    encode_report(result, bound).map(Some)
}

pub(crate) fn evaluate_to_binary_native(
    value: Option<&[u8]>,
    charset: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    binary_transform(value, charset, TransformOp::ENCODE)
}

pub(crate) fn evaluate_from_binary_native(
    value: Option<&[u8]>,
    charset: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    binary_transform(value, charset, TransformOp::DECODE)
}

pub(crate) fn evaluate_convert_using_native(
    value: Option<&[u8]>,
    source_exact: Option<&[u8]>,
    source_effective: Option<&[u8]>,
    target: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    if !convert_using_native_args_valid(value, source_exact, source_effective, target) {
        return Err(NativeIdentityFrameError::Invalid);
    }
    let target = name(target).expect("validated target charset");
    if !is_supported_encoding(target) {
        return encode_report(NativeConvertCharsetResult::UnknownCharset, 1).map(Some);
    }
    let bound =
        native_convert_charset_output_bound(value).ok_or(NativeIdentityFrameError::Capacity)?;
    let bytes = value.unwrap_or_default();
    let source_is_binary = name(source_effective) == Some("binary");
    let target_is_binary = target == "binary";
    if source_is_binary && !target_is_binary {
        let (decoded, error) =
            find_encoding(target).transform(bytes, TransformOp::DECODE_REPLACE, |_| ());
        return if error.is_some() {
            Ok(None)
        } else {
            encode_report(NativeConvertCharsetResult::Bytes(&decoded), bound).map(Some)
        };
    }
    if target_is_binary {
        // Original to_binary called pure eval_string a second time. The caller
        // may reuse its first computed byte value; no context/getters/warnings
        // are involved. Encoding still uses the EXACT source name, not effective.
        return evaluate_to_binary_native(value, source_exact);
    }
    let encoding = find_encoding(target);
    if encoding.is_valid(bytes) {
        return encode_report(NativeConvertCharsetResult::RetagString(bytes), bound).map(Some);
    }
    let (replaced, _) = encoding.transform(bytes, TransformOp::REPLACE_NO_ERR, |_| ());
    encode_report(NativeConvertCharsetResult::RetagString(&replaced), bound).map(Some)
}

fn transport_error(error: NativeIdentityFrameError) -> tidb_query_common::Error {
    other_err!("Invalid native charset conversion transport: {:?}", error)
}

#[rpn_fn(nullable)]
fn to_binary_native(value: Option<BytesRef>, charset: Option<BytesRef>) -> Result<Option<Bytes>> {
    evaluate_to_binary_native(value, charset).map_err(transport_error)
}

#[rpn_fn(nullable)]
fn from_binary_native(value: Option<BytesRef>, charset: Option<BytesRef>) -> Result<Option<Bytes>> {
    evaluate_from_binary_native(value, charset).map_err(transport_error)
}

#[rpn_fn(nullable)]
fn convert_using_native(
    value: Option<BytesRef>,
    source_exact: Option<BytesRef>,
    source_effective: Option<BytesRef>,
    target: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    evaluate_convert_using_native(value, source_exact, source_effective, target)
        .map_err(transport_error)
}

#[cfg(test)]
mod tests {
    use tidb_query_datatype::codec::collation::native_encoding::{
        SharedNativeEncoding, find_encoding_take_utf8_as_noop,
    };

    use super::*;

    #[test]
    fn charset_reports_keep_exact_names_effective_metadata_and_error_null_boundaries() {
        let report = |value: Option<Vec<u8>>| value.expect("non-NULL report");
        assert_eq!(find_encoding("utf8"), SharedNativeEncoding::Utf8);
        assert_eq!(
            find_encoding_take_utf8_as_noop("utf8"),
            SharedNativeEncoding::Binary
        );
        assert_eq!(find_encoding("GBK"), SharedNativeEncoding::Binary);
        assert!(!is_supported_encoding("utf8mb3"));
        assert_eq!(
            report(evaluate_to_binary_native(None, Some(b"gbk")).unwrap()),
            vec![0]
        );
        assert_eq!(
            report(evaluate_from_binary_native(None, Some(b"gbk")).unwrap()),
            vec![0]
        );
        assert_eq!(
            report(evaluate_to_binary_native(Some("一".as_bytes()), Some(b"gbk")).unwrap()),
            b"\0\xd2\xbb"
        );
        assert_eq!(
            report(evaluate_from_binary_native(Some(b"\xd2\xbb"), Some(b"gbk")).unwrap()),
            [b"\0".as_slice(), "一".as_bytes()].concat()
        );
        assert_eq!(
            report(evaluate_to_binary_native(Some("😂".as_bytes()), Some(b"gbk")).unwrap()),
            vec![2]
        );
        assert_eq!(
            report(evaluate_from_binary_native(Some(b"\xff"), Some(b"utf8mb4")).unwrap()),
            vec![2]
        );
        assert_eq!(
            report(evaluate_to_binary_native(Some(b"\xff"), Some(b"UNKNOWN")).unwrap()),
            vec![0, 255]
        );
        assert_eq!(
            report(
                evaluate_convert_using_native(
                    None,
                    Some(b"binary"),
                    Some(b"binary"),
                    Some(b"UTF8")
                )
                .unwrap()
            ),
            vec![3]
        );
        assert_eq!(
            evaluate_convert_using_native(
                Some(b"\xff"),
                Some(b"unknown"),
                Some(b"binary"),
                Some(b"utf8mb4")
            )
            .unwrap(),
            None
        );
        // Unknown exact source + binary fallback is legitimate FieldType data.
        assert_eq!(
            report(
                evaluate_convert_using_native(
                    Some(b"\xd2\xbb"),
                    Some(b"unknown"),
                    Some(b"binary"),
                    Some(b"gbk")
                )
                .unwrap()
            ),
            [b"\0".as_slice(), "一".as_bytes()].concat()
        );
        // Effective gbk must not replace the exact upper-case lookup spelling.
        assert_eq!(
            report(
                evaluate_convert_using_native(
                    Some("一".as_bytes()),
                    Some(b"GBK"),
                    Some(b"gbk"),
                    Some(b"binary")
                )
                .unwrap()
            ),
            [b"\0".as_slice(), "一".as_bytes()].concat()
        );
        assert_eq!(
            report(
                evaluate_convert_using_native(
                    Some("😂".as_bytes()),
                    Some(b"utf8mb4"),
                    Some(b"utf8mb4"),
                    Some(b"gbk")
                )
                .unwrap()
            ),
            b"\x01?"
        );
        let retained = report(
            evaluate_convert_using_native(
                Some(b"\xff"),
                Some(b"utf8mb4"),
                Some(b"utf8mb4"),
                Some(b"latin1"),
            )
            .unwrap(),
        );
        assert_eq!(
            decode_native_convert_charset_result(&retained),
            Some(NativeConvertCharsetResult::RetagString(&[255]))
        );
        assert_eq!(native_convert_charset_output_bound(Some(b"123")), Some(25));
        assert!(retained.capacity() <= native_convert_charset_output_bound(Some(b"\xff")).unwrap());
        assert!(to_binary_native_args_valid(None, Some(b"")));
        assert!(!to_binary_native_args_valid(None, Some(b"\xff")));
        assert!(!convert_using_native_args_valid(
            None,
            Some(b"unknown"),
            Some(b"unknown"),
            Some(b"utf8")
        ));
        for invalid in [b"".as_slice(), &[2, 0], &[3, 0], &[4]] {
            assert!(decode_native_convert_charset_result(invalid).is_none());
        }
        assert_eq!(
            decode_native_convert_charset_result(&[0]),
            Some(NativeConvertCharsetResult::Bytes(&[]))
        );
        assert_eq!(
            decode_native_convert_charset_result(&[1]),
            Some(NativeConvertCharsetResult::RetagString(&[]))
        );
    }
}
