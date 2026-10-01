// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::{convert::TryFrom, io::Read};

use byteorder::{ByteOrder, LittleEndian};
use crypto::rand;
use flate2::{
    Compression, Decompress, FlushDecompress, Status,
    read::{ZlibDecoder, ZlibEncoder},
};
use openssl::hash::{self, MessageDigest};
use tidb_query_codegen::rpn_fn;
use tidb_query_common::{Result, error::EvaluateError};
use tidb_query_datatype::{
    codec::data_type::*,
    expr::{Error, EvalContext},
};

pub(crate) mod native_go_flate;

const SHA0: i64 = 0;
const SHA224: i64 = 224;
const SHA256: i64 = 256;
const SHA384: i64 = 384;
const SHA512: i64 = 512;

const MAX_RAND_BYTES_LENGTH: i64 = 1024;

#[rpn_fn(nullable)]
#[inline]
pub fn md5(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    match arg {
        Some(arg) => hex_digest(MessageDigest::md5(), arg).map(Some),
        None => Ok(None),
    }
}

#[rpn_fn(nullable)]
#[inline]
pub fn sha1(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    match arg {
        Some(arg) => hex_digest(MessageDigest::sha1(), arg).map(Some),
        None => Ok(None),
    }
}

#[rpn_fn(nullable, capture = [ctx])]
#[inline]
pub fn sha2(
    ctx: &mut EvalContext,
    input: Option<BytesRef>,
    hash_length: Option<&Int>,
) -> Result<Option<Bytes>> {
    match (input, hash_length) {
        (Some(input), Some(hash_length)) => {
            let result = sha2_impl(input, hash_length)?;
            if result.is_none() {
                ctx.warnings
                    .append_warning(Error::incorrect_parameters("sha2"));
            }
            Ok(result)
        }
        _ => Ok(None),
    }
}

#[rpn_fn]
#[inline]
fn sha2_native(input: BytesRef, hash_length: &Int) -> Result<Option<Bytes>> {
    sha2_impl(input, hash_length)
}

#[inline]
fn sha2_impl(input: BytesRef, hash_length: &Int) -> Result<Option<Bytes>> {
    let sha2 = match *hash_length {
        SHA0 | SHA256 => MessageDigest::sha256(),
        SHA224 => MessageDigest::sha224(),
        SHA384 => MessageDigest::sha384(),
        SHA512 => MessageDigest::sha512(),
        _ => return Ok(None),
    };
    hex_digest(sha2, input).map(Some)
}

#[inline]
fn compressed_length_prefix(original_len: u32) -> [u8; 4] {
    original_len.to_le_bytes()
}

#[inline]
fn compressed_needs_dot(bytes: &[u8]) -> bool {
    bytes.last().copied() == Some(b' ')
}

/// Native framing, also exposed narrowly for the retained native helper tests.
/// Wire compression keeps its own streaming writer and allocation order.
pub fn frame_compressed(original_len: u32, compressed: Vec<u8>) -> Vec<u8> {
    let append_suffix = compressed_needs_dot(&compressed);
    let mut framed = Vec::with_capacity(4 + compressed.len() + usize::from(append_suffix));
    framed.extend_from_slice(&compressed_length_prefix(original_len));
    framed.extend_from_slice(&compressed);
    if append_suffix {
        framed.push(b'.');
    }
    framed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InflateError {
    Decode,
    OutputLimit,
}

/// Strictly inflates the first zlib stream without allocating the length
/// prefix. Preserve the native progress, checksum and one-byte-over-limit
/// decisions.
pub fn inflate(data: &[u8], max_output: usize) -> std::result::Result<Vec<u8>, InflateError> {
    let mut decoder = Decompress::new(true);
    let mut out = Vec::new();
    let mut input_offset = 0;
    let mut chunk = [0; 8 * 1024];
    loop {
        let input_before = decoder.total_in();
        let output_before = decoder.total_out();
        let remaining = max_output.saturating_sub(out.len());
        // Give zlib one byte beyond the remaining budget so an over-limit
        // write is detected without ever appending bytes past the limit.
        let output_len = chunk.len().min(remaining.saturating_add(1));
        let status = decoder
            .decompress(
                &data[input_offset..],
                &mut chunk[..output_len],
                FlushDecompress::None,
            )
            .map_err(|_| InflateError::Decode)?;
        input_offset = usize::try_from(decoder.total_in()).map_err(|_| InflateError::Decode)?;
        let produced = usize::try_from(decoder.total_out() - output_before)
            .map_err(|_| InflateError::Decode)?;
        if produced > remaining {
            return Err(InflateError::OutputLimit);
        }
        out.extend_from_slice(&chunk[..produced]);
        if status == Status::StreamEnd {
            return Ok(out);
        }
        // `compress/zlib.NewReader` refuses a stream that ends before the
        // DEFLATE terminator and Adler-32 checksum.  The high-level flate2
        // reader can instead report a successful zero-byte read for that
        // truncated input, so require either stream completion or forward
        // progress toward it here.
        if decoder.total_in() == input_before && produced == 0 {
            return Err(InflateError::Decode);
        }
    }
}

#[rpn_fn(nullable)]
fn compress_go_native(input: Option<BytesRef>) -> Result<Option<Bytes>> {
    let Some(payload) = input else {
        return Ok(None);
    };
    if payload.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let compressed = native_go_flate::go_zlib_deflate(payload);
    Ok(Some(frame_compressed(payload.len() as u32, compressed)))
}

// Closed factory transport, not SQL bytes: NULL stays None; 0 prefixes the
// computed value (including empty), while exact one-byte 1/2 report actual
// corruption/output-limit dispositions. No EvalContext warning is emitted here.
#[rpn_fn(nullable)]
fn uncompress_native(input: Option<BytesRef>) -> Result<Option<Bytes>> {
    let Some(payload) = input else {
        return Ok(None);
    };
    if payload.is_empty() {
        return Ok(Some(vec![0]));
    }
    if payload.len() <= 4 {
        return Ok(Some(vec![1]));
    }
    let length = LittleEndian::read_u32(&payload[..4]);
    let mut bytes = match inflate(&payload[4..], length as usize) {
        Ok(bytes) => bytes,
        Err(InflateError::OutputLimit) => return Ok(Some(vec![2])),
        Err(InflateError::Decode) => return Ok(Some(vec![1])),
    };
    // Retain the original final declared-length check after successful inflate.
    if length < bytes.len() as u32 {
        return Ok(Some(vec![2]));
    }
    // Envelope allocation failure is a transport error, not a zlib disposition.
    bytes.try_reserve(1).map_err(|source| {
        other_err!("Unable to allocate UNCOMPRESS result envelope: {}", source)
    })?;
    bytes.insert(0, 0);
    Ok(Some(bytes))
}

#[rpn_fn(writer)]
#[inline]
pub fn compress(input: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
    // compress implements the `COMPRESS` built-in function.
    // MySQL doc: https://dev.mysql.com/doc/refman/5.6/en/encryption-functions.html#function_compress
    // according to MySQL doc: Empty strings are stored as empty strings.
    if input.is_empty() {
        return Ok(writer.write_ref(Some(b"")));
    }
    let mut e = ZlibEncoder::new(input, Compression::default());
    // preferred capacity is input length plus four bytes length header and one
    // extra end "." max capacity is isize::MAX, or will panic with
    // "capacity overflow"
    let mut vec = Vec::with_capacity((input.len() + 5).min(isize::MAX as usize));
    vec.resize(4, 0);
    vec[..4].copy_from_slice(&compressed_length_prefix(input.len() as u32));
    match e.read_to_end(&mut vec) {
        Ok(_) => {
            // according to MySQL doc: append "." if ends with space
            if compressed_needs_dot(&vec) {
                vec.push(b'.');
            }
            Ok(writer.write_ref(Some(vec.as_ref())))
        }
        _ => Ok(writer.write(None)),
    }
}

#[rpn_fn(writer, capture = [ctx])]
#[inline]
pub fn uncompress(
    ctx: &mut EvalContext,
    input: BytesRef,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    // uncompressed implements the `UNCOMPRESS` built-in function.
    // MySQL doc: https://dev.mysql.com/doc/refman/5.6/en/encryption-functions.html#function_uncompress
    // according to MySQL doc: Empty strings are stored as empty strings.
    if input.is_empty() {
        return Ok(writer.write_ref(Some(b"")));
    }
    if input.len() <= 4 {
        ctx.warnings.append_warning(Error::zlib_data_corrupted());
        return Ok(writer.write(None));
    }

    let len = LittleEndian::read_u32(&input[0..4]) as usize;
    let mut d = ZlibDecoder::new(&input[4..]);
    let mut vec = Vec::with_capacity(len);

    // - if the length of uncompressed string is greater than the length we read
    //   from the first four bytes, return null and generate a length corrupted
    //   warning.
    // - if the length of uncompressed string is zero or uncompress fail, return
    //   null and generate a data corrupted warning match d.read_to_end(&mut vec) {
    match d.read_to_end(&mut vec) {
        Ok(decoded_len) if len >= decoded_len && decoded_len != 0 => {
            Ok(writer.write_ref(Some(vec.as_ref())))
        }
        Ok(decoded_len) if len < decoded_len => {
            ctx.warnings.append_warning(Error::zlib_length_corrupted());
            Ok(writer.write(None))
        }
        _ => {
            ctx.warnings.append_warning(Error::zlib_data_corrupted());
            Ok(writer.write(None))
        }
    }
}

// https://dev.mysql.com/doc/refman/5.7/en/password-hashing.html
#[rpn_fn(nullable, capture = [ctx])]
#[inline]
pub fn password(ctx: &mut EvalContext, input: Option<BytesRef>) -> Result<Option<Bytes>> {
    ctx.warnings.append_warning(Error::Other(box_err!(
        "Warning: Deprecated syntax PASSWORD"
    )));
    match input {
        Some(bytes) => {
            if bytes.is_empty() {
                Ok(Some(Vec::new()))
            } else {
                let hash1 = hex_digest(MessageDigest::sha1(), bytes)?;
                let mut hash2 = hex_digest(MessageDigest::sha1(), hash1.as_slice())?;
                hash2.insert(0, b'*');
                Ok(Some(hash2))
            }
        }
        None => Ok(None),
    }
}

#[inline]
fn hex_digest(hashtype: MessageDigest, input: &[u8]) -> Result<Bytes> {
    hash::hash(hashtype, input)
        .map(|digest| hex::encode(digest).into_bytes())
        .map_err(|e| box_err!("OpenSSL error: {:?}", e))
}

#[rpn_fn(nullable, capture = [ctx])]
#[inline]
pub fn uncompressed_length(ctx: &mut EvalContext, arg: Option<BytesRef>) -> Result<Option<Int>> {
    Ok(arg.map(|s| {
        let (value, short) = uncompressed_length_impl(s);
        if short {
            ctx.warnings.append_warning(Error::zlib_data_corrupted());
        }
        value
    }))
}

#[rpn_fn]
#[inline]
fn uncompressed_length_native(arg: BytesRef) -> Result<Option<Int>> {
    Ok(Some(uncompressed_length_impl(arg).0))
}

#[inline]
fn uncompressed_length_impl(arg: BytesRef) -> (Int, bool) {
    if arg.is_empty() {
        (0, false)
    } else if arg.len() <= 4 {
        (0, true)
    } else {
        (Int::from(LittleEndian::read_u32(&arg[0..4])), false)
    }
}

#[rpn_fn(nullable, capture = [ctx])]
#[inline]
pub fn random_bytes(_ctx: &mut EvalContext, arg: Option<&Int>) -> Result<Option<Bytes>> {
    match arg {
        Some(arg) => {
            if *arg < 1 || *arg > MAX_RAND_BYTES_LENGTH {
                return Err(Error::overflow("length", "random_bytes").into());
            }
            let len = *arg as usize;
            let mut rand_bytes = vec![0; len];
            rand::rand_bytes(&mut rand_bytes).map_err(|_| {
                EvaluateError::Other("SSL library can't generate random bytes".to_owned())
            })?;
            Ok(Some(rand_bytes))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use tipb::ScalarFuncSig;

    use super::*;
    use crate::types::test_util::RpnFnScalarEvaluator;

    #[test]
    fn test_compress_go_native_nullable_and_frame() {
        assert_eq!(compress_go_native(None).unwrap(), None);
        assert_eq!(compress_go_native(Some(b"")).unwrap(), Some(Vec::new()));
        let payload = b"factory bytes";
        let expected = frame_compressed(
            payload.len() as u32,
            native_go_flate::go_zlib_deflate(payload),
        );
        assert_eq!(compress_go_native(Some(payload)).unwrap(), Some(expected));
        assert_eq!(compressed_length_prefix(0x0102_0304), [4, 3, 2, 1]);
        assert!(compressed_needs_dot(b"stream "));
        assert!(!compressed_needs_dot(b""));
    }

    #[test]
    fn test_uncompress_native_computed_dispositions() {
        assert_eq!(uncompress_native(None).unwrap(), None);
        assert_eq!(uncompress_native(Some(b"")).unwrap(), Some(vec![0]));
        assert_eq!(uncompress_native(Some(&[0; 4])).unwrap(), Some(vec![1]));
        let payload = b"bounded native decoder";
        let framed = compress_go_native(Some(payload)).unwrap().unwrap();
        let mut expected = vec![0];
        expected.extend_from_slice(payload);
        assert_eq!(
            uncompress_native(Some(&framed)).unwrap(),
            Some(expected.clone())
        );
        let mut trailing = framed.clone();
        trailing.extend_from_slice(b"ignored trailing data");
        assert_eq!(uncompress_native(Some(&trailing)).unwrap(), Some(expected));
        let mut too_small = framed;
        too_small[..4].fill(0);
        assert_eq!(uncompress_native(Some(&too_small)).unwrap(), Some(vec![2]));
        let mut bad_checksum = native_go_flate::go_zlib_deflate(payload);
        *bad_checksum.last_mut().unwrap() ^= 1;
        let corrupted = frame_compressed(payload.len() as u32, bad_checksum);
        assert_eq!(uncompress_native(Some(&corrupted)).unwrap(), Some(vec![1]));
        // A complete stream producing zero bytes is native success, unlike
        // wire's decoded-zero corruption policy. Do not run the Go encoder on
        // an empty payload; COMPRESS's original empty shortcut avoids that call.
        let mut stream = Vec::new();
        ZlibEncoder::new(&b""[..], Compression::default())
            .read_to_end(&mut stream)
            .unwrap();
        let empty = frame_compressed(0, stream);
        assert_eq!(uncompress_native(Some(&empty)).unwrap(), Some(vec![0]));
    }

    fn test_unary_func_ok_none<'a, I, O>(sig: ScalarFuncSig)
    where
        I: EvaluableRef<'a>,
        O: EvaluableRet + PartialEq,
        Option<I>: Into<ScalarValue>,
        Option<O>: From<ScalarValue>,
    {
        assert_eq!(
            None,
            RpnFnScalarEvaluator::new()
                .push_param(Option::<I>::None)
                .evaluate::<O>(sig)
                .unwrap()
        );
    }

    #[test]
    fn test_md5() {
        let test_cases = vec![
            (vec![], "d41d8cd98f00b204e9800998ecf8427e"),
            (b"a".to_vec(), "0cc175b9c0f1b6a831c399e269772661"),
            (b"ab".to_vec(), "187ef4436122d1cc2f40dc2b92f0eba0"),
            (b"abc".to_vec(), "900150983cd24fb0d6963f7d28e17f72"),
            (b"123".to_vec(), "202cb962ac59075b964b07152d234b70"),
            (
                "你好".as_bytes().to_vec(),
                "7eca689f0d3389d9dea66ae112e5cfd7",
            ),
            (
                "分布式データベース".as_bytes().to_vec(),
                "63c0354797bd261e2cbf8581147eeeda",
            ),
            (vec![0xc0, 0x80], "b26555f33aedac7b2684438cc5d4d05e"),
            (vec![0xED, 0xA0, 0x80], "546d3dc8de10fbf8b448f678a47901e4"),
        ];
        for (arg, expect_output) in test_cases {
            let expect_output = Some(Bytes::from(expect_output));

            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Bytes>(ScalarFuncSig::Md5)
                .unwrap();
            assert_eq!(output, expect_output);
        }
        test_unary_func_ok_none::<BytesRef, Bytes>(ScalarFuncSig::Md5);
    }

    #[test]
    fn test_sha1() {
        let test_cases = vec![
            (vec![], "da39a3ee5e6b4b0d3255bfef95601890afd80709"),
            (b"a".to_vec(), "86f7e437faa5a7fce15d1ddcb9eaeaea377667b8"),
            (b"ab".to_vec(), "da23614e02469a0d7c7bd1bdab5c9c474b1904dc"),
            (b"abc".to_vec(), "a9993e364706816aba3e25717850c26c9cd0d89d"),
            (b"123".to_vec(), "40bd001563085fc35165329ea1ff5c5ecbdbbeef"),
            (
                "你好".as_bytes().to_vec(),
                "440ee0853ad1e99f962b63e459ef992d7c211722",
            ),
            (
                "分布式データベース".as_bytes().to_vec(),
                "82aa64080df2ca37550ddfc3419d75ac1df3e0d0",
            ),
            (vec![0xc0, 0x80], "8bf4822782a21d7ac68ece130ac36987548003bd"),
            (
                vec![0xED, 0xA0, 0x80],
                "10db70ec072d000c68dd95879f9b831e43a859fd",
            ),
        ];
        for (arg, expect_output) in test_cases {
            let expect_output = Some(Bytes::from(expect_output));

            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Bytes>(ScalarFuncSig::Sha1)
                .unwrap();
            assert_eq!(output, expect_output);
        }
        test_unary_func_ok_none::<BytesRef, Bytes>(ScalarFuncSig::Sha1);
    }

    #[test]
    fn test_uncompressed_length() {
        let cases = vec![
            (Some(""), Some(0)),
            (
                Some("0B000000789CCB48CDC9C95728CF2FCA4901001A0B045D"),
                Some(11),
            ),
            (
                Some("0C000000789CCB48CDC9C95728CF2F32303402001D8004202E"),
                Some(12),
            ),
            (Some("020000000000"), Some(2)),
            (Some("0000000001"), Some(0)),
            (
                Some("02000000789CCB48CDC9C95728CF2FCA4901001A0B045D"),
                Some(2),
            ),
            (Some("010203"), Some(0)),
            (Some("01020304"), Some(0)),
            (None, None),
        ];

        for (s, exp) in cases {
            let s = s.map(|inner| hex::decode(inner.as_bytes()).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(s)
                .evaluate(ScalarFuncSig::UncompressedLength)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    #[rustfmt::skip]
    fn test_sha2() {
        let cases = vec![
            ("pingcap", 0, "2871823be240f8ecd1d72f24c99eaa2e58af18b4b8ba99a4fc2823ba5c43930a"),
            ("pingcap", 224, "cd036dc9bec69e758401379c522454ea24a6327b48724b449b40c6b7"),
            ("pingcap", 256, "2871823be240f8ecd1d72f24c99eaa2e58af18b4b8ba99a4fc2823ba5c43930a"),
            ("pingcap", 384, "c50955b6b0c7b9919740d956849eedcb0f0f90bf8a34e8c1f4e071e3773f53bd6f8f16c04425ff728bed04de1b63db51"),
            ("pingcap", 512, "ea903c574370774c4844a83b7122105a106e04211673810e1baae7c2ae7aba2cf07465e02f6c413126111ef74a417232683ce7ba210052e63c15fc82204aad80"),
            ("13572468", 0, "1c91ab1c162fd0cae60a5bb9880f3e7d5a133a65b6057a644b26973d9c55dcfe"),
            ("13572468", 224, "8ad67735bbf49576219f364f4640d595357a440358d15bf6815a16e4"),
            ("13572468", 256, "1c91ab1c162fd0cae60a5bb9880f3e7d5a133a65b6057a644b26973d9c55dcfe"),
            ("13572468.123", 384, "3b4ee302435dc1e15251efd9f3982b1ca6fe4ac778d3260b7bbf3bea613849677eda830239420e448e4c6dc7c2649d89"),
            ("13572468.123", 512, "4820aa3f2760836557dc1f2d44a0ba7596333fdb60c8a1909481862f4ab0921c00abb23d57b7e67a970363cc3fcb78b25b6a0d45cdcac0e87aa0c96bc51f7f96"),
        ];

        for (input_str, hash_length_i64, exp_str) in cases {
            let exp = Some(Bytes::from(exp_str));

            let got = RpnFnScalarEvaluator::new()
                .push_param(Some(Bytes::from(input_str)))
                .push_param(Some(Int::from(hash_length_i64)))
                .evaluate::<Bytes>(ScalarFuncSig::Sha2)
                .unwrap();
            assert_eq!(got, exp, "sha2('{:?}', {:?})", input_str, hash_length_i64);
        }

        let null_cases = vec![
            (ScalarValue::Bytes(None), ScalarValue::Int(Some(1))),
            (
                ScalarValue::Bytes(Some(b"13572468".to_vec())),
                ScalarValue::Int(None),
            ),
            (ScalarValue::Bytes(None), ScalarValue::Int(None)),
            (
                ScalarValue::Bytes(Some(b"pingcap".to_vec())),
                ScalarValue::Int(Some(-1)),
            ),
            (
                ScalarValue::Bytes(Some(b"13572468".to_vec())),
                ScalarValue::Int(Some(999)),
            ),
        ];

        for (input_str, hash_length_i64) in null_cases {
            assert!(
                RpnFnScalarEvaluator::new()
                    .push_param(input_str)
                    .push_param(hash_length_i64)
                    .evaluate::<Bytes>(ScalarFuncSig::Sha2)
                    .unwrap()
                    .is_none()
            )
        }
    }

    #[test]
    fn test_compress() {
        let test_cases = vec![
            (
                b"hello world".to_vec(),
                "0B000000789CCB48CDC9C95728CF2FCA4901001A0B045D",
            ),
            (b"".to_vec(), ""),
            (
                b"hello wor012".to_vec(),
                "0C000000789CCB48CDC9C95728CF2F32303402001D8004202E",
            ),
        ];
        for (arg, expect) in test_cases {
            let expect = Some(hex::decode(expect.as_bytes()).unwrap());

            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Bytes>(ScalarFuncSig::Compress)
                .unwrap();
            assert_eq!(output, expect);
        }
        test_unary_func_ok_none::<BytesRef, Bytes>(ScalarFuncSig::Compress);
    }

    #[test]
    fn test_uncompress() {
        let cases = vec![
            ("", Some("")),
            (
                "0B000000789CCB48CDC9C95728CF2FCA4901001A0B045D",
                Some("hello world"),
            ),
            (
                "0C000000789CCB48CDC9C95728CF2F32303402001D8004202E",
                Some("hello wor012"),
            ),
            (
                "12000000789CCB48CDC9C95728CF2FCA4901001A0B045D",
                Some("hello world"),
            ),
            ("010203", None),
            ("01020304", None),
            ("020000000000", None),
            ("0000000001", None),
            ("02000000789CCB48CDC9C95728CF2FCA4901001A0B045D", None),
        ];
        for (arg, expect) in cases {
            let arg = hex::decode(arg.as_bytes()).unwrap();
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Bytes>(ScalarFuncSig::Uncompress)
                .unwrap();
            let expect = expect.map(Bytes::from);
            assert_eq!(output, expect);
        }
        test_unary_func_ok_none::<BytesRef, Bytes>(ScalarFuncSig::Uncompress);
    }

    #[test]
    fn test_random_bytes() {
        let cases = vec![1, 32, 233, 1024];

        for len in cases {
            let got = RpnFnScalarEvaluator::new()
                .push_param(Some(len as i64))
                .evaluate::<Bytes>(ScalarFuncSig::RandomBytes)
                .unwrap();
            assert_eq!(got.unwrap().len(), len);
        }

        let overflow_tests = vec![
            ScalarValue::Int(Some(-32)),
            ScalarValue::Int(Some(1025)),
            ScalarValue::Int(Some(0)),
        ];

        for len in overflow_tests {
            RpnFnScalarEvaluator::new()
                .push_param(len)
                .evaluate::<Bytes>(ScalarFuncSig::RandomBytes)
                .unwrap_err();
        }

        // test NULL case
        assert!(
            RpnFnScalarEvaluator::new()
                .push_param(ScalarValue::Int(None))
                .evaluate::<Bytes>(ScalarFuncSig::RandomBytes)
                .unwrap()
                .is_none()
        )
    }

    #[test]
    fn test_password() {
        let cases = vec![
            ("TiKV", "*cca644408381f962dba8dfb9889db1371ee74208"),
            ("Pingcap", "*f33bc75eac70ac317621fbbfa560d6251c43cf8a"),
            ("rust", "*090c2b08e0c1776910e777b917c2185be6554c2e"),
            ("database", "*02e86b4af5219d0ba6c974908aea62d42eb7da24"),
            ("raft", "*b23a77787ed44e62ef2570f03ce8982d119fb699"),
        ];

        for (input, output) in cases {
            let res = RpnFnScalarEvaluator::new()
                .push_param(Some(Bytes::from(input)))
                .evaluate::<Bytes>(ScalarFuncSig::Password)
                .unwrap();
            assert_eq!(res, Some(Bytes::from(output)))
        }

        // test for null
        let res = RpnFnScalarEvaluator::new()
            .push_param(ScalarValue::Bytes(None))
            .evaluate::<Bytes>(ScalarFuncSig::Password)
            .unwrap();
        assert_eq!(None, res)
    }
}
