// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::{convert::TryFrom, fmt, io::Read};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeAesOperation {
    Encrypt,
    Decrypt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeAesProfile {
    Aes128Ecb,
    Aes192Ecb,
    Aes256Ecb,
    Aes128Cbc,
    Aes192Cbc,
    Aes256Cbc,
    Aes128Ofb,
    Aes192Ofb,
    Aes256Ofb,
    Aes128Cfb,
    Aes192Cfb,
    Aes256Cfb,
}

impl NativeAesProfile {
    fn key_size(self) -> usize {
        match self {
            Self::Aes128Ecb | Self::Aes128Cbc | Self::Aes128Ofb | Self::Aes128Cfb => 16,
            Self::Aes192Ecb | Self::Aes192Cbc | Self::Aes192Ofb | Self::Aes192Cfb => 24,
            Self::Aes256Ecb | Self::Aes256Cbc | Self::Aes256Ofb | Self::Aes256Cfb => 32,
        }
    }

    fn iv_required(self) -> bool {
        !matches!(self, Self::Aes128Ecb | Self::Aes192Ecb | Self::Aes256Ecb)
    }
}

/// Only an actual short IV produces this SQL cause. Admission and transport
/// failures stay infrastructure errors; the receipt authenticates both fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeAesError {
    operation: NativeAesOperation,
    profile: NativeAesProfile,
}

impl NativeAesError {
    pub fn operation(&self) -> NativeAesOperation {
        self.operation
    }

    pub fn profile(&self) -> NativeAesProfile {
        self.profile
    }
}

impl fmt::Display for NativeAesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let function = match self.operation {
            NativeAesOperation::Encrypt => "aes_encrypt",
            NativeAesOperation::Decrypt => "aes_decrypt",
        };
        write!(
            formatter,
            "The initialization vector supplied to {function} is too short. Must be at least 16 bytes long"
        )
    }
}

impl std::error::Error for NativeAesError {}

fn native_aes(
    operation: NativeAesOperation,
    profile: NativeAesProfile,
    input: BytesRef,
    password: BytesRef,
    iv: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    use NativeAesOperation::{Decrypt, Encrypt};
    use NativeAesProfile::*;
    use tidb_query_crypto::aes;

    let iv = if profile.iv_required() {
        let iv = iv.ok_or_else(|| other_err!("Native AES IV recipe requires its IV operand"))?;
        if iv.len() < 16 {
            return Err(
                EvaluateError::Caused(Box::new(NativeAesError { operation, profile })).into(),
            );
        }
        &iv[..16]
    } else {
        if iv.is_some() {
            return Err(other_err!(
                "Native AES ECB recipe must not receive an IV operand"
            ));
        }
        &[]
    };
    let key = aes::derive_key_mysql(password, profile.key_size());
    let result: std::result::Result<Bytes, aes::EncryptError> = match (operation, profile) {
        (Encrypt, Aes128Ecb | Aes192Ecb | Aes256Ecb) => aes::aes_encrypt_with_ecb(input, &key),
        (Decrypt, Aes128Ecb | Aes192Ecb | Aes256Ecb) => aes::aes_decrypt_with_ecb(input, &key),
        (Encrypt, Aes128Cbc | Aes192Cbc | Aes256Cbc) => aes::aes_encrypt_with_cbc(input, &key, iv),
        (Decrypt, Aes128Cbc | Aes192Cbc | Aes256Cbc) => aes::aes_decrypt_with_cbc(input, &key, iv),
        (Encrypt, Aes128Ofb | Aes192Ofb | Aes256Ofb) => aes::aes_encrypt_with_ofb(input, &key, iv),
        (Decrypt, Aes128Ofb | Aes192Ofb | Aes256Ofb) => aes::aes_decrypt_with_ofb(input, &key, iv),
        (Encrypt, Aes128Cfb | Aes192Cfb | Aes256Cfb) => aes::aes_encrypt_with_cfb(input, &key, iv),
        (Decrypt, Aes128Cfb | Aes192Cfb | Aes256Cfb) => aes::aes_decrypt_with_cfb(input, &key, iv),
    };
    // Only the shared cipher's source-compatible EncryptError becomes SQL NULL.
    // Do not apply this conversion to admission, allocation, or transport errors.
    Ok(result.ok())
}

macro_rules! native_aes_ecb_recipe {
    ($name:ident, $operation:ident, $profile:ident) => {
        #[rpn_fn]
        fn $name(input: BytesRef, password: BytesRef) -> Result<Option<Bytes>> {
            native_aes(
                NativeAesOperation::$operation,
                NativeAesProfile::$profile,
                input,
                password,
                None,
            )
        }
    };
}

macro_rules! native_aes_iv_recipe {
    ($name:ident, $operation:ident, $profile:ident) => {
        #[rpn_fn]
        fn $name(input: BytesRef, password: BytesRef, iv: BytesRef) -> Result<Option<Bytes>> {
            native_aes(
                NativeAesOperation::$operation,
                NativeAesProfile::$profile,
                input,
                password,
                Some(iv),
            )
        }
    };
}

native_aes_ecb_recipe!(aes_encrypt_128_ecb_native, Encrypt, Aes128Ecb);
native_aes_ecb_recipe!(aes_encrypt_192_ecb_native, Encrypt, Aes192Ecb);
native_aes_ecb_recipe!(aes_encrypt_256_ecb_native, Encrypt, Aes256Ecb);
native_aes_ecb_recipe!(aes_decrypt_128_ecb_native, Decrypt, Aes128Ecb);
native_aes_ecb_recipe!(aes_decrypt_192_ecb_native, Decrypt, Aes192Ecb);
native_aes_ecb_recipe!(aes_decrypt_256_ecb_native, Decrypt, Aes256Ecb);
native_aes_iv_recipe!(aes_encrypt_128_cbc_native, Encrypt, Aes128Cbc);
native_aes_iv_recipe!(aes_encrypt_192_cbc_native, Encrypt, Aes192Cbc);
native_aes_iv_recipe!(aes_encrypt_256_cbc_native, Encrypt, Aes256Cbc);
native_aes_iv_recipe!(aes_decrypt_128_cbc_native, Decrypt, Aes128Cbc);
native_aes_iv_recipe!(aes_decrypt_192_cbc_native, Decrypt, Aes192Cbc);
native_aes_iv_recipe!(aes_decrypt_256_cbc_native, Decrypt, Aes256Cbc);
native_aes_iv_recipe!(aes_encrypt_128_ofb_native, Encrypt, Aes128Ofb);
native_aes_iv_recipe!(aes_encrypt_192_ofb_native, Encrypt, Aes192Ofb);
native_aes_iv_recipe!(aes_encrypt_256_ofb_native, Encrypt, Aes256Ofb);
native_aes_iv_recipe!(aes_decrypt_128_ofb_native, Decrypt, Aes128Ofb);
native_aes_iv_recipe!(aes_decrypt_192_ofb_native, Decrypt, Aes192Ofb);
native_aes_iv_recipe!(aes_decrypt_256_ofb_native, Decrypt, Aes256Ofb);
native_aes_iv_recipe!(aes_encrypt_128_cfb_native, Encrypt, Aes128Cfb);
native_aes_iv_recipe!(aes_encrypt_192_cfb_native, Encrypt, Aes192Cfb);
native_aes_iv_recipe!(aes_encrypt_256_cfb_native, Encrypt, Aes256Cfb);
native_aes_iv_recipe!(aes_decrypt_128_cfb_native, Decrypt, Aes128Cfb);
native_aes_iv_recipe!(aes_decrypt_192_cfb_native, Decrypt, Aes192Cfb);
native_aes_iv_recipe!(aes_decrypt_256_cfb_native, Decrypt, Aes256Cfb);

#[rpn_fn(nullable)]
fn aes_null_native(witness: Option<&Int>) -> Result<Option<Bytes>> {
    match witness {
        None => Ok(None),
        Some(_) => Err(other_err!("Native AES NULL witness must be an actual NULL")),
    }
}

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

#[rpn_fn]
fn get_native_sql_encode(data: BytesRef, password: BytesRef) -> Result<Option<Bytes>> {
    Ok(Some(tidb_query_crypto::sql_encode(data, password)))
}

#[rpn_fn]
fn get_native_sql_decode(data: BytesRef, password: BytesRef) -> Result<Option<Bytes>> {
    Ok(Some(tidb_query_crypto::sql_decode(data, password)))
}

#[rpn_fn(nullable)]
fn get_native_sql_crypt_null(arg: Option<&Int>) -> Result<Option<Bytes>> {
    match arg {
        None => Ok(None),
        Some(_) => Err(other_err!(
            "Native SQLCrypt NULL witness must be an actual NULL"
        )),
    }
}

#[rpn_fn(nullable)]
fn password_native(input: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(input.map(|bytes| tidb_query_crypto::encode_password_bytes(bytes).into_bytes()))
}

#[rpn_fn(nullable)]
fn sm3_native(input: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(input.map(|bytes| hex::encode(tidb_query_crypto::sm3_hash(bytes)).into_bytes()))
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
mod native_aes_tests {
    use tidb_query_common::error::ErrorInner;

    use super::*;

    type EcbRecipe = fn(&[u8], &[u8]) -> Result<Option<Bytes>>;
    type IvRecipe = fn(&[u8], &[u8], &[u8]) -> Result<Option<Bytes>>;
    const KEY128: &str = "2b7e151628aed2a6abf7158809cf4f3c";
    const KEY192: &str = "8e73b0f7da0e6452c810f32b809079e562f8ead2522c6b7b";
    const KEY256: &str = "603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4";

    // NIST SP 800-38A two-block vectors distinguish OFB from CFB. MySQL
    // ECB/CBC append PKCS#7; the stream modes preserve the input length.
    const ECB: [(EcbRecipe, EcbRecipe, &str, &str); 3] = [
        (
            aes_encrypt_128_ecb_native,
            aes_decrypt_128_ecb_native,
            KEY128,
            concat!(
                "3ad77bb40d7a3660a89ecaf32466ef97",
                "f5d3d58503b9699de785895a96fdbaaf"
            ),
        ),
        (
            aes_encrypt_192_ecb_native,
            aes_decrypt_192_ecb_native,
            KEY192,
            concat!(
                "bd334f1d6e45f25ff712a214571fa5cc",
                "974104846d0ad3ad7734ecb3ecee4eef"
            ),
        ),
        (
            aes_encrypt_256_ecb_native,
            aes_decrypt_256_ecb_native,
            KEY256,
            concat!(
                "f3eed1bdb5d2a03c064b5a7e3db181f8",
                "591ccb10d410ed26dc5ba74a31362870"
            ),
        ),
    ];
    const IV: [(NativeAesProfile, IvRecipe, IvRecipe, &str, &str); 9] = [
        (
            NativeAesProfile::Aes128Cbc,
            aes_encrypt_128_cbc_native,
            aes_decrypt_128_cbc_native,
            KEY128,
            concat!(
                "7649abac8119b246cee98e9b12e9197d",
                "5086cb9b507219ee95db113a917678b2"
            ),
        ),
        (
            NativeAesProfile::Aes192Cbc,
            aes_encrypt_192_cbc_native,
            aes_decrypt_192_cbc_native,
            KEY192,
            concat!(
                "4f021db243bc633d7178183a9fa071e8",
                "b4d9ada9ad7dedf4e5e738763f69145a"
            ),
        ),
        (
            NativeAesProfile::Aes256Cbc,
            aes_encrypt_256_cbc_native,
            aes_decrypt_256_cbc_native,
            KEY256,
            concat!(
                "f58c4c04d6e5f1ba779eabfb5f7bfbd6",
                "9cfc4e967edb808d679f777bc6702c7d"
            ),
        ),
        (
            NativeAesProfile::Aes128Ofb,
            aes_encrypt_128_ofb_native,
            aes_decrypt_128_ofb_native,
            KEY128,
            concat!(
                "3b3fd92eb72dad20333449f8e83cfb4a",
                "7789508d16918f03f53c52dac54ed825"
            ),
        ),
        (
            NativeAesProfile::Aes192Ofb,
            aes_encrypt_192_ofb_native,
            aes_decrypt_192_ofb_native,
            KEY192,
            concat!(
                "cdc80d6fddf18cab34c25909c99a4174",
                "fcc28b8d4c63837c09e81700c1100401"
            ),
        ),
        (
            NativeAesProfile::Aes256Ofb,
            aes_encrypt_256_ofb_native,
            aes_decrypt_256_ofb_native,
            KEY256,
            concat!(
                "dc7e84bfda79164b7ecd8486985d3860",
                "4febdc6740d20b3ac88f6ad82a4fb08d"
            ),
        ),
        (
            NativeAesProfile::Aes128Cfb,
            aes_encrypt_128_cfb_native,
            aes_decrypt_128_cfb_native,
            KEY128,
            concat!(
                "3b3fd92eb72dad20333449f8e83cfb4a",
                "c8a64537a0b3a93fcde3cdad9f1ce58b"
            ),
        ),
        (
            NativeAesProfile::Aes192Cfb,
            aes_encrypt_192_cfb_native,
            aes_decrypt_192_cfb_native,
            KEY192,
            concat!(
                "cdc80d6fddf18cab34c25909c99a4174",
                "67ce7f7f81173621961a2b70171d3d7a"
            ),
        ),
        (
            NativeAesProfile::Aes256Cfb,
            aes_encrypt_256_cfb_native,
            aes_decrypt_256_cfb_native,
            KEY256,
            concat!(
                "dc7e84bfda79164b7ecd8486985d3860",
                "39ffed143b28b1c832113c6331e5407b"
            ),
        ),
    ];

    #[test]
    fn native_aes_static_nist_and_go_ciphertexts() {
        let plaintext = hex::decode(concat!(
            "6bc1bee22e409f96e93d7e117393172a",
            "ae2d8a571e03ac9c9eb76fac45af8e51"
        ))
        .unwrap();
        let iv = hex::decode("000102030405060708090a0b0c0d0e0f").unwrap();
        for (encrypt, decrypt, key, ciphertext) in ECB {
            let key = hex::decode(key).unwrap();
            let expected = hex::decode(ciphertext).unwrap();
            let actual = encrypt(&plaintext, &key).unwrap().unwrap();
            assert_eq!(actual.len(), 48);
            assert_eq!(&actual[..32], expected);
            // This fixed raw NIST plaintext has no PKCS#7 padding.
            assert_eq!(decrypt(&expected, &key).unwrap(), None);
        }
        for (profile, encrypt, decrypt, key, ciphertext) in IV {
            let key = hex::decode(key).unwrap();
            let expected = hex::decode(ciphertext).unwrap();
            let actual = encrypt(&plaintext, &key, &iv).unwrap().unwrap();
            assert_eq!(&actual[..32], expected);
            if matches!(
                profile,
                NativeAesProfile::Aes128Cbc
                    | NativeAesProfile::Aes192Cbc
                    | NativeAesProfile::Aes256Cbc
            ) {
                assert_eq!(actual.len(), 48);
                assert_eq!(decrypt(&expected, &key, &iv).unwrap(), None);
            } else {
                assert_eq!(actual.len(), 32);
                assert_eq!(
                    decrypt(&expected, &key, &iv).unwrap(),
                    Some(plaintext.clone())
                );
            }
        }
        // Original Go vectors include complete PKCS#7 ciphertext, not just
        // raw blocks, and exercise valid ECB/CBC decryption independently.
        let key = b"1234567890123456";
        let ecb = hex::decode("697BFE9B3F8C2F289DD82C88C7BC95C4").unwrap();
        let cbc = hex::decode("2ECA0077C5EA5768A0485AA522774792").unwrap();
        assert_eq!(
            aes_encrypt_128_ecb_native(b"pingcap", key).unwrap(),
            Some(ecb.clone())
        );
        assert_eq!(
            aes_decrypt_128_ecb_native(&ecb, key).unwrap(),
            Some(b"pingcap".to_vec())
        );
        assert_eq!(
            aes_encrypt_128_cbc_native(b"pingcap", key, key).unwrap(),
            Some(cbc.clone())
        );
        assert_eq!(
            aes_decrypt_128_cbc_native(&cbc, key, key).unwrap(),
            Some(b"pingcap".to_vec())
        );
    }

    #[test]
    fn native_aes_exact_error_profiles_iv_truncation_and_null() {
        for (profile, encrypt, decrypt, ..) in IV {
            for (operation, recipe, function) in [
                (NativeAesOperation::Encrypt, encrypt, "aes_encrypt"),
                (NativeAesOperation::Decrypt, decrypt, "aes_decrypt"),
            ] {
                // Even empty ciphertext must validate the IV before cipher errors.
                let error = recipe(b"", b"password", &[0; 15]).unwrap_err();
                match error.0.as_ref() {
                    ErrorInner::Evaluate(EvaluateError::Caused(cause)) => {
                        let cause = cause.downcast_ref::<NativeAesError>().unwrap();
                        assert_eq!(cause.operation(), operation);
                        assert_eq!(cause.profile(), profile);
                        assert_eq!(
                            cause.to_string(),
                            format!(
                                "The initialization vector supplied to {function} is too short. Must be at least 16 bytes long"
                            )
                        );
                    }
                    _ => panic!("lost native AES typed cause: {error:?}"),
                }
            }
        }
        let expected = hex::decode("2ECA0077C5EA5768A0485AA522774792").unwrap();
        assert_eq!(
            aes_encrypt_128_cbc_native(b"pingcap", b"1234567890123456", b"1234567890123456ignored")
                .unwrap(),
            Some(expected)
        );
        assert_eq!(
            aes_decrypt_128_ecb_native(b"short", b"password").unwrap(),
            None
        );
        assert_eq!(
            aes_decrypt_128_cbc_native(b"short", b"password", &[0; 16]).unwrap(),
            None
        );
        assert_eq!(aes_null_native(None).unwrap(), None);
        assert!(aes_null_native(Some(&0)).is_err());
    }
}

#[cfg(test)]
mod tests {
    use tipb::ScalarFuncSig;

    use super::*;
    use crate::types::test_util::RpnFnScalarEvaluator;

    #[test]
    fn test_native_sql_crypt_source_literals() {
        // Original builtin_ext/crypto.rs vectors deliberately DECODE plaintext
        // to the fixed hex and ENCODE that cryptogram back; do not swap names.
        for (origin, password, encoded_hex) in [
            ("", "", ""),
            ("pingcap", "1234567890123456", "2C35B5A4ADF391"),
            ("pingcap", "asdfjasfwefjfjkj", "351CC412605905"),
            (
                "pingcap123",
                "123456789012345678901234",
                "7698723DC6DFE7724221",
            ),
            ("pingcap#%$%^", "*^%YTu1234567", "8634B9C55FF55E5B6328F449"),
            ("pingcap", "", "4A77B524BD2C5C"),
            (
                "分布式データベース",
                "pass1234@#$%%^^&",
                "80CADC8D328B3026D04FB285F36FED04BBCA0CC685BF78B1E687CE",
            ),
            (
                "分布式データベース",
                "分布式7782734adgwy1242",
                "0E24CFEF272EE32B6E0BFBDB89F29FB43B4B30DAA95C3F914444BC",
            ),
            ("pingcap", "密匙", "CE5C02A5010010"),
            (
                "pingcap数据库",
                "数据库passwd12345667",
                "36D5F90D3834E30E396BE3226E3B4ED3",
            ),
            ("数据库5667", "123.435", "B22196D0569386237AE12F8AAB"),
        ] {
            let cryptogram = hex::decode(encoded_hex).unwrap();
            assert_eq!(
                get_native_sql_decode(origin.as_bytes(), password.as_bytes()).unwrap(),
                Some(cryptogram.clone())
            );
            assert_eq!(
                get_native_sql_encode(&cryptogram, password.as_bytes()).unwrap(),
                Some(origin.as_bytes().to_vec())
            );
        }
    }

    #[test]
    fn test_native_sql_crypt_null_witness() {
        assert_eq!(get_native_sql_crypt_null(None).unwrap(), None);
        assert!(get_native_sql_crypt_null(Some(&0)).is_err());
    }

    #[test]
    fn test_password_sm3_native_source_literals() {
        assert_eq!(password_native(None).unwrap(), None);
        assert_eq!(sm3_native(None).unwrap(), None);
        for (input, expected) in [
            (b"".as_slice(), ""),
            (b"abc", "*0D3CED9BEC10A777AEC23CCC353A8C08A633045E"),
            (b"\xff\x00a", "*F5A241511384DB827F22D2A2188A456E87F7D4F2"),
        ] {
            assert_eq!(
                password_native(Some(input)).unwrap(),
                Some(expected.as_bytes().to_vec())
            );
        }
        // Fixed original parser-auth Go vectors, not the provider's own output.
        for (input, expected) in [
            (
                b"abc".as_slice(),
                "66c7f0f462eeedd9d1f2d46bdc10e4e24167c4875cf2f7a2297da02b8f4ba8e0",
            ),
            (
                b"abcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd",
                "debe9ff92275b8a138604889c18e5a4d6fdb70e5387e5765293dcba39c0c5732",
            ),
        ] {
            assert_eq!(
                sm3_native(Some(input)).unwrap(),
                Some(expected.as_bytes().to_vec())
            );
        }
    }

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
