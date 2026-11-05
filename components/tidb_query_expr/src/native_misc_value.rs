// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Actual UUID/hash values over the original pure owners, with finite work.
use tidb_query_common::error::{Error, ErrorInner, EvaluateError};
use tidb_query_datatype::codec::mysql::Decimal;

use crate::impl_miscellaneous as original;
/// Original misc SQL policy, independent of transport or input evaluation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeMiscOperation {
    /// UUID validation with the native whitespace policy.
    IsUuid,
    /// UUID version nibble.
    UuidVersion,
    /// Exact UUID timestamp Decimal.
    UuidTimestamp,
    /// Parse then optional forward field permutation.
    UuidToBin,
    /// Optional inverse permutation then format.
    BinToUuid,
    /// Native hashed low byte.
    TidbShard,
    /// Complete unsigned hashed bits.
    VitessHash,
}
impl NativeMiscOperation {
    /// Original lower-case SQL name.
    pub fn name(self) -> &'static str {
        match self {
            Self::IsUuid => "is_uuid",
            Self::UuidVersion => "uuid_version",
            Self::UuidTimestamp => "uuid_timestamp",
            Self::UuidToBin => "uuid_to_bin",
            Self::BinToUuid => "bin_to_uuid",
            Self::TidbShard => "tidb_shard",
            Self::VitessHash => "vitess_hash",
        }
    }
    /// Original fixed/optional argument shape.
    pub fn accepts_arity(self, n: usize) -> bool {
        match self {
            Self::UuidToBin | Self::BinToUuid => matches!(n, 1 | 2),
            _ => n == 1,
        }
    }
}
/// Owned genuine typed cause, distinct from finite work exhaustion.
#[derive(Debug, Eq, PartialEq)]
pub enum NativeMiscValueError {
    /// Finite work refused before processing.
    Resource,
    /// Original UUID_TO_BIN whitespace rejection.
    ToBinWhitespace,
    /// Original UUID_TO_BIN parse rejection.
    ToBinInvalid,
    /// Original UUID_VERSION parse rejection.
    VersionInvalid,
    /// Original UUID_TIMESTAMP parse rejection.
    TimestampInvalid,
    /// Original rejected binary input, owned by the pure kernel error.
    BinLength(Vec<u8>),
    /// A closed kernel returned an impossible shape/cause.
    Contract,
}
use NativeMiscValueError as E;
fn error(e: Error) -> E {
    match *e.0 {
        ErrorInner::Evaluate(EvaluateError::UuidToBinWhitespace) => E::ToBinWhitespace,
        ErrorInner::Evaluate(EvaluateError::UuidToBinInvalid) => E::ToBinInvalid,
        ErrorInner::Evaluate(EvaluateError::UuidVersionInvalid) => E::VersionInvalid,
        ErrorInner::Evaluate(EvaluateError::UuidTimestampInvalid) => E::TimestampInvalid,
        ErrorInner::Evaluate(EvaluateError::BinToUuidInvalidLength { input }) => {
            E::BinLength(input)
        }
        _ => E::Contract,
    }
}
/// Shared finite logical work gate, not a physical allocator-peak bound.
pub fn native_misc_work(maximum: usize, n: usize) -> Result<(), E> {
    if n > maximum {
        Err(E::Resource)
    } else {
        Ok(())
    }
}
fn text_work(input: Option<&[u8]>, maximum: usize, scan: bool) -> Result<(), E> {
    native_misc_work(maximum, 0)?;
    if let Some(input) = input {
        let n = if scan {
            input
                .len()
                .checked_mul(16)
                .and_then(|n| n.checked_add(1024))
                .ok_or(E::Resource)?
        } else {
            1024
        };
        native_misc_work(maximum, n)?;
    }
    Ok(())
}
/// Original nullable UUID validation, including lossy whitespace observation.
pub fn native_is_uuid_value(input: Option<&[u8]>, maximum: usize) -> Result<Option<i64>, E> {
    text_work(input, maximum, true)?;
    original::is_uuid_native(input).map_err(error)
}
/// Original nullable native UUID version; parsing only inspects accepted
/// lengths.
pub fn native_uuid_version_value(input: Option<&[u8]>, maximum: usize) -> Result<Option<i64>, E> {
    text_work(input, maximum, false)?;
    original::uuid_version_native(input).map_err(error)
}
/// Original exact six-place Decimal, without SQL rendering or frame encoding.
pub fn native_uuid_timestamp_value(
    input: Option<&[u8]>,
    maximum: usize,
) -> Result<Option<Decimal>, E> {
    text_work(input, maximum, false)?;
    original::uuid_timestamp_native(input).map_err(error)
}
/// Genuine parsed UUID bytes. NULL/parse failure precedes optional flag demand.
pub fn native_uuid_parse_value(
    input: Option<&[u8]>,
    maximum: usize,
) -> Result<Option<[u8; 16]>, E> {
    text_work(input, maximum, true)?;
    original::uuid_to_bin_parse_native(input)
        .map_err(error)?
        .map(|v| v.try_into().map_err(|_| E::Contract))
        .transpose()
}
/// Apply the original forward field permutation to the actual parsed UUID.
pub fn native_uuid_swap_value(input: &[u8; 16], flag: i64, maximum: usize) -> Result<Vec<u8>, E> {
    native_misc_work(maximum, 1024)?;
    original::uuid_to_bin_swap_native(Some(input), Some(&flag))
        .map_err(error)?
        .ok_or(E::Contract)
}
/// Original binary length error owns its actual payload; formatting is
/// unchanged.
pub fn native_bin_to_uuid_value(
    input: Option<&[u8]>,
    flag: i64,
    maximum: usize,
) -> Result<Option<Vec<u8>>, E> {
    text_work(input, maximum, true)?;
    original::bin_to_uuid_native(input, Some(&flag)).map_err(error)
}
/// Hash the actual nullable integer with the original crypto owner.
pub fn native_shard_value(
    input: Option<i64>,
    shard: bool,
    maximum: usize,
) -> Result<Option<u64>, E> {
    native_misc_work(maximum, if input.is_some() { 1024 } else { 0 })?;
    (if shard {
        original::get_native_tidb_shard(input.as_ref())
    } else {
        original::get_native_vitess_hash(input.as_ref())
    })
    .map(|v| v.map(|v| v as u64))
    .map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_misc_values_keep_uuid_layout_errors_precision_bits_and_finite_work() {
        let text = b"6ba7b810-9dad-11d1-80b4-00c04fd430c8";
        let bytes = [
            0x6b, 0xa7, 0xb8, 0x10, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4,
            0x30, 0xc8,
        ];
        assert_eq!(native_is_uuid_value(Some(text), 8192), Ok(Some(1)));
        assert_eq!(native_uuid_version_value(Some(text), 8192), Ok(Some(1)));
        let parsed = native_uuid_parse_value(Some(text), 8192).unwrap().unwrap();
        assert_eq!(parsed, bytes);
        let swapped = native_uuid_swap_value(&parsed, 1, 8192).unwrap();
        assert_eq!(
            swapped,
            [
                0x11, 0xd1, 0x9d, 0xad, 0x6b, 0xa7, 0xb8, 0x10, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4,
                0x30, 0xc8
            ]
        );
        assert_eq!(
            native_bin_to_uuid_value(Some(&swapped), 1, 8192),
            Ok(Some(text.to_vec()))
        );
        let wrapped = [&[255][..], &text[..], &[254][..]].concat();
        assert_eq!(native_is_uuid_value(Some(&wrapped), 8192), Ok(Some(1)));
        assert_eq!(
            native_uuid_parse_value(Some(&wrapped), 8192),
            Ok(Some(bytes))
        );
        assert_eq!(native_uuid_version_value(Some(&wrapped), 8192), Ok(Some(1)));
        assert_eq!(
            native_uuid_parse_value(Some(b" bad "), 8192),
            Err(E::ToBinWhitespace)
        );
        assert_eq!(
            native_uuid_parse_value(Some(b"bad"), 8192),
            Err(E::ToBinInvalid)
        );
        assert_eq!(
            native_uuid_version_value(Some(b"bad"), 8192),
            Err(E::VersionInvalid)
        );
        assert_eq!(
            native_uuid_timestamp_value(Some(b"bad"), 8192),
            Err(E::TimestampInvalid)
        );
        assert_eq!(
            native_bin_to_uuid_value(Some(&[255]), 0, 8192),
            Err(E::BinLength(vec![255]))
        );
        assert_eq!(
            native_uuid_timestamp_value(Some(b"00000000-0000-1000-8000-000000000000"), 8192)
                .unwrap()
                .unwrap()
                .to_string(),
            "-12219292800.000000"
        );
        assert_eq!(
            native_uuid_timestamp_value(Some(b"00000000-0000-4000-8000-000000000000"), 8192),
            Ok(None)
        );
        assert_eq!(
            native_shard_value(Some(0), false, 8192),
            Ok(Some(10_134_873_677_816_210_343))
        );
        assert_eq!(native_shard_value(Some(0), true, 8192), Ok(Some(167)));
        assert_eq!(
            native_shard_value(Some(-1), false, 8192),
            Ok(Some(0x355550B2150E2451))
        );
        assert_eq!(native_is_uuid_value(None, 0), Ok(None));
        assert_eq!(native_uuid_version_value(None, 0), Ok(None));
        assert_eq!(native_uuid_timestamp_value(None, 0), Ok(None));
        assert_eq!(native_uuid_parse_value(None, 0), Ok(None));
        assert_eq!(native_bin_to_uuid_value(None, 0, 0), Ok(None));
        assert_eq!(native_shard_value(None, false, 0), Ok(None));
        assert_eq!(
            native_is_uuid_value(None, usize::MAX),
            native_is_uuid_value(None, 1 << 30)
        );
        assert_eq!(native_uuid_parse_value(Some(text), 0), Err(E::Resource));
        assert_eq!(native_uuid_swap_value(&bytes, 0, 0), Err(E::Resource));
        assert_eq!(
            native_is_uuid_value(Some(&vec![255; 1 << 20]), 8192),
            Err(E::Resource)
        );
    }
}
