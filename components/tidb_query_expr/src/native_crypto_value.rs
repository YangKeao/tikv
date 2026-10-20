// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Finite actual deterministic crypto values over the original pure owners.
use crate::impl_encryption as original;
/// Closed original native crypto operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCryptoOperation {
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
            Self::Sm3 => "sm3",
            Self::Password => "password",
            Self::Encode => "encode",
            Self::Decode => "decode",
        }
    }
    /// Original eager child-expression count.
    pub fn arity(self) -> usize {
        match self {
            Self::Sm3 | Self::Password => 1,
            Self::Encode | Self::Decode => 2,
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
    if maximum == usize::MAX || n > maximum {
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
        NativeCryptoOperation::Sm3 | NativeCryptoOperation::Password
    ) {
        return Err(E::Contract);
    }
    work(input.map_or(Ok(0), |v| extent(v.len()))?, maximum)?;
    match op {
        NativeCryptoOperation::Sm3 => original::sm3_native(input),
        NativeCryptoOperation::Password => original::password_native(input),
        _ => unreachable!(),
    }
    .map_err(|_| E::Contract)
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
                Err(E::Resource)
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
