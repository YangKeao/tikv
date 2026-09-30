// Copyright 2024 TiKV Project Authors. Licensed under Apache-2.0.

use super::*;
use crate::codec::collation::gb::{self, GbCollation, GbPolicy};

/// Collator for `gb18030_bin`
#[derive(Debug)]
pub struct CollatorGb18030Bin;

impl Collator for CollatorGb18030Bin {
    type Charset = CharsetGb18030;
    type Weight = u32;
    const IS_CASE_INSENSITIVE: bool = false;
    const LIKE_PATTERN_MODE: LikePatternMode = LikePatternMode::Bytes;

    #[inline]
    fn char_weight(ch: char) -> u32 {
        // If the incoming character is not, convert it to '?'. This should not
        // happened.
        let r = ch as usize;
        if r > 0x10FFFF {
            return '?' as u32;
        }

        (&GB18030_BIN_TABLE[r * 4..r * 4 + 4])
            .read_u32_le()
            .unwrap()
    }

    fn preprocess_sort_key(bstr: &[u8], options: KeyOptions) -> &[u8] {
        prepare_padded_key(bstr, options)
    }

    fn max_sort_key_len(bstr: &[u8]) -> usize {
        utf8_rune_count(bstr) * 4
    }

    #[inline]
    fn write_sort_key_unpadded<W: BufferWriter>(writer: &mut W, bstr: &[u8]) -> Result<usize> {
        gb::write_key_unpadded(GbCollation::Gb18030Bin, GbPolicy::Wire, writer, bstr)
    }

    #[inline]
    fn sort_compare(a: &[u8], b: &[u8], force_no_pad: bool) -> Result<Ordering> {
        gb::compare(GbCollation::Gb18030Bin, GbPolicy::Wire, a, b, force_no_pad)
    }

    #[inline]
    fn sort_hash<H: Hasher>(state: &mut H, bstr: &[u8]) -> Result<()> {
        let mut bstr_rest = trim_end_padding(bstr);
        while !bstr_rest.is_empty() {
            match next_utf8_char(bstr_rest) {
                Some((ch_b, b_next)) => {
                    Self::char_weight(ch_b).hash(state);
                    bstr_rest = b_next
                }
                None => {
                    Self::char_weight('?').hash(state);
                    bstr_rest = &bstr_rest[1..];
                }
            }
        }
        Ok(())
    }
}

/// Collator for `gb18030_chinese_ci`
#[derive(Debug)]
pub struct CollatorGb18030ChineseCi;

impl Collator for CollatorGb18030ChineseCi {
    type Charset = CharsetGb18030;
    type Weight = u32;
    const IS_CASE_INSENSITIVE: bool = true;
    const LIKE_PATTERN_MODE: LikePatternMode = LikePatternMode::CollatorDefined;

    #[inline]
    fn char_weight(ch: char) -> u32 {
        // If the incoming character is not, convert it to '?'. This should not
        // happened.
        let r = ch as usize;
        if r > 0x10FFFF {
            return '?' as u32;
        }

        (&GB18030_CHINESE_CI_TABLE[r * 4..r * 4 + 4])
            .read_u32_le()
            .unwrap()
    }

    fn preprocess_sort_key(bstr: &[u8], options: KeyOptions) -> &[u8] {
        prepare_padded_key(bstr, options)
    }

    fn max_sort_key_len(bstr: &[u8]) -> usize {
        utf8_rune_count(bstr) * 4
    }

    #[inline]
    fn write_sort_key_unpadded<W: BufferWriter>(writer: &mut W, bstr: &[u8]) -> Result<usize> {
        gb::write_key_unpadded(GbCollation::Gb18030ChineseCi, GbPolicy::Wire, writer, bstr)
    }

    #[inline]
    fn sort_compare(a: &[u8], b: &[u8], force_no_pad: bool) -> Result<Ordering> {
        gb::compare(
            GbCollation::Gb18030ChineseCi,
            GbPolicy::Wire,
            a,
            b,
            force_no_pad,
        )
    }

    #[inline]
    fn sort_hash<H: Hasher>(state: &mut H, bstr: &[u8]) -> Result<()> {
        let mut bstr_rest = trim_end_padding(bstr);
        while !bstr_rest.is_empty() {
            match next_utf8_char(bstr_rest) {
                Some((ch_b, b_next)) => {
                    Self::char_weight(ch_b).hash(state);
                    bstr_rest = b_next
                }
                _ => break,
            }
        }
        Ok(())
    }
}

const TABLE_SIZE_FOR_GB18030: usize = 4 * (0x10FFFF + 1);

// Existing wire collation weights. They are not the native GB18030 encoder:
// the shared kernel keeps the known override/key differences policy-explicit.
const GB18030_BIN_TABLE: &[u8; TABLE_SIZE_FOR_GB18030] = include_bytes!("gb18030_bin.data");

// GB18030_CHINESE_CI_TABLE are the sort key tables for GB18030 codepoint.
const GB18030_CHINESE_CI_TABLE: &[u8; TABLE_SIZE_FOR_GB18030] =
    include_bytes!("gb18030_chinese_ci.data");

#[cfg(test)]
mod tests {
    use crate::codec::collation::{
        Collator,
        collator::{CollatorGb18030Bin, CollatorGb18030ChineseCi},
    };

    #[test]
    fn test_weight() {
        let cases: Vec<(char, u32, u32)> = vec![
            ('中', 0xFFA09BC1, 0xD6D0),
            ('€', 0xA2E3, 0xA2E3),
            ('', 0xFF001D21, 0x8135F437),
            ('ḿ', 0xFF001D20, 0xA8BC),
            ('ǹ', 0xFF000154, 0xA8BF),
            ('䦃', 0xFFA09E8A, 0xFE89),
        ];

        for (case, exp_chinese_ci, exp_bin) in cases {
            let chinese_ci = CollatorGb18030ChineseCi::char_weight(case);
            let bin = CollatorGb18030Bin::char_weight(case);
            assert_eq!(
                exp_bin, bin,
                "{} expected:{:02X?}, but got:{:02X?}",
                case, exp_bin, bin
            );
            assert_eq!(
                exp_chinese_ci, chinese_ci,
                "{} expected:{:02X?}, but got:{:02X?}",
                case, exp_chinese_ci, chinese_ci
            );
        }
    }
}
