// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Actual native compression values; finite work is separate from zlib
//! warnings.
use flate2::{Decompress, FlushDecompress, Status};

use crate::{impl_encryption as original, local::UncompressOutcome};
/// Closed native compression operations in the canonical host frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCompressionOperation {
    /// Original Go-compatible encoder and frame.
    Compress,
    /// Original strict zlib decoder and SQL dispositions.
    Uncompress,
    /// Original little-endian header inspection.
    Length,
}
impl NativeCompressionOperation {
    /// Original SQL spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Compress => "compress",
            Self::Uncompress => "uncompress",
            Self::Length => "uncompressed_length",
        }
    }
}
/// Actual work/copy refusal, distinct from a SQL decoder disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCompressionValueError {
    /// A demanded operation exceeds the finite allowance.
    Resource,
    /// An original owner returned an impossible result.
    Contract,
}
use NativeCompressionValueError as E;
fn check(n: usize, maximum: usize) -> Result<(), E> {
    if n > maximum {
        Err(E::Resource)
    } else {
        Ok(())
    }
}
/// Actual nullable native encoder over its original byte owner.
pub fn native_compress_actual_value(
    input: Option<&[u8]>,
    maximum: usize,
) -> Result<Option<Vec<u8>>, E> {
    let n = input.filter(|v| !v.is_empty()).map_or(Ok(0), |v| {
        v.len()
            .checked_mul(4)
            .and_then(|n| n.checked_add(1 << 20))
            .ok_or(E::Resource)
    })?;
    check(n, maximum)?;
    original::compress_go_native(input).map_err(|_| E::Contract)
}
/// Actual nullable header value and its original short-input warning fact.
pub fn native_uncompressed_length_actual_value(
    input: Option<&[u8]>,
    maximum: usize,
) -> Result<(Option<i64>, bool), E> {
    check(input.map_or(0, |v| v.len().min(5) * 16), maximum)?;
    Ok(input.map_or((None, false), |v| {
        let (n, short) = original::uncompressed_length_impl(v);
        (Some(n), short)
    }))
}
/// Internal inflater failures; SQL output overflow and finite work are
/// distinct.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InflateFailure {
    Decode,
    OutputLimit,
    Resource,
}
/// The original inflate loop with optional finite demanded-work accounting.
/// None preserves the explicit compatibility caller's original allocation
/// policy.
pub(crate) fn inflate_actual(
    data: &[u8],
    max_output: usize,
    maximum: Option<usize>,
) -> Result<Vec<u8>, InflateFailure> {
    let mut decoder = Decompress::new(true);
    let mut out = Vec::new();
    let mut input_offset = 0;
    let mut chunk = [0; 8 * 1024];
    if maximum.is_some_and(|m| m < 32768) {
        return Err(InflateFailure::Resource);
    }
    loop {
        let input_before = decoder.total_in();
        let output_before = decoder.total_out();
        let remaining = max_output.saturating_sub(out.len());
        let mut output_len = chunk.len().min(remaining.saturating_add(1));
        let left = data.len() - input_offset;
        let mut input_len = left;
        if let Some(maximum) = maximum {
            let base = input_offset
                .checked_mul(16)
                .and_then(|n| out.capacity().checked_mul(4).and_then(|v| n.checked_add(v)))
                .and_then(|n| n.checked_add(32768))
                .ok_or(InflateFailure::Resource)?;
            let available = maximum.checked_sub(base).ok_or(InflateFailure::Resource)?;
            input_len = input_len.min(available / 32);
            output_len = output_len.min(available / 32);
            if output_len == 0 || (left != 0 && input_len == 0) {
                return Err(InflateFailure::Resource);
            }
        }
        let status = decoder
            .decompress(
                &data[input_offset..input_offset + input_len],
                &mut chunk[..output_len],
                FlushDecompress::None,
            )
            .map_err(|_| InflateFailure::Decode)?;
        input_offset = usize::try_from(decoder.total_in()).map_err(|_| InflateFailure::Decode)?;
        let produced = usize::try_from(decoder.total_out() - output_before)
            .map_err(|_| InflateFailure::Decode)?;
        if produced > remaining {
            return Err(InflateFailure::OutputLimit);
        }
        if let Some(maximum) = maximum {
            let needed = out
                .len()
                .checked_add(produced)
                .ok_or(InflateFailure::Resource)?;
            let held = out.capacity().max(needed);
            let work = input_offset
                .checked_mul(16)
                .and_then(|n| held.checked_mul(4).and_then(|v| n.checked_add(v)))
                .and_then(|n| n.checked_add(32768))
                .ok_or(InflateFailure::Resource)?;
            if work > maximum {
                return Err(InflateFailure::Resource);
            }
            if needed > out.capacity() {
                out.try_reserve_exact(produced)
                    .map_err(|_| InflateFailure::Resource)?;
            }
        }
        out.extend_from_slice(&chunk[..produced]);
        if status == Status::StreamEnd {
            return Ok(out);
        }
        if decoder.total_in() == input_before && produced == 0 {
            return Err(if input_len < left {
                InflateFailure::Resource
            } else {
                InflateFailure::Decode
            });
        }
    }
}
/// Actual strict nullable UNCOMPRESS result with no private wire envelope.
pub fn native_uncompress_actual_value(
    input: Option<&[u8]>,
    maximum: usize,
) -> Result<UncompressOutcome, E> {
    check(0, maximum)?;
    uncompress_report(input, Some(maximum))
}
/// Shared report owner, optionally retaining the explicit wire path's unbounded
/// policy.
pub(crate) fn uncompress_report(
    input: Option<&[u8]>,
    maximum: Option<usize>,
) -> Result<UncompressOutcome, E> {
    let Some(payload) = input else {
        return Ok(UncompressOutcome::Null);
    };
    if payload.is_empty() {
        return Ok(UncompressOutcome::Value(Vec::new()));
    }
    if payload.len() <= 4 {
        return Ok(UncompressOutcome::Corrupt);
    }
    let length = u32::from_le_bytes(payload[..4].try_into().expect("checked header"));
    match inflate_actual(&payload[4..], length as usize, maximum) {
        Ok(bytes) if length < bytes.len() as u32 => Ok(UncompressOutcome::OutputLimit),
        Ok(bytes) => Ok(UncompressOutcome::Value(bytes)),
        Err(InflateFailure::Decode) => Ok(UncompressOutcome::Corrupt),
        Err(InflateFailure::OutputLimit) => Ok(UncompressOutcome::OutputLimit),
        Err(InflateFailure::Resource) => Err(E::Resource),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_compression_original_frames_and_decoder_dispositions() {
        let frame = vec![
            5, 0, 0, 0, 120, 156, 202, 72, 205, 201, 201, 7, 4, 0, 0, 255, 255, 6, 44, 2, 21,
        ];
        assert_eq!(
            native_compress_actual_value(Some(b"hello"), 2 << 20),
            Ok(Some(frame.clone()))
        );
        assert_eq!(
            native_uncompress_actual_value(Some(&frame), 128 << 10),
            Ok(UncompressOutcome::Value(b"hello".to_vec()))
        );
        let mut bad = frame.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(
            native_uncompress_actual_value(Some(&bad), 128 << 10),
            Ok(UncompressOutcome::Corrupt)
        );
        let mut short_output = frame.clone();
        short_output[..4].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            native_uncompress_actual_value(Some(&short_output), 128 << 10),
            Ok(UncompressOutcome::OutputLimit)
        );
        for input in [None, Some(b"".as_slice())] {
            let expected = input.map(|_| Vec::new());
            assert_eq!(native_compress_actual_value(input, 0), Ok(expected));
            assert!(matches!(
                native_uncompress_actual_value(input, 0),
                Ok(UncompressOutcome::Null | UncompressOutcome::Value(_))
            ));
        }
        for bytes in [b"x".as_slice(), b"1234".as_slice()] {
            assert_eq!(
                native_uncompressed_length_actual_value(Some(bytes), 128),
                Ok((Some(0), true))
            );
            assert_eq!(
                native_uncompress_actual_value(Some(bytes), 128),
                Ok(UncompressOutcome::Corrupt)
            );
        }
        assert_eq!(
            native_uncompressed_length_actual_value(None, 0),
            Ok((None, false))
        );
        assert_eq!(
            native_uncompressed_length_actual_value(Some(b""), 0),
            Ok((Some(0), false))
        );
        assert_eq!(
            native_uncompressed_length_actual_value(Some(&frame), 128),
            Ok((Some(5), false))
        );
    }
    #[test]
    fn actual_compression_finite_resources_and_undemanded_suffixes() {
        let mut frame = vec![
            5, 0, 0, 0, 120, 156, 202, 72, 205, 201, 201, 7, 4, 0, 0, 255, 255, 6, 44, 2, 21,
        ];
        frame.extend_from_slice(&vec![255; 1 << 20]);
        assert_eq!(
            native_uncompress_actual_value(Some(&frame), 128 << 10),
            Ok(UncompressOutcome::Value(b"hello".to_vec()))
        );
        assert_eq!(
            native_uncompressed_length_actual_value(Some(&frame), 128),
            Ok((Some(5), false))
        );
        let mut huge_header = frame.clone();
        huge_header[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            native_uncompress_actual_value(Some(&huge_header), 128 << 10),
            Ok(UncompressOutcome::Value(b"hello".to_vec()))
        );
        assert_eq!(
            native_uncompress_actual_value(Some(&vec![255; 1 << 20]), 128 << 10),
            Ok(UncompressOutcome::Corrupt)
        );
        let raw = vec![b'x'; 1 << 20];
        let bomb = native_compress_actual_value(Some(&raw), 8 << 20)
            .unwrap()
            .unwrap();
        assert_eq!(
            native_uncompress_actual_value(Some(&bomb), 128 << 10),
            Err(E::Resource)
        );
        assert_eq!(
            native_compress_actual_value(Some(&raw), 128 << 10),
            Err(E::Resource)
        );
        assert_eq!(
            native_compress_actual_value(None, usize::MAX),
            native_compress_actual_value(None, 1 << 30)
        );
        assert_eq!(
            native_uncompress_actual_value(None, usize::MAX),
            native_uncompress_actual_value(None, 1 << 30)
        );
        assert_eq!(
            native_uncompressed_length_actual_value(None, usize::MAX),
            native_uncompressed_length_actual_value(None, 1 << 30)
        );
    }
}
