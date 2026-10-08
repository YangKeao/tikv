// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
// Copyright 2026 PingCAP, Inc. (relocated native Vitess hash).

//! Legacy Vitess shard-key compatibility using fixed-key DES.
//! This is a non-security compatibility hash, not data protection or a FIPS
//! primitive.

use std::sync::LazyLock;

use des::{
    Des,
    cipher::{Block, BlockCipherEncrypt, KeyInit},
};

static NULL_KEY_BLOCK: LazyLock<Des> = LazyLock::new(|| {
    Des::new_from_slice(&[0; 8]).expect("DES accepts the fixed-width all-zero Vitess key")
});

/// Implements Vitess' method of calculating a hash used for determining a shard
/// key range: a DES encryption with a 64-bit null key over a 64-bit block.
pub fn hash_uint64(shard_key: u64) -> u64 {
    let mut block = Block::<Des>::default();
    block.copy_from_slice(&shard_key.to_be_bytes());
    NULL_KEY_BLOCK.encrypt_block(&mut block);
    u64::from_be_bytes(block.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_vitess_vectors() {
        // Original native util/vitess.rs fixtures.
        assert_eq!(hash_uint64(30375298039), 0x031265661e5f1133);
        assert_eq!(hash_uint64(1123), 0x031b565d41bdf8ca);
        assert_eq!(hash_uint64(u64::MAX), 0x355550b2150e2451);
    }
}
