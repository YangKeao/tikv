// Copyright 2026 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Shared AES helpers relocated from the native `pkg/util/encrypt/aes.go` port.

use std::fmt;

use aes::{
    Aes128, Aes192, Aes256,
    cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit},
};

/// AES block size in bytes, independent of key length.
pub const AES_BLOCK_SIZE: usize = 16;

/// Source-compatible failures from TiDB's AES helpers.
#[derive(Debug)]
pub enum EncryptError {
    /// Go `aes.NewCipher` rejected a key outside 128/192/256 bits.
    InvalidKeyLength(usize),
    /// A block-mode ciphertext was not a whole number of AES blocks.
    CorruptedData,
    /// PKCS#7 size or terminal length was invalid.
    InvalidPaddingSize,
    /// PKCS#7 bytes disagreed with the terminal pad byte.
    InvalidPadding,
}

impl fmt::Display for EncryptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKeyLength(length) => {
                write!(formatter, "crypto/aes: invalid key size {length}")
            }
            Self::CorruptedData => formatter.write_str("Corrupted data"),
            Self::InvalidPaddingSize => formatter.write_str("Invalid padding size"),
            Self::InvalidPadding => formatter.write_str("Invalid padding"),
        }
    }
}

impl std::error::Error for EncryptError {}

/// Opaque AES block primitive for the native random-access encryption layer.
/// Key variants and the underlying cipher implementation remain private.
pub struct AesCipher {
    cipher: Cipher,
}

enum Cipher {
    Aes128(Aes128),
    Aes192(Aes192),
    Aes256(Aes256),
}

impl AesCipher {
    /// Constructs an AES cipher with a 16-, 24-, or 32-byte key.
    pub fn new(key: &[u8]) -> Result<Self, EncryptError> {
        let cipher = match key.len() {
            16 => Cipher::Aes128(Aes128::new_from_slice(key).expect("validated AES-128 key")),
            24 => Cipher::Aes192(Aes192::new_from_slice(key).expect("validated AES-192 key")),
            32 => Cipher::Aes256(Aes256::new_from_slice(key).expect("validated AES-256 key")),
            length => return Err(EncryptError::InvalidKeyLength(length)),
        };
        Ok(Self { cipher })
    }

    /// Encrypts exactly one AES block in place, without mode or padding policy.
    pub fn encrypt_block(&self, block: &mut [u8; AES_BLOCK_SIZE]) {
        let mut value = aes::Block::from(*block);
        match &self.cipher {
            Cipher::Aes128(cipher) => cipher.encrypt_block(&mut value),
            Cipher::Aes192(cipher) => cipher.encrypt_block(&mut value),
            Cipher::Aes256(cipher) => cipher.encrypt_block(&mut value),
        }
        block.copy_from_slice(&value);
    }

    /// Decrypts exactly one AES block in place, without mode or padding policy.
    pub fn decrypt_block(&self, block: &mut [u8; AES_BLOCK_SIZE]) {
        let mut value = aes::Block::from(*block);
        match &self.cipher {
            Cipher::Aes128(cipher) => cipher.decrypt_block(&mut value),
            Cipher::Aes192(cipher) => cipher.decrypt_block(&mut value),
            Cipher::Aes256(cipher) => cipher.decrypt_block(&mut value),
        }
        block.copy_from_slice(&value);
    }
}

/// Pads `data` using the PKCS#7 algorithm.
pub fn pkcs7_pad(data: &[u8], block_size: usize) -> Vec<u8> {
    let pad_len = block_size - data.len() % block_size;
    let mut padded = Vec::with_capacity(data.len() + pad_len);
    padded.extend_from_slice(data);
    padded.resize(data.len() + pad_len, pad_len as u8);
    padded
}

/// Removes PKCS#7 padding.
#[allow(clippy::manual_is_multiple_of)] // `% 0` preserves Go's zero-block panic.
pub fn pkcs7_unpad(data: &[u8], block_size: usize) -> Result<&[u8], EncryptError> {
    if data.is_empty() || data.len() % block_size != 0 {
        return Err(EncryptError::InvalidPaddingSize);
    }
    let pad = data[data.len() - 1];
    let pad_len = usize::from(pad);
    if pad_len > block_size || pad_len == 0 {
        return Err(EncryptError::InvalidPaddingSize);
    }
    if data[data.len() - pad_len..data.len() - 1]
        .iter()
        .any(|value| *value != pad)
    {
        return Err(EncryptError::InvalidPadding);
    }
    Ok(&data[..data.len() - pad_len])
}

fn encrypt_ecb_blocks(cipher: &AesCipher, source: &[u8]) -> Vec<u8> {
    assert_eq!(
        source.len() % AES_BLOCK_SIZE,
        0,
        "ECBEncrypter: input not full blocks"
    );
    let mut destination = source.to_vec();
    for block in destination.as_chunks_mut::<AES_BLOCK_SIZE>().0 {
        cipher.encrypt_block(block);
    }
    destination
}

fn decrypt_ecb_blocks(cipher: &AesCipher, source: &[u8]) -> Vec<u8> {
    assert_eq!(
        source.len() % AES_BLOCK_SIZE,
        0,
        "ECBDecrypter: input not full blocks"
    );
    let mut destination = source.to_vec();
    for block in destination.as_chunks_mut::<AES_BLOCK_SIZE>().0 {
        cipher.decrypt_block(block);
    }
    destination
}

/// Encrypts arbitrary-length data using AES-ECB and PKCS#7 padding.
pub fn aes_encrypt_with_ecb(data: &[u8], key: &[u8]) -> Result<Vec<u8>, EncryptError> {
    let cipher = AesCipher::new(key)?;
    Ok(encrypt_ecb_blocks(
        &cipher,
        &pkcs7_pad(data, AES_BLOCK_SIZE),
    ))
}

/// Decrypts AES-ECB data and removes PKCS#7 padding.
pub fn aes_decrypt_with_ecb(data: &[u8], key: &[u8]) -> Result<Vec<u8>, EncryptError> {
    let cipher = AesCipher::new(key)?;
    if !data.len().is_multiple_of(AES_BLOCK_SIZE) {
        return Err(EncryptError::CorruptedData);
    }
    let decrypted = decrypt_ecb_blocks(&cipher, data);
    pkcs7_unpad(&decrypted, AES_BLOCK_SIZE).map(<[u8]>::to_vec)
}

/// Derives an AES key using MySQL's historical XOR-folding algorithm.
pub fn derive_key_mysql(key: &[u8], block_size: usize) -> Vec<u8> {
    let mut derived = vec![0; block_size];
    let mut index = 0;
    for value in key {
        if index == block_size {
            index = 0;
        }
        derived[index] ^= value;
        index += 1;
    }
    derived
}

fn validate_iv(iv: &[u8], panic_message: &str) -> [u8; AES_BLOCK_SIZE] {
    if iv.len() != AES_BLOCK_SIZE {
        panic!("{panic_message}");
    }
    iv.try_into().expect("validated AES IV")
}

/// Encrypts arbitrary-length data using AES-CBC and PKCS#7 padding.
pub fn aes_encrypt_with_cbc(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, EncryptError> {
    let cipher = AesCipher::new(key)?;
    let mut previous = validate_iv(
        iv,
        "cipher.NewCBCEncrypter: IV length must equal block size",
    );
    let mut destination = pkcs7_pad(data, AES_BLOCK_SIZE);
    for block in destination.as_chunks_mut::<AES_BLOCK_SIZE>().0 {
        for (value, prior) in block.iter_mut().zip(previous) {
            *value ^= prior;
        }
        cipher.encrypt_block(block);
        previous = *block;
    }
    Ok(destination)
}

/// Decrypts AES-CBC data and removes PKCS#7 padding.
pub fn aes_decrypt_with_cbc(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, EncryptError> {
    let cipher = AesCipher::new(key)?;
    let mut previous = validate_iv(
        iv,
        "cipher.NewCBCDecrypter: IV length must equal block size",
    );
    if !data.len().is_multiple_of(AES_BLOCK_SIZE) {
        return Err(EncryptError::CorruptedData);
    }
    let mut destination = data.to_vec();
    for block in destination.as_chunks_mut::<AES_BLOCK_SIZE>().0 {
        let mut ciphertext = [0_u8; AES_BLOCK_SIZE];
        ciphertext.copy_from_slice(block);
        cipher.decrypt_block(block);
        for (value, prior) in block.iter_mut().zip(previous) {
            *value ^= prior;
        }
        previous = ciphertext;
    }
    pkcs7_unpad(&destination, AES_BLOCK_SIZE).map(<[u8]>::to_vec)
}

fn increment_counter(counter: &mut [u8; AES_BLOCK_SIZE]) {
    for value in counter.iter_mut().rev() {
        *value = value.wrapping_add(1);
        if *value != 0 {
            break;
        }
    }
}

fn aes_crypt_with_ofb(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, EncryptError> {
    let cipher = AesCipher::new(key)?;
    let mut feedback = validate_iv(iv, "cipher.NewOFB: IV length must equal block size");
    let mut destination = Vec::with_capacity(data.len());
    for chunk in data.chunks(AES_BLOCK_SIZE) {
        cipher.encrypt_block(&mut feedback);
        destination.extend(chunk.iter().zip(feedback).map(|(value, mask)| value ^ mask));
    }
    Ok(destination)
}

/// Encrypts data using AES-OFB.
pub fn aes_encrypt_with_ofb(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, EncryptError> {
    aes_crypt_with_ofb(data, key, iv)
}

/// Decrypts data using AES-OFB.
pub fn aes_decrypt_with_ofb(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, EncryptError> {
    aes_crypt_with_ofb(data, key, iv)
}

fn aes_crypt_with_ctr(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, EncryptError> {
    let cipher = AesCipher::new(key)?;
    let mut counter = validate_iv(iv, "bad IV length");
    let mut destination = Vec::with_capacity(data.len());
    for chunk in data.chunks(AES_BLOCK_SIZE) {
        let mut mask = counter;
        cipher.encrypt_block(&mut mask);
        increment_counter(&mut counter);
        destination.extend(chunk.iter().zip(mask).map(|(value, mask)| value ^ mask));
    }
    Ok(destination)
}

/// Encrypts data using AES-CTR.
pub fn aes_encrypt_with_ctr(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, EncryptError> {
    aes_crypt_with_ctr(data, key, iv)
}

/// Decrypts data using AES-CTR.
pub fn aes_decrypt_with_ctr(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, EncryptError> {
    aes_crypt_with_ctr(data, key, iv)
}

/// Encrypts data using full-block AES-CFB.
pub fn aes_encrypt_with_cfb(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, EncryptError> {
    let cipher = AesCipher::new(key)?;
    let mut feedback = validate_iv(iv, "cipher.newCFB: IV length must equal block size");
    let mut destination = Vec::with_capacity(data.len());
    for chunk in data.chunks(AES_BLOCK_SIZE) {
        let mut mask = feedback;
        cipher.encrypt_block(&mut mask);
        let start = destination.len();
        destination.extend(chunk.iter().zip(mask).map(|(value, mask)| value ^ mask));
        if chunk.len() == AES_BLOCK_SIZE {
            feedback.copy_from_slice(&destination[start..start + AES_BLOCK_SIZE]);
        }
    }
    Ok(destination)
}

/// Decrypts data using full-block AES-CFB.
pub fn aes_decrypt_with_cfb(data: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, EncryptError> {
    let cipher = AesCipher::new(key)?;
    let mut feedback = validate_iv(iv, "cipher.newCFB: IV length must equal block size");
    let mut destination = Vec::with_capacity(data.len());
    for chunk in data.chunks(AES_BLOCK_SIZE) {
        let mut mask = feedback;
        cipher.encrypt_block(&mut mask);
        destination.extend(chunk.iter().zip(mask).map(|(value, mask)| value ^ mask));
        if chunk.len() == AES_BLOCK_SIZE {
            feedback.copy_from_slice(chunk);
        }
    }
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_block_primitive_keeps_pinned_aes128_vector() {
        let cipher = AesCipher::new(&[0; AES_BLOCK_SIZE]).unwrap();
        let mut block = [0; AES_BLOCK_SIZE];
        cipher.encrypt_block(&mut block);
        assert_eq!(
            block,
            [
                0x66, 0xe9, 0x4b, 0xd4, 0xef, 0x8a, 0x2c, 0x3b, 0x88, 0x4c, 0xfa, 0x59, 0xca, 0x34,
                0x2b, 0x2e,
            ]
        );
        cipher.decrypt_block(&mut block);
        assert_eq!(block, [0; AES_BLOCK_SIZE]);
        assert!(matches!(
            AesCipher::new(&[0; 15]),
            Err(EncryptError::InvalidKeyLength(15))
        ));
    }
}
