// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use super::*;
use crate::codec::collation::gb::{self, GbCollation, GbPolicy};

trait GbkCollator: 'static + Send + Sync + std::fmt::Debug {
    const KIND: GbCollation;
    const IS_CASE_INSENSITIVE: bool;
    const LIKE_PATTERN_MODE: LikePatternMode;
    const NEED_TRUNCATE_INVALID_UTF8_RUNE: bool;
    const WEIGHT_TABLE: &'static [u8; TABLE_SIZE_FOR_GBK];
}

impl<T: GbkCollator> Collator for T {
    type Charset = CharsetGbk;
    type Weight = u16;

    const IS_CASE_INSENSITIVE: bool = T::IS_CASE_INSENSITIVE;
    const LIKE_PATTERN_MODE: LikePatternMode = T::LIKE_PATTERN_MODE;

    #[inline]
    fn char_weight(ch: char) -> Self::Weight {
        // All GBK code point are in BMP, if the incoming character is not, convert it
        // to '?'. This should not happened.
        let r = ch as usize;
        if r > 0xFFFF {
            return '?' as u16;
        }

        (&Self::WEIGHT_TABLE[r * 2..r * 2 + 2]).read_u16().unwrap()
    }

    fn preprocess_sort_key(bstr: &[u8], options: KeyOptions) -> &[u8] {
        prepare_padded_key(bstr, options)
    }

    fn max_sort_key_len(bstr: &[u8]) -> usize {
        utf8_rune_count(bstr) * 2
    }

    #[inline]
    fn write_sort_key_unpadded<W: BufferWriter>(writer: &mut W, bstr: &[u8]) -> Result<usize> {
        gb::write_key_unpadded(T::KIND, GbPolicy::Wire, writer, bstr)
    }

    #[inline]
    fn sort_compare(a: &[u8], b: &[u8], force_no_pad: bool) -> Result<Ordering> {
        gb::compare(T::KIND, GbPolicy::Wire, a, b, force_no_pad)
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
                _ => {
                    if Self::NEED_TRUNCATE_INVALID_UTF8_RUNE {
                        break;
                    }
                    Self::char_weight('?').hash(state);
                    bstr_rest = &bstr_rest[1..]
                }
            }
        }
        Ok(())
    }
}

/// Collator for `gbk_bin` collation with padding behavior (trims right spaces).
#[derive(Debug)]
pub struct CollatorGbkBin;

impl GbkCollator for CollatorGbkBin {
    const KIND: GbCollation = GbCollation::GbkBin;
    const IS_CASE_INSENSITIVE: bool = false;
    const LIKE_PATTERN_MODE: LikePatternMode = LikePatternMode::BinaryRunes;
    const NEED_TRUNCATE_INVALID_UTF8_RUNE: bool = false;
    const WEIGHT_TABLE: &'static [u8; TABLE_SIZE_FOR_GBK] = GBK_BIN_TABLE;
}

/// Collator for `gbk_chinese_ci` collation with padding behavior (trims right
/// spaces).
#[derive(Debug)]
pub struct CollatorGbkChineseCi;

impl GbkCollator for CollatorGbkChineseCi {
    const KIND: GbCollation = GbCollation::GbkChineseCi;
    const IS_CASE_INSENSITIVE: bool = true;
    const LIKE_PATTERN_MODE: LikePatternMode = LikePatternMode::CollatorDefined;
    const NEED_TRUNCATE_INVALID_UTF8_RUNE: bool = true;
    const WEIGHT_TABLE: &'static [u8; TABLE_SIZE_FOR_GBK] = GBK_CHINESE_CI_TABLE;
}

const TABLE_SIZE_FOR_GBK: usize = (0xffff + 1) * 2;

// Existing wire collation weights, including 0x80 for the euro sign. Native
// encoding rejects euro instead; the shared kernel preserves that distinction.
// Unmapped wire code points retain the existing 0x3F(?) weight.
const GBK_BIN_TABLE: &[u8; TABLE_SIZE_FOR_GBK] = include_bytes!("gbk_bin.data");

// GBK_CHINESE_CI_TABLE are the sort key tables for GBK codepoint.
// If there is no mapping code in GBK, use 0x3F(?) instead. It should not
// happened.
const GBK_CHINESE_CI_TABLE: &[u8; TABLE_SIZE_FOR_GBK] = include_bytes!("gbk_chinese_ci.data");
