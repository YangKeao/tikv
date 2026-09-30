// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.
// Copyright 2026 PingCAP, Inc. Licensed under Apache-2.0.

//! Shared GB comparison, key emission and native encoding leaves.
//!
//! `Wire` preserves TiKV's existing collator contracts; `Native` preserves the
//! relocated TiDB Rust implementation, including its codec version and PUA key
//! padding. These policies are not interchangeable. Registry/new-collation mode
//! and signed wire IDs remain in their existing facades, outside this module.

use std::cmp::Ordering;

use codec::prelude::*;
use lazy_static::lazy_static;
use native_codec::{EncoderResult, GB18030, GBK};

use super::{
    Collator, KeyOptions,
    collator::{
        CollatorGb18030Bin, CollatorGb18030ChineseCi, CollatorGbkBin, CollatorGbkChineseCi,
        next_utf8_char, trim_end_padding,
    },
    encoding::gb18030_data::GB18030_TO_UNICODE,
};
use crate::codec::Result;

/// Concrete GB identities, independent of either registry's numeric IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GbCollation {
    GbkBin,
    GbkChineseCi,
    Gb18030Bin,
    Gb18030ChineseCi,
}

impl GbCollation {
    fn is_ci(self) -> bool {
        matches!(self, Self::GbkChineseCi | Self::Gb18030ChineseCi)
    }

    fn encoding(self) -> GbEncoding {
        match self {
            Self::GbkBin | Self::GbkChineseCi => GbEncoding::Gbk,
            Self::Gb18030Bin | Self::Gb18030ChineseCi => GbEncoding::Gb18030,
        }
    }

    fn weight(self, ch: char) -> u32 {
        match self {
            Self::GbkBin => u32::from(CollatorGbkBin::char_weight(ch)),
            Self::GbkChineseCi => u32::from(CollatorGbkChineseCi::char_weight(ch)),
            Self::Gb18030Bin => CollatorGb18030Bin::char_weight(ch),
            Self::Gb18030ChineseCi => CollatorGb18030ChineseCi::char_weight(ch),
        }
    }
}

/// Existing operation policies, not an operand-origin tag or new collation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GbPolicy {
    Wire,
    Native,
}

/// The two GB encoding leaves used by the native transform facade.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GbEncoding {
    Gbk,
    Gb18030,
}

// The canonical 2103-pair table includes these nine wire-only overrides. Native
// uses exactly the original 2094-pair subset, falling through to its own pinned
// codec for these nine entries. Values remain only in the canonical table.
const WIRE_ONLY_OVERRIDE_CODES: [u32; 9] = [
    0xFD9C, 0xFD9D, 0xFD9E, 0xFD9F, 0xFDA0, 0xFE40, 0xFE41, 0xFE47, 0xFE49,
];

lazy_static! {
    // Only a derived index, never a second complete mapping. Byte lookup uses
    // the canonical table's already-sorted byte column directly.
    static ref RUNE_INDEX: Vec<usize> = {
        let mut indices: Vec<_> = (0..GB18030_TO_UNICODE.len()).collect();
        indices.sort_unstable_by_key(|&index| GB18030_TO_UNICODE[index].1);
        indices
    };
}

fn override_allowed(policy: GbPolicy, encoded: u32) -> bool {
    policy == GbPolicy::Wire || !WIRE_ONLY_OVERRIDE_CODES.contains(&encoded)
}

pub(super) fn decode_override(policy: GbPolicy, encoded: u32) -> Option<char> {
    if !override_allowed(policy, encoded) {
        return None;
    }
    GB18030_TO_UNICODE
        .binary_search_by_key(&encoded, |row| row.0)
        .ok()
        .map(|index| GB18030_TO_UNICODE[index].1)
}

pub(super) fn encode_override(policy: GbPolicy, ch: char) -> Option<u32> {
    let index = RUNE_INDEX
        .binary_search_by_key(&ch, |&index| GB18030_TO_UNICODE[index].1)
        .ok()?;
    let encoded = GB18030_TO_UNICODE[RUNE_INDEX[index]].0;
    override_allowed(policy, encoded).then_some(encoded)
}

/// Compare raw input using the selected existing policy. Never compares public
/// sort keys: native GB18030 PUA keys add a NUL that comparison does not use;
/// wire numeric weights also need not have encoded-byte lexicographic order.
pub fn compare(
    kind: GbCollation,
    policy: GbPolicy,
    left: &[u8],
    right: &[u8],
    force_no_pad: bool,
) -> Result<Ordering> {
    let mut left = if force_no_pad {
        left
    } else {
        trim_end_padding(left)
    };
    let mut right = if force_no_pad {
        right
    } else {
        trim_end_padding(right)
    };
    if policy == GbPolicy::Native && !kind.is_ci() {
        // The original native Compare encodes without the key-only PUA NUL.
        let (left, _) = encode_native_replacing(kind.encoding(), left);
        let (right, _) = encode_native_replacing(kind.encoding(), right);
        return Ok(left.cmp(&right));
    }

    while !left.is_empty() && !right.is_empty() {
        let a = next_utf8_char(left);
        let b = next_utf8_char(right);
        if kind.is_ci() && (a.is_none() || b.is_none()) {
            return Ok(Ordering::Equal);
        }
        let (a, next_left) = a.unwrap_or(('?', &left[1..]));
        let (b, next_right) = b.unwrap_or(('?', &right[1..]));
        let order = kind.weight(a).cmp(&kind.weight(b));
        if order != Ordering::Equal {
            return Ok(order);
        }
        left = next_left;
        right = next_right;
    }
    Ok(left.len().cmp(&right.len()))
}

/// Materialize a GB key with explicit PAD SPACE / NoPad selection.
pub fn key(
    kind: GbCollation,
    policy: GbPolicy,
    value: &[u8],
    options: KeyOptions,
) -> Result<Vec<u8>> {
    let value = match options {
        KeyOptions::Default => trim_end_padding(value),
        KeyOptions::NoPad => value,
    };
    let mut key = Vec::new();
    write_key_unpadded(kind, policy, &mut key, value)?;
    Ok(key)
}

/// Append a key for already-prepared input. Wire collator signatures and their
/// preprocessing stay unchanged and delegate only this emission worker.
pub fn write_key_unpadded<W: BufferWriter>(
    kind: GbCollation,
    policy: GbPolicy,
    writer: &mut W,
    value: &[u8],
) -> Result<usize> {
    if policy == GbPolicy::Native && !kind.is_ci() {
        return write_native_binary_key(kind.encoding(), writer, value);
    }
    let mut rest = value;
    let mut written = 0;
    while !rest.is_empty() {
        match next_utf8_char(rest) {
            Some((ch, next)) => {
                let weight = kind.weight(ch);
                if policy == GbPolicy::Native {
                    // Native CI emits the shortest big-endian weight. Current
                    // shared GB18030 weights have no three-byte values; retain
                    // the source rule rather than making that an API promise.
                    let bytes = weight.to_be_bytes();
                    let first = bytes.iter().position(|&byte| byte != 0).unwrap_or(3);
                    writer.write_bytes(&bytes[first..])?;
                    written += bytes.len() - first;
                } else if weight > 0xFFFF {
                    writer.write_u32_be(weight)?;
                    written += 4;
                } else if weight > 0xFF {
                    writer.write_u16_be(weight as u16)?;
                    written += 2;
                } else {
                    writer.write_u8(weight as u8)?;
                    written += 1;
                }
                rest = next;
            }
            None if kind.is_ci() => break,
            None => {
                writer.write_u8(b'?')?;
                written += 1;
                rest = &rest[1..];
            }
        }
    }
    Ok(written)
}

// Relocated native key-only PUA rule from pkg/util/collate/gb18030_bin.go.
const FOUR_BYTE_PUA: [u32; 19] = [
    0xE78D, 0xE78E, 0xE78F, 0xE790, 0xE791, 0xE792, 0xE793, 0xE794, 0xE795, 0xE796, 0xE7C7, 0xE81E,
    0xE826, 0xE82B, 0xE82C, 0xE832, 0xE843, 0xE854, 0xE864,
];

// Go's first-byte width class, deliberately not UTF-8 validation. Shared by
// native GB grouping and the relocated rune-wise GB18030 key path.
fn native_rune_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first < 0xE0 {
        2
    } else if first < 0xF0 {
        3
    } else {
        4
    }
}

fn write_native_binary_key<W: BufferWriter>(
    encoding: GbEncoding,
    writer: &mut W,
    value: &[u8],
) -> Result<usize> {
    if encoding == GbEncoding::Gbk {
        let (bytes, _) = encode_native_replacing(encoding, value);
        writer.write_bytes(&bytes)?;
        return Ok(bytes.len());
    }
    let mut rest = value;
    let mut written = 0;
    while !rest.is_empty() {
        let width = native_rune_len(rest[0]).min(rest.len());
        let rune = std::str::from_utf8(&rest[..width])
            .ok()
            .and_then(|text| text.chars().next());
        let mut chunk = rest[..width].to_vec();
        if matches!(rune, Some(ch) if FOUR_BYTE_PUA.contains(&(ch as u32))) {
            chunk.push(0);
        }
        let (bytes, invalid) = encode_native_replacing(encoding, &chunk);
        if invalid {
            writer.write_u8(b'?')?;
            written += 1;
        } else {
            writer.write_bytes(&bytes)?;
            written += bytes.len();
        }
        rest = &rest[width..];
    }
    Ok(written)
}

fn encode_native_replacing(encoding: GbEncoding, value: &[u8]) -> (Vec<u8>, bool) {
    let mut bytes = Vec::new();
    let mut invalid = false;
    foreach_native(encoding, value, true, |_, converted, valid| {
        if valid {
            bytes.extend_from_slice(converted);
        } else {
            invalid = true;
            bytes.push(b'?');
        }
        true
    });
    (bytes, invalid)
}

/// Visit the exact native GB groups. Invalid groups expose RuneError bytes and
/// `valid=false`; the native facade retains TransformPolicy and error objects.
/// Returning false from the visitor stops before the next source group.
pub fn foreach_native<F>(encoding: GbEncoding, source: &[u8], from_utf8: bool, mut visit: F)
where
    F: FnMut(&[u8], &[u8], bool) -> bool,
{
    let mut offset = 0;
    while offset < source.len() {
        let width = if from_utf8 {
            native_rune_len(source[offset]).min(source.len() - offset)
        } else {
            peek_native(encoding, &source[offset..]).len()
        };
        let width = width.max(1).min(source.len() - offset);
        let group = &source[offset..offset + width];
        let converted = if from_utf8 {
            encode_native_group(encoding, group)
        } else {
            decode_native_group(encoding, group)
        };
        let (bytes, valid) = converted
            .map(|bytes| (bytes, true))
            .unwrap_or_else(|| (b"\xEF\xBF\xBD".to_vec(), false));
        if !visit(group, &bytes, valid) {
            break;
        }
        offset += width;
    }
}

fn encode_native_group(encoding: GbEncoding, source: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(source).ok()?;
    if text.chars().count() != 1 {
        return None;
    }
    let character = text.chars().next()?;
    if encoding == GbEncoding::Gbk && character == '€' {
        return None;
    }
    if encoding == GbEncoding::Gb18030 {
        if let Some(encoded) = encode_override(GbPolicy::Native, character) {
            let bytes = encoded.to_be_bytes();
            let first = bytes.iter().position(|&byte| byte != 0).unwrap_or(3);
            return Some(bytes[first..].to_vec());
        }
    }
    let codec = if encoding == GbEncoding::Gbk {
        GBK
    } else {
        GB18030
    };
    let mut output = [0_u8; 8];
    let (result, read, written) =
        codec
            .new_encoder()
            .encode_from_utf8_without_replacement(text, &mut output, true);
    if result == EncoderResult::InputEmpty && read == source.len() {
        Some(output[..written].to_vec())
    } else {
        None
    }
}

fn decode_native_group(encoding: GbEncoding, source: &[u8]) -> Option<Vec<u8>> {
    if source.first() == Some(&0x80) {
        return None;
    }
    if encoding == GbEncoding::Gb18030 {
        let encoded = source
            .iter()
            .fold(0, |value, &byte| (value << 8) | u32::from(byte));
        if let Some(ch) = decode_override(GbPolicy::Native, encoded) {
            let mut buffer = [0_u8; 4];
            return Some(ch.encode_utf8(&mut buffer).as_bytes().to_vec());
        }
        if source == [0x84, 0x31, 0xA4, 0x37] {
            return Some(b"\xEF\xBF\xBD".to_vec());
        }
    }
    let codec = if encoding == GbEncoding::Gbk {
        GBK
    } else {
        GB18030
    };
    let decoded = codec
        .decode_without_bom_handling_and_without_replacement(source)
        .map(|text| text.into_owned())?;
    if encoding == GbEncoding::Gbk
        && decoded
            .chars()
            .any(|ch| ('\u{E000}'..='\u{F8FF}').contains(&ch))
    {
        None
    } else {
        Some(decoded.into_bytes())
    }
}

/// Native encoded-byte grouping, distinct from both UTF-8 validation and MbLen.
pub fn peek_native(encoding: GbEncoding, source: &[u8]) -> &[u8] {
    if encoding == GbEncoding::Gbk {
        let width = if source.first().is_some_and(|&byte| byte >= 0x80) {
            2
        } else {
            1
        };
        return &source[..source.len().min(width)];
    }
    let Some(&first) = source.first() else {
        return source;
    };
    if first == 0x80 || first == 0xFF || first <= 0x7F {
        return &source[..1];
    }
    if !(0x81..=0xFE).contains(&first) || source.len() < 2 {
        return &source[..1];
    }
    let second = source[1];
    if (0x40..0x7F).contains(&second) || (0x80..=0xFE).contains(&second) {
        return &source[..2];
    }
    if source.len() >= 4
        && (0x30..=0x39).contains(&second)
        && (0x81..=0xFE).contains(&source[2])
        && (0x30..=0x39).contains(&source[3])
    {
        return &source[..4];
    }
    &source[..1]
}

fn gbk_mb_len(source: &[u8]) -> usize {
    if source.len() >= 2
        && (0x81..=0xFE).contains(&source[0])
        && ((0x40..=0x7E).contains(&source[1]) || (0x80..=0xFE).contains(&source[1]))
    {
        2
    } else {
        0
    }
}

/// Preserve native MbLen, including its source-observed panic for truncated
/// four-byte prefixes. This is not the safe Peek/Foreach grouping entrypoint.
pub fn mb_len_native(encoding: GbEncoding, source: &[u8]) -> usize {
    if encoding == GbEncoding::Gbk {
        return gbk_mb_len(source);
    }
    if source.len() < 2 {
        return 0;
    }
    if gbk_mb_len(source) == 2 {
        return 2;
    }
    if (0x81..=0xFE).contains(&source[0])
        && (0x30..=0x39).contains(&source[1])
        && (0x81..=0xFE).contains(&source[2])
        && (0x30..=0x39).contains(&source[3])
    {
        4
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_overrides_have_one_exact_native_subset() {
        assert_eq!(GB18030_TO_UNICODE.len(), 2103);
        assert!(
            GB18030_TO_UNICODE
                .windows(2)
                .all(|pair| pair[0].0 < pair[1].0)
        );
        assert!(
            RUNE_INDEX
                .windows(2)
                .all(|pair| { GB18030_TO_UNICODE[pair[0]].1 < GB18030_TO_UNICODE[pair[1]].1 })
        );
        let mut native = 0;
        for &(encoded, ch) in GB18030_TO_UNICODE {
            assert_eq!(decode_override(GbPolicy::Wire, encoded), Some(ch));
            assert_eq!(encode_override(GbPolicy::Wire, ch), Some(encoded));
            if WIRE_ONLY_OVERRIDE_CODES.contains(&encoded) {
                assert_eq!(decode_override(GbPolicy::Native, encoded), None);
                assert_eq!(encode_override(GbPolicy::Native, ch), None);
            } else {
                native += 1;
                assert_eq!(decode_override(GbPolicy::Native, encoded), Some(ch));
                assert_eq!(encode_override(GbPolicy::Native, ch), Some(encoded));
            }
        }
        assert_eq!(native, 2094);
    }

    #[test]
    fn native_overrides_preserve_all_36_wire_bin_differences_and_19_pua_keys() {
        let mut differing = 0;
        for &(encoded, ch) in GB18030_TO_UNICODE {
            if !override_allowed(GbPolicy::Native, encoded) {
                continue;
            }
            differing += usize::from(CollatorGb18030Bin::char_weight(ch) != encoded);
            let mut utf8 = [0_u8; 4];
            let source = ch.encode_utf8(&mut utf8).as_bytes();
            let bytes = encoded.to_be_bytes();
            let first = bytes.iter().position(|&byte| byte != 0).unwrap_or(3);
            assert_eq!(
                encode_native_group(GbEncoding::Gb18030, source).unwrap(),
                bytes[first..]
            );
        }
        assert_eq!(differing, 36);
        assert_eq!(FOUR_BYTE_PUA.len(), 19);
        for codepoint in FOUR_BYTE_PUA {
            let ch = char::from_u32(codepoint).unwrap();
            let mut utf8 = [0_u8; 4];
            let source = ch.encode_utf8(&mut utf8).as_bytes();
            let encoded = encode_override(GbPolicy::Native, ch).unwrap().to_be_bytes();
            assert_eq!(
                encode_native_replacing(GbEncoding::Gb18030, source),
                (encoded.to_vec(), false)
            );
            let mut expected_key = encoded.to_vec();
            expected_key.push(0);
            assert_eq!(
                key(
                    GbCollation::Gb18030Bin,
                    GbPolicy::Native,
                    source,
                    KeyOptions::NoPad
                )
                .unwrap(),
                expected_key
            );
        }
    }

    #[test]
    fn gb_binary_policies_keep_euro_pua_and_compare_distinct() {
        assert_eq!(
            key(
                GbCollation::GbkBin,
                GbPolicy::Wire,
                "€".as_bytes(),
                KeyOptions::Default
            )
            .unwrap(),
            [0x80]
        );
        assert_eq!(
            key(
                GbCollation::GbkBin,
                GbPolicy::Native,
                "€".as_bytes(),
                KeyOptions::Default
            )
            .unwrap(),
            b"?"
        );
        let pua = "\u{E78D}".as_bytes();
        assert_eq!(
            key(
                GbCollation::Gb18030Bin,
                GbPolicy::Wire,
                pua,
                KeyOptions::Default
            )
            .unwrap(),
            [0xA6, 0xD9]
        );
        assert_eq!(
            key(
                GbCollation::Gb18030Bin,
                GbPolicy::Native,
                pua,
                KeyOptions::Default
            )
            .unwrap(),
            [0x84, 0x31, 0x82, 0x36, 0]
        );
        assert_eq!(
            encode_native_replacing(GbEncoding::Gb18030, pua),
            (vec![0x84, 0x31, 0x82, 0x36], false)
        );
        let a = "\u{80}".as_bytes();
        let b = "中".as_bytes();
        assert_eq!(
            compare(GbCollation::Gb18030Bin, GbPolicy::Wire, a, b, false).unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            compare(GbCollation::Gb18030Bin, GbPolicy::Native, a, b, false).unwrap(),
            Ordering::Less
        );
        assert_eq!(
            key(
                GbCollation::Gb18030Bin,
                GbPolicy::Wire,
                a,
                KeyOptions::Default
            )
            .unwrap()
            .cmp(
                &key(
                    GbCollation::Gb18030Bin,
                    GbPolicy::Wire,
                    b,
                    KeyOptions::Default
                )
                .unwrap()
            ),
            Ordering::Less
        );
    }

    #[test]
    fn gb_shared_padding_invalid_and_native_grouping() {
        for kind in [
            GbCollation::GbkBin,
            GbCollation::GbkChineseCi,
            GbCollation::Gb18030Bin,
            GbCollation::Gb18030ChineseCi,
        ] {
            for policy in [GbPolicy::Wire, GbPolicy::Native] {
                assert_eq!(
                    compare(kind, policy, b"a ", b"a", false).unwrap(),
                    Ordering::Equal
                );
                assert_eq!(
                    compare(kind, policy, b"a ", b"a", true).unwrap(),
                    Ordering::Greater
                );
                assert_eq!(
                    key(kind, policy, b"a ", KeyOptions::Default).unwrap(),
                    key(kind, policy, b"a", KeyOptions::Default).unwrap()
                );
                assert_ne!(
                    key(kind, policy, b"a ", KeyOptions::NoPad).unwrap(),
                    key(kind, policy, b"a", KeyOptions::NoPad).unwrap()
                );
                if kind.is_ci() {
                    assert_eq!(
                        compare(kind, policy, b"\xff", b"x", false).unwrap(),
                        Ordering::Equal
                    );
                    assert_eq!(
                        key(kind, policy, b"a\xffz", KeyOptions::Default).unwrap(),
                        key(kind, policy, b"a", KeyOptions::Default).unwrap()
                    );
                }
            }
        }
        let mut groups = Vec::new();
        foreach_native(GbEncoding::Gbk, b"\x80az", true, |from, to, valid| {
            groups.push((from.to_vec(), to.to_vec(), valid));
            false
        });
        assert_eq!(
            groups,
            vec![(b"\x80a".to_vec(), b"\xEF\xBF\xBD".to_vec(), false)]
        );
        assert_eq!(peek_native(GbEncoding::Gb18030, &[0x81, 0x30]), &[0x81]);
        for value in [&[0x81, 0x30][..], &[0x81, 0x30, 0x81][..]] {
            assert!(
                std::panic::catch_unwind(|| mb_len_native(GbEncoding::Gb18030, value)).is_err()
            );
        }
    }
}
