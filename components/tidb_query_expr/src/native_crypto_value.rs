// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Finite actual deterministic crypto values over the original pure owners.
use crate::impl_encryption as original;
/// Closed original native crypto operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCryptoOperation {
    /// Original MD5 lowercase digest.
    Md5,
    /// Original SHA spelling of SHA1.
    Sha,
    /// Original SHA1 lowercase digest.
    Sha1,
    /// Original warning-free native SHA2 with eager children.
    Sha2,
    /// Original SM3 lowercase hexadecimal digest.
    Sm3,
    /// Native double-raw SHA1 password policy, distinct from wire PASSWORD.
    Password,
    /// Native MySQL 3.21 deterministic encoder.
    Encode,
    /// Native MySQL 3.21 deterministic decoder.
    Decode,
}
impl NativeCryptoOperation {
    /// Original lower-case SQL name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::Sha => "sha",
            Self::Sha1 => "sha1",
            Self::Sha2 => "sha2",
            Self::Sm3 => "sm3",
            Self::Password => "password",
            Self::Encode => "encode",
            Self::Decode => "decode",
        }
    }
    /// Original eager child-expression count.
    pub fn arity(self) -> usize {
        match self {
            Self::Md5 | Self::Sha | Self::Sha1 | Self::Sm3 | Self::Password => 1,
            Self::Sha2 | Self::Encode | Self::Decode => 2,
        }
    }
}
/// Finite work exhaustion is distinct from an impossible pure-owner result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCryptoValueError {
    /// Actual byte work exceeds its finite allowance.
    Resource,
    /// The selected pure owner returned an impossible error/shape.
    Contract,
}
use NativeCryptoValueError as E;
fn work(n: usize, maximum: usize) -> Result<(), E> {
    if n > maximum {
        Err(E::Resource)
    } else {
        Ok(())
    }
}
fn extent(n: usize) -> Result<usize, E> {
    n.checked_mul(16)
        .and_then(|n| n.checked_add(4096))
        .ok_or(E::Resource)
}
/// Genuine nullable bytes, with no context warning or transport envelope.
pub fn native_crypto_unary_actual_value(
    op: NativeCryptoOperation,
    input: Option<&[u8]>,
    maximum: usize,
) -> Result<Option<Vec<u8>>, E> {
    if !matches!(
        op,
        NativeCryptoOperation::Md5
            | NativeCryptoOperation::Sha
            | NativeCryptoOperation::Sha1
            | NativeCryptoOperation::Sm3
            | NativeCryptoOperation::Password
    ) {
        return Err(E::Contract);
    }
    work(input.map_or(Ok(0), |v| extent(v.len()))?, maximum)?;
    match op {
        NativeCryptoOperation::Md5 => original::md5(input),
        NativeCryptoOperation::Sha | NativeCryptoOperation::Sha1 => original::sha1(input),
        NativeCryptoOperation::Sm3 => original::sm3_native(input),
        NativeCryptoOperation::Password => original::password_native(input),
        _ => unreachable!(),
    }
    .map_err(|_| E::Contract)
}
/// Actual non-NULL SHA2 bytes and original converted length, without wire
/// warnings. Invalid selectors do not demand a scan of the borrowed payload;
/// the original pure owner still decides the genuine NULL result.
pub fn native_sha2_actual_value(
    input: &[u8],
    length: i64,
    maximum: usize,
) -> Result<Option<Vec<u8>>, E> {
    let cost = if matches!(length, 0 | 224 | 256 | 384 | 512) {
        extent(input.len())?
    } else {
        0
    };
    work(cost, maximum)?;
    original::sha2_native(input, &length).map_err(|_| E::Contract)
}
/// Both inputs are genuine already-coerced non-NULL byte values. The frontend
/// retains ordered NULL/coercion barriers before invoking this pure owner.
pub fn native_sql_crypt_actual_value(
    op: NativeCryptoOperation,
    data: &[u8],
    password: &[u8],
    maximum: usize,
) -> Result<Vec<u8>, E> {
    if !matches!(
        op,
        NativeCryptoOperation::Encode | NativeCryptoOperation::Decode
    ) {
        return Err(E::Contract);
    }
    work(
        extent(data.len().checked_add(password.len()).ok_or(E::Resource)?)?,
        maximum,
    )?;
    match op {
        NativeCryptoOperation::Encode => original::get_native_sql_encode(data, password),
        NativeCryptoOperation::Decode => original::get_native_sql_decode(data, password),
        _ => unreachable!(),
    }
    .map_err(|_| E::Contract)?
    .ok_or(E::Contract)
}

#[cfg(test)]
mod tests {
    use NativeCryptoOperation as O;

    use super::*;
    #[test]
    fn actual_crypto_values_preserve_original_hash_password_crypt_bytes_and_finite_work() {
        for op in [O::Sm3, O::Password] {
            assert_eq!(native_crypto_unary_actual_value(op, None, 0), Ok(None));
            assert_eq!(
                native_crypto_unary_actual_value(op, None, usize::MAX),
                native_crypto_unary_actual_value(op, None, 1 << 30)
            );
            assert_eq!(
                native_crypto_unary_actual_value(op, Some(&vec![255; 1 << 20]), 8192),
                Err(E::Resource)
            );
        }
        assert_eq!(
            native_crypto_unary_actual_value(O::Sm3, Some(b"abc"), 8192),
            Ok(Some(
                b"66c7f0f462eeedd9d1f2d46bdc10e4e24167c4875cf2f7a2297da02b8f4ba8e0".to_vec()
            ))
        );
        assert_eq!(
            native_crypto_unary_actual_value(O::Password, Some(b"123"), 8192),
            Ok(Some(b"*23AE809DDACAF96AF0FD78ED04B6A265E05AA257".to_vec()))
        );
        assert_eq!(
            native_crypto_unary_actual_value(O::Password, Some(b"\xff\0a"), 8192),
            Ok(Some(b"*F5A241511384DB827F22D2A2188A456E87F7D4F2".to_vec()))
        );
        assert_eq!(
            native_crypto_unary_actual_value(O::Password, Some(b""), 8192),
            Ok(Some(vec![]))
        );
        assert_eq!(
            native_crypto_unary_actual_value(O::Encode, None, 8192),
            Err(E::Contract)
        );
        let decoded = [0x2c, 0x35, 0xb5, 0xa4, 0xad, 0xf3, 0x91];
        assert_eq!(
            native_sql_crypt_actual_value(O::Decode, b"pingcap", b"1234567890123456", 8192),
            Ok(decoded.to_vec())
        );
        assert_eq!(
            native_sql_crypt_actual_value(O::Encode, &decoded, b"1234567890123456", 8192),
            Ok(b"pingcap".to_vec())
        );
        assert_eq!(
            native_sql_crypt_actual_value(O::Decode, b"pingcap", b" \t", 8192),
            Ok(vec![0x4a, 0x77, 0xb5, 0x24, 0xbd, 0x2c, 0x5c])
        );
        for op in [O::Encode, O::Decode] {
            assert_eq!(
                native_sql_crypt_actual_value(op, b"", b"", 8192),
                Ok(vec![])
            );
            assert_eq!(
                native_sql_crypt_actual_value(op, b"x", b"p", 0),
                Err(E::Resource)
            );
        }
        assert_eq!(
            native_sql_crypt_actual_value(O::Password, b"", b"", 8192),
            Err(E::Contract)
        );
    }
}

#[cfg(test)]
mod digest_tests {
    use super::*;
    #[test]
    fn actual_digests_keep_original_vectors_aliases_null_invalid_selectors_and_finite_work() {
        for (op, expected) in [
            (
                NativeCryptoOperation::Md5,
                b"900150983cd24fb0d6963f7d28e17f72".as_slice(),
            ),
            (
                NativeCryptoOperation::Sha,
                b"a9993e364706816aba3e25717850c26c9cd0d89d".as_slice(),
            ),
            (
                NativeCryptoOperation::Sha1,
                b"a9993e364706816aba3e25717850c26c9cd0d89d".as_slice(),
            ),
        ] {
            assert_eq!(
                native_crypto_unary_actual_value(op, Some(b"abc"), 8192),
                Ok(Some(expected.to_vec()))
            );
            assert_eq!(native_crypto_unary_actual_value(op, None, 0), Ok(None));
            assert_eq!(
                native_crypto_unary_actual_value(op, None, usize::MAX),
                native_crypto_unary_actual_value(op, None, 1 << 30)
            );
            assert_eq!(
                native_crypto_unary_actual_value(op, Some(&vec![255; 1 << 20]), 8192),
                Err(E::Resource)
            );
        }
        for (bits, expected) in [
            (
                0,
                "2871823be240f8ecd1d72f24c99eaa2e58af18b4b8ba99a4fc2823ba5c43930a",
            ),
            (
                224,
                "cd036dc9bec69e758401379c522454ea24a6327b48724b449b40c6b7",
            ),
            (
                256,
                "2871823be240f8ecd1d72f24c99eaa2e58af18b4b8ba99a4fc2823ba5c43930a",
            ),
            (
                384,
                "c50955b6b0c7b9919740d956849eedcb0f0f90bf8a34e8c1f4e071e3773f53bd6f8f16c04425ff728bed04de1b63db51",
            ),
            (
                512,
                "ea903c574370774c4844a83b7122105a106e04211673810e1baae7c2ae7aba2cf07465e02f6c413126111ef74a417232683ce7ba210052e63c15fc82204aad80",
            ),
        ] {
            assert_eq!(
                native_sha2_actual_value(b"pingcap", bits, 8192),
                Ok(Some(expected.as_bytes().to_vec()))
            );
        }
        let big = vec![255; 1 << 20];
        for bits in [-1, 128, i64::MAX] {
            assert_eq!(native_sha2_actual_value(&big, bits, 0), Ok(None));
            assert_eq!(
                native_sha2_actual_value(&big, bits, usize::MAX),
                native_sha2_actual_value(&big, bits, 1 << 30)
            );
        }
        assert_eq!(native_sha2_actual_value(&big, 256, 8192), Err(E::Resource));
        assert_eq!(
            native_crypto_unary_actual_value(NativeCryptoOperation::Sha2, None, 8192),
            Err(E::Contract)
        );
    }
}
