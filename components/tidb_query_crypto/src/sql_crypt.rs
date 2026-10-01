// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
// Copyright 2026 PingCAP, Inc. (relocated native legacy SQL codec).

//! Historical ENCODE/DECODE compatibility, not secure encryption.
//! Password seeding and permutations preserve the native source policy;
//! the recurrence is shared with TiKV RAND. FIPS behavior is not verified.

use crate::MySqlRand;

#[derive(Clone, Copy)]
struct RandStruct(MySqlRand);

impl Default for RandStruct {
    fn default() -> Self {
        Self(MySqlRand::from_seeds(0, 0))
    }
}

impl RandStruct {
    fn random_init(&mut self, password: &[u8]) {
        let mut nr = 1_345_345_333_u32;
        let mut add = 7_u32;
        let mut nr2 = 0x1234_5671_u32;

        for password_byte in password {
            if matches!(*password_byte, b' ' | b'\t') {
                continue;
            }
            let value = u32::from(*password_byte);
            nr ^= (nr & 63)
                .wrapping_add(add)
                .wrapping_mul(value)
                .wrapping_add(nr << 8);
            nr2 = nr2.wrapping_add((nr2 << 8) ^ nr);
            add = add.wrapping_add(value);
        }

        let seed1 = nr & ((1_u32 << 31) - 1);
        let seed2 = nr2 & ((1_u32 << 31) - 1);
        self.0 = MySqlRand::from_seeds(seed1, seed2);
    }

    fn my_rand(&mut self) -> f64 {
        self.0.next_f64()
    }
}

struct SqlCrypt {
    random: RandStruct,
    original_random: RandStruct,
    decode_buffer: [u8; 256],
    encode_buffer: [u8; 256],
    shift: u32,
}

impl Default for SqlCrypt {
    fn default() -> Self {
        Self {
            random: RandStruct::default(),
            original_random: RandStruct::default(),
            decode_buffer: [0; 256],
            encode_buffer: [0; 256],
            shift: 0,
        }
    }
}

impl SqlCrypt {
    fn init(&mut self, password: &[u8]) {
        self.random.random_init(password);
        for (index, value) in self.decode_buffer.iter_mut().enumerate() {
            *value = index as u8;
        }
        for index in 0..256 {
            let random_index = (self.random.my_rand() * 255.0) as usize;
            self.decode_buffer.swap(random_index, index);
        }
        for index in 0..256 {
            self.encode_buffer[usize::from(self.decode_buffer[index])] = index as u8;
        }
        self.original_random = self.random;
        self.shift = 0;
    }

    fn encode(&mut self, value: &mut [u8]) {
        for byte in value {
            self.shift ^= (self.random.my_rand() * 255.0) as u32;
            let index = u32::from(*byte);
            *byte = self.encode_buffer[index as usize] ^ self.shift as u8;
            self.shift ^= index;
        }
    }

    fn decode(&mut self, value: &mut [u8]) {
        for byte in value {
            self.shift ^= (self.random.my_rand() * 255.0) as u32;
            let index = u32::from(*byte ^ self.shift as u8);
            *byte = self.decode_buffer[index as usize];
            self.shift ^= u32::from(*byte);
        }
    }
}

/// Applies MySQL's historical `DECODE()` transformation to arbitrary bytes.
pub fn sql_decode(value: &[u8], password: &[u8]) -> Vec<u8> {
    let mut crypt = SqlCrypt::default();
    crypt.init(password);
    let mut decoded = value.to_vec();
    crypt.decode(&mut decoded);
    decoded
}

/// Applies MySQL's historical `ENCODE()` inverse transformation.
pub fn sql_encode(value: &[u8], password: &[u8]) -> Vec<u8> {
    let mut crypt = SqlCrypt::default();
    crypt.init(password);
    let mut encoded = value.to_vec();
    crypt.encode(&mut encoded);
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_sql_crypt_vectors() {
        // Original util/encrypt/crypt.rs literals, not provider recordings.
        let decoded = [0x2c, 0x35, 0xb5, 0xa4, 0xad, 0xf3, 0x91];
        assert_eq!(sql_decode(b"pingcap", b"1234567890123456"), decoded);
        assert_eq!(sql_encode(&decoded, b"1234567890123456"), b"pingcap");
        assert_eq!(sql_decode(b"", b""), b"");
        // Hand-derived password whitespace equivalence from random_init.
        assert_eq!(
            sql_decode(b"pingcap", b" \t"),
            [0x4a, 0x77, 0xb5, 0x24, 0xbd, 0x2c, 0x5c]
        );
    }
}
