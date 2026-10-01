// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
// Copyright 2026 PingCAP, Inc. (relocated parser-auth implementation).

//! Pure source-compatible password, digest and legacy SQL compatibility leaves.
//! This is not a general cryptographic framework or a FIPS provider.

pub mod aes;
mod mysql_rng;
mod sql_crypt;
mod vitess;

use std::fmt;

pub use mysql_rng::{MySqlRand, mysql_rand_step};
use sha1::{Digest, Sha1};
pub use sql_crypt::{sql_decode, sql_encode};
pub use vitess::hash_uint64;

/// Calculates SHA-1 using the same byte contract as Go's helper.
pub fn sha1_hash(input: &[u8]) -> [u8; 20] {
    Sha1::digest(input).into()
}

/// Encodes plaintext bytes as MySQL's uppercase `*SHA1(SHA1(password))` form.
pub fn encode_password_bytes(password: &[u8]) -> String {
    if password.is_empty() {
        return String::new();
    }
    let stage_one = sha1_hash(password);
    let stage_two = sha1_hash(&stage_one);
    let mut encoded = String::with_capacity(41);
    encoded.push('*');
    for byte in stage_two {
        use fmt::Write as _;
        let _ = write!(encoded, "{byte:02X}");
    }
    encoded
}

/// Incremental SM3 state transcreated from TiDB's Go implementation.
#[derive(Debug, Clone)]
pub struct Sm3 {
    digest: [u32; 8],
    length_bits: u64,
    pending: Vec<u8>,
}

impl Default for Sm3 {
    fn default() -> Self {
        Self {
            digest: [
                0x7380_166f,
                0x4914_b2b9,
                0x1724_42d7,
                0xda8a_0600,
                0xa96f_30bc,
                0x1631_38aa,
                0xe38d_ee4d,
                0xb0fb_0e4e,
            ],
            length_bits: 0,
            pending: Vec::new(),
        }
    }
}

impl Sm3 {
    /// Underlying block size.
    pub const fn block_size(&self) -> usize {
        64
    }

    /// Digest byte size.
    pub const fn size(&self) -> usize {
        32
    }

    /// Resets the state to the SM3 initialization vector.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Adds bytes to the running hash and returns their source byte count.
    pub fn write(&mut self, input: &[u8]) -> usize {
        self.length_bits = self
            .length_bits
            .wrapping_add((input.len() as u64).wrapping_mul(8));
        self.pending.extend_from_slice(input);
        let complete = self.pending.len() / 64 * 64;
        if complete != 0 {
            let blocks = self.pending[..complete].to_vec();
            compress_sm3_blocks(&mut self.digest, &blocks);
            self.pending.drain(..complete);
        }
        input.len()
    }

    /// Mirrors Go's concrete `Sum`: input is written, but only the digest
    /// returns.
    pub fn sum(&mut self, input: &[u8]) -> Vec<u8> {
        self.write(input);
        let mut final_digest = self.digest;
        let mut padded = self.pending.clone();
        padded.push(0x80);
        while padded.len() % 64 != 56 {
            padded.push(0);
        }
        padded.extend_from_slice(&self.length_bits.to_be_bytes());
        compress_sm3_blocks(&mut final_digest, &padded);
        let mut output = Vec::with_capacity(32);
        for word in final_digest {
            output.extend_from_slice(&word.to_be_bytes());
        }
        output
    }
}

/// Constructs a reset SM3 state.
pub fn new_sm3() -> Sm3 {
    Sm3::default()
}

/// Calculates one SM3 digest.
pub fn sm3_hash(input: &[u8]) -> [u8; 32] {
    let mut hasher = new_sm3();
    hasher.write(input);
    hasher.sum(&[]).try_into().expect("SM3 output is 32 bytes")
}

fn p0(value: u32) -> u32 {
    value ^ value.rotate_left(9) ^ value.rotate_left(17)
}

fn p1(value: u32) -> u32 {
    value ^ value.rotate_left(15) ^ value.rotate_left(23)
}

fn compress_sm3_blocks(digest: &mut [u32; 8], mut input: &[u8]) {
    while input.len() >= 64 {
        let mut w = [0_u32; 68];
        let mut w1 = [0_u32; 64];
        for (index, chunk) in input[..64].as_chunks::<4>().0.iter().enumerate() {
            w[index] = u32::from_be_bytes(*chunk);
        }
        for index in 16..68 {
            w[index] = p1(w[index - 16] ^ w[index - 9] ^ w[index - 3].rotate_left(15))
                ^ w[index - 13].rotate_left(7)
                ^ w[index - 6];
        }
        for index in 0..64 {
            w1[index] = w[index] ^ w[index + 4];
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *digest;
        for index in 0..64 {
            let constant: u32 = if index < 16 { 0x79cc_4519 } else { 0x7a87_9d8a };
            let ss1 = a
                .rotate_left(12)
                .wrapping_add(e)
                .wrapping_add(constant.rotate_left(index as u32))
                .rotate_left(7);
            let ss2 = ss1 ^ a.rotate_left(12);
            let ff = if index < 16 {
                a ^ b ^ c
            } else {
                (a & b) | (a & c) | (b & c)
            };
            let gg = if index < 16 {
                e ^ f ^ g
            } else {
                (e & f) | ((!e) & g)
            };
            let tt1 = ff.wrapping_add(d).wrapping_add(ss2).wrapping_add(w1[index]);
            let tt2 = gg.wrapping_add(h).wrapping_add(ss1).wrapping_add(w[index]);
            d = c;
            c = b.rotate_left(9);
            b = a;
            a = tt1;
            h = g;
            g = f.rotate_left(19);
            f = e;
            e = p0(tt2);
        }
        digest[0] ^= a;
        digest[1] ^= b;
        digest[2] ^= c;
        digest[3] ^= d;
        digest[4] ^= e;
        digest[5] ^= f;
        digest[6] ^= g;
        digest[7] ^= h;
        input = &input[64..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(input: &[u8]) -> String {
        input.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn password_and_sha1_source_literals() {
        // Pinned auth and preexisting SHA1/PASSWORD fixture literals.
        assert_eq!(
            hex(&sha1_hash(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            hex(&sha1_hash(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(encode_password_bytes(b""), "");
        assert_eq!(
            encode_password_bytes(b"123"),
            "*23AE809DDACAF96AF0FD78ED04B6A265E05AA257"
        );
        assert_eq!(
            encode_password_bytes(b"abc"),
            "*0D3CED9BEC10A777AEC23CCC353A8C08A633045E"
        );
        assert_eq!(
            encode_password_bytes(b"\xff\x00a"),
            "*F5A241511384DB827F22D2A2188A456E87F7D4F2"
        );
    }

    #[test]
    fn sm3_source_literals_and_mutating_sum_contract() {
        const ABC: &str = "66c7f0f462eeedd9d1f2d46bdc10e4e24167c4875cf2f7a2297da02b8f4ba8e0";
        const BLOCK: &str = "debe9ff92275b8a138604889c18e5a4d6fdb70e5387e5765293dcba39c0c5732";
        let input = *b"abc";
        let block = b"abcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd";
        assert_eq!(hex(&sm3_hash(&input)), ABC);
        assert_eq!(hex(&sm3_hash(block)), BLOCK);
        let mut state = new_sm3();
        assert_eq!(state.block_size(), 64);
        assert_eq!(state.size(), 32);
        assert_eq!(hex(&state.sum(&input)), ABC);
        assert_eq!(input, *b"abc");
        assert_eq!(hex(&state.sum(&[])), ABC);
        // Sum consumed abc into the live state: write continues it, not a reset.
        assert_eq!(state.write(&block[3..]), 61);
        assert_eq!(hex(&state.sum(&[])), BLOCK);
        let mut cloned = state.clone();
        assert_eq!(hex(&cloned.sum(&[])), BLOCK);
        state.reset();
        assert_eq!(state.write(b"a"), 1);
        assert_eq!(hex(&state.sum(b"bc")), ABC);
        assert_eq!(state.write(&[]), 0);
        assert_eq!(hex(&state.sum(&[])), ABC);
        state.reset();
        for chunk in block.chunks(3) {
            assert_eq!(state.write(chunk), chunk.len());
        }
        assert_eq!(hex(&state.sum(&[])), BLOCK);
    }
}
