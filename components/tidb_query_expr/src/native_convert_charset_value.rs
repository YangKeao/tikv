// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Actual borrowed charset inputs and owned computed bytes, not report frames.
use tidb_query_datatype::codec::collation::native_encoding::{
    SharedNativeEncoding, TransformOp, find_encoding, is_supported_encoding,
};

/// Actual conversion outcome; collation presentation remains a late caller
/// read.
#[derive(Debug, Eq, PartialEq)]
pub enum NativeCharsetValue {
    /// Actual bytes, including the empty typed-reader result.
    Bytes(Vec<u8>),
    /// Actual UTF-8 bytes to publish with the target's current default
    /// collation.
    RetagString(Vec<u8>),
    /// Actual source NULL shortcut or binary-to-character conversion failure.
    Null,
    /// The original strict transform encountered invalid/unrepresentable input.
    InvalidCharacter,
    /// The exact target is outside the seven supported native names.
    UnknownCharset,
}
/// Metadata contract errors remain distinct from explicit finite resource
/// limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCharsetValueError {
    /// Effective source name must be an actual canonical supported field
    /// charset.
    Invalid,
    /// Finite actual retained/codec work allowance refused the operation.
    ResourceLimit(&'static str),
}
fn check(bytes: usize, maximum: usize) -> Result<(), NativeCharsetValueError> {
    if maximum == usize::MAX || bytes > maximum {
        Err(NativeCharsetValueError::ResourceLimit(
            "charset actual byte/work allowance exceeded",
        ))
    } else {
        Ok(())
    }
}
fn copy(bytes: &[u8], maximum: usize) -> Result<Vec<u8>, NativeCharsetValueError> {
    check(bytes.len(), maximum)?;
    let mut value = Vec::new();
    value
        .try_reserve_exact(bytes.len())
        .map_err(|_| NativeCharsetValueError::ResourceLimit("charset byte allocation failed"))?;
    check(value.capacity(), maximum)?;
    value.extend_from_slice(bytes);
    Ok(value)
}
fn transformed(
    bytes: &[u8],
    encoding: SharedNativeEncoding,
    operation: TransformOp,
    maximum: usize,
) -> Result<(Vec<u8>, bool), NativeCharsetValueError> {
    if matches!(
        encoding,
        SharedNativeEncoding::Binary | SharedNativeEncoding::Latin1
    ) {
        return copy(bytes, maximum).map(|value| (value, false));
    }
    // Only GB decoding can expand. This includes the codec Vec's geometric
    // capacity (at most twice the <=4-byte output of each consumed group).
    let work = if matches!(
        encoding,
        SharedNativeEncoding::Gbk | SharedNativeEncoding::Gb18030
    ) {
        bytes
            .len()
            .checked_mul(8)
            .ok_or(NativeCharsetValueError::ResourceLimit(
                "charset GB extent overflow",
            ))?
    } else {
        bytes.len()
    };
    check(work, maximum)?;
    let (value, error) = encoding.transform(bytes, operation, |_| ());
    check(value.capacity(), maximum)?;
    Ok((value, error.is_some()))
}
pub(crate) fn binary(
    value: Option<&[u8]>,
    charset: &str,
    operation: TransformOp,
    maximum: usize,
) -> Result<NativeCharsetValue, NativeCharsetValueError> {
    let (bytes, error) = transformed(
        value.unwrap_or_default(),
        find_encoding(charset),
        operation,
        maximum,
    )?;
    Ok(if error {
        NativeCharsetValue::InvalidCharacter
    } else {
        NativeCharsetValue::Bytes(bytes)
    })
}
/// Original strict UTF-8-to-encoded-byte conversion. Unknown EXACT names are
/// binary lookup fallback; typed-reader None means empty bytes, not source
/// NULL.
pub fn native_to_binary_value(
    value: Option<&[u8]>,
    charset: &str,
    maximum: usize,
) -> Result<NativeCharsetValue, NativeCharsetValueError> {
    binary(value, charset, TransformOp::ENCODE, maximum)
}
/// Original strict encoded-byte-to-UTF-8 conversion, retaining exact name
/// lookup.
pub fn native_from_binary_value(
    value: Option<&[u8]>,
    charset: &str,
    maximum: usize,
) -> Result<NativeCharsetValue, NativeCharsetValueError> {
    binary(value, charset, TransformOp::DECODE, maximum)
}
/// Original CONVERT USING with separate exact source spelling and effective
/// canonical source charset. Caller metadata validation precedes ETString read;
/// source NULL short-circuit is a different primitive below.
pub fn native_convert_using_value(
    value: Option<&[u8]>,
    source_exact: &str,
    source_effective: &str,
    target: &str,
    maximum: usize,
) -> Result<NativeCharsetValue, NativeCharsetValueError> {
    if !is_supported_encoding(source_effective) {
        return Err(NativeCharsetValueError::Invalid);
    }
    if !is_supported_encoding(target) {
        return Ok(NativeCharsetValue::UnknownCharset);
    }
    let bytes = value.unwrap_or_default();
    if source_effective == "binary" && target != "binary" {
        let (decoded, error) = transformed(
            bytes,
            find_encoding(target),
            TransformOp::DECODE_REPLACE,
            maximum,
        )?;
        return Ok(if error {
            NativeCharsetValue::Null
        } else {
            NativeCharsetValue::Bytes(decoded)
        });
    }
    if target == "binary" {
        return native_to_binary_value(value, source_exact, maximum);
    }
    check(bytes.len(), maximum)?;
    let encoding = find_encoding(target);
    if encoding.is_valid(bytes) {
        return copy(bytes, maximum).map(NativeCharsetValue::RetagString);
    }
    let (replaced, _) = transformed(bytes, encoding, TransformOp::REPLACE_NO_ERR, maximum)?;
    Ok(NativeCharsetValue::RetagString(replaced))
}
/// Called only after observing an actual SQL NULL source. It must never be
/// confused with nullable ETString reader None, which codecs treat as empty.
pub const fn native_charset_null_value() -> NativeCharsetValue {
    NativeCharsetValue::Null
}

/// Actual native SQL call, separate from nullable ETString codec input.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum NativeCharsetOperation {
    /// Encode using the source field's exact charset.
    ToBinary,
    /// Decode using the result field's exact charset.
    FromBinary,
    /// Source, then dynamic target, then original cast/context/conversion.
    ConvertUsing,
}
impl NativeCharsetOperation {
    /// Original SQL source count.
    pub const fn arity(self) -> usize {
        match self {
            Self::ConvertUsing => 2,
            _ => 1,
        }
    }
    /// Original scalar-function return-publication name.
    pub const fn sql_name(self) -> &'static str {
        match self {
            Self::ToBinary => "to_binary",
            Self::FromBinary => "from_binary",
            Self::ConvertUsing => "convert_using",
        }
    }
}
/// SDK source demand after observing the actual first datum's NULL presence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCharsetSourceStep {
    /// Actual SQL NULL: no target/coercion/connection read.
    Null,
    /// Read the actual dynamic target before any source string cast.
    Target,
    /// One-source encoding/decoding can compute now.
    Convert,
}
/// Consume actual source presence, never typed-reader None or a host-selected
/// conversion result. The SDK owns the NULL/target-demand policy.
pub const fn native_charset_after_source(
    operation: NativeCharsetOperation,
    source_is_null: bool,
) -> NativeCharsetSourceStep {
    if source_is_null {
        NativeCharsetSourceStep::Null
    } else {
        match operation {
            NativeCharsetOperation::ConvertUsing => NativeCharsetSourceStep::Target,
            _ => NativeCharsetSourceStep::Convert,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn values_keep_exact_lookup_reader_none_and_null_distinct() {
        assert_eq!(native_charset_null_value(), NativeCharsetValue::Null);
        for encode in [true, false] {
            let result = if encode {
                native_to_binary_value(None, "gbk", 0)
            } else {
                native_from_binary_value(None, "gbk", 0)
            };
            assert_eq!(result.unwrap(), NativeCharsetValue::Bytes(vec![]));
        }
        assert_eq!(
            native_convert_using_value(None, "utf8mb4", "utf8mb4", "gbk", 0).unwrap(),
            NativeCharsetValue::RetagString(vec![])
        );
        assert_eq!(
            native_convert_using_value(None, "binary", "binary", "gbk", 0).unwrap(),
            NativeCharsetValue::Bytes(vec![])
        );
        assert_eq!(
            native_convert_using_value(Some("一".as_bytes()), "GBK", "gbk", "binary", 64).unwrap(),
            NativeCharsetValue::Bytes("一".as_bytes().to_vec())
        );
        assert_eq!(
            native_convert_using_value(Some("一".as_bytes()), "gbk", "gbk", "binary", 64).unwrap(),
            NativeCharsetValue::Bytes(vec![0xd2, 0xbb])
        );
        assert_eq!(
            native_to_binary_value(Some(&[255]), "UNKNOWN", 1).unwrap(),
            NativeCharsetValue::Bytes(vec![255])
        );
        assert_eq!(
            native_from_binary_value(Some(&[255]), "utf8mb4", 8).unwrap(),
            NativeCharsetValue::InvalidCharacter
        );
        assert_eq!(
            native_convert_using_value(Some(&[255]), "binary", "binary", "utf8mb4", 8).unwrap(),
            NativeCharsetValue::Null
        );
        assert_eq!(
            native_convert_using_value(Some("😂".as_bytes()), "utf8mb4", "utf8mb4", "gbk", 64)
                .unwrap(),
            NativeCharsetValue::RetagString(b"?".to_vec())
        );
    }
    #[test]
    fn values_bound_actual_work_without_a_report_header_or_skipped_copy() {
        assert_eq!(
            native_to_binary_value(Some(b"abc"), "binary", 3).unwrap(),
            NativeCharsetValue::Bytes(b"abc".to_vec())
        );
        assert!(matches!(
            native_to_binary_value(Some(b"abc"), "binary", 2),
            Err(NativeCharsetValueError::ResourceLimit(_))
        ));
        assert!(matches!(
            native_from_binary_value(Some(&[0xd2, 0xbb]), "gbk", 1),
            Err(NativeCharsetValueError::ResourceLimit(_))
        ));
        let NativeCharsetValue::Bytes(bytes) =
            native_from_binary_value(Some(&[0xd2, 0xbb]), "gbk", 16).unwrap()
        else {
            panic!("GB bytes")
        };
        assert_eq!(bytes, "一".as_bytes());
        assert!(bytes.capacity() <= 16);
        let big = vec![b'A'; 16384];
        assert_eq!(
            native_convert_using_value(Some(&big), "binary", "binary", "GBK", 0).unwrap(),
            NativeCharsetValue::UnknownCharset
        );
        assert_eq!(
            native_convert_using_value(Some(b"a"), "unknown", "unknown", "utf8mb4", 8),
            Err(NativeCharsetValueError::Invalid)
        );
        assert!(matches!(
            native_to_binary_value(None, "binary", usize::MAX),
            Err(NativeCharsetValueError::ResourceLimit(_))
        ));
    }
    #[test]
    fn source_policy_skips_target_only_after_actual_source_null() {
        for operation in [
            NativeCharsetOperation::ToBinary,
            NativeCharsetOperation::FromBinary,
            NativeCharsetOperation::ConvertUsing,
        ] {
            assert_eq!(
                native_charset_after_source(operation, true),
                NativeCharsetSourceStep::Null
            );
            assert_eq!(
                native_charset_after_source(operation, false),
                if operation == NativeCharsetOperation::ConvertUsing {
                    NativeCharsetSourceStep::Target
                } else {
                    NativeCharsetSourceStep::Convert
                }
            );
            assert_eq!(
                operation.arity(),
                if operation == NativeCharsetOperation::ConvertUsing {
                    2
                } else {
                    1
                }
            );
        }
    }
}
