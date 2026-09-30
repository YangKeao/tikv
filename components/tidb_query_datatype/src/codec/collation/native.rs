// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
// Copyright 2026 PingCAP, Inc. Licensed under Apache-2.0.

//! Native collation policy selection over the existing shared kernels.
//!
//! The sixteen identities and explicit small tags are independent of registry
//! IDs, signed wire IDs, input origin, and the global new-collation switch.
//! Native facades resolve those concerns before selecting a policy here.

use std::{borrow::Cow, cmp::Ordering};

use super::{
    Collator, KeyOptions, LikePatternMode,
    collator::*,
    gb::{self, GbCollation, GbPolicy},
    pattern::{self, CompiledPattern, MatchOptions, TrailingEscape},
};
use crate::codec::Result;

/// Native comparison/key policy, not a registry or input-origin identifier.
/// Aliases retain distinct stable tags while sharing their existing kernels.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NativeCollation {
    Binary,
    AsciiBin,
    Latin1Bin,
    Utf8Bin,
    Utf8GeneralCi,
    Utf8UnicodeCi,
    Utf8Mb4Bin,
    Utf8Mb4GeneralCi,
    Utf8Mb4UnicodeCi,
    Utf8Mb40900AiCi,
    Utf8Mb40900Bin,
    /// Reserved identity only: operations retain the original panic stub.
    Utf8Mb4ZhPinyinTiDbAsCs,
    GbkBin,
    GbkChineseCi,
    Gb18030Bin,
    Gb18030ChineseCi,
}

// This is the relocated native selector. No corresponding selector remains in
// the native facade, which maps its sixteen metadata identities to this enum.
macro_rules! with_native_collator {
    ($collation:expr, $C:ident, $body:expr) => {{
        match $collation {
            NativeCollation::Binary => {
                type $C = CollatorBinary;
                $body
            }
            NativeCollation::AsciiBin | NativeCollation::Utf8Bin | NativeCollation::Utf8Mb4Bin => {
                type $C = CollatorUtf8Mb4Bin;
                $body
            }
            NativeCollation::Latin1Bin => {
                type $C = CollatorLatin1Bin;
                $body
            }
            NativeCollation::Utf8Mb40900Bin => {
                type $C = CollatorUtf8Mb4BinNoPadding;
                $body
            }
            NativeCollation::Utf8GeneralCi | NativeCollation::Utf8Mb4GeneralCi => {
                type $C = CollatorUtf8Mb4GeneralCi;
                $body
            }
            NativeCollation::Utf8UnicodeCi | NativeCollation::Utf8Mb4UnicodeCi => {
                type $C = CollatorUtf8Mb4UnicodeCi;
                $body
            }
            NativeCollation::Utf8Mb40900AiCi => {
                type $C = CollatorUtf8Mb40900AiCi;
                $body
            }
            NativeCollation::GbkBin => {
                type $C = CollatorGbkBin;
                $body
            }
            NativeCollation::GbkChineseCi => {
                type $C = CollatorGbkChineseCi;
                $body
            }
            NativeCollation::Gb18030Bin => {
                type $C = CollatorGb18030Bin;
                $body
            }
            NativeCollation::Gb18030ChineseCi => {
                type $C = CollatorGb18030ChineseCi;
                $body
            }
            NativeCollation::Utf8Mb4ZhPinyinTiDbAsCs => panic!("implement me"),
        }
    }};
}

impl NativeCollation {
    /// Stable carrier tag. Deliberately does not cast an enum discriminant.
    pub const fn tag(self) -> i64 {
        match self {
            Self::Binary => 0,
            Self::AsciiBin => 1,
            Self::Latin1Bin => 2,
            Self::Utf8Bin => 3,
            Self::Utf8GeneralCi => 4,
            Self::Utf8UnicodeCi => 5,
            Self::Utf8Mb4Bin => 6,
            Self::Utf8Mb4GeneralCi => 7,
            Self::Utf8Mb4UnicodeCi => 8,
            Self::Utf8Mb40900AiCi => 9,
            Self::Utf8Mb40900Bin => 10,
            Self::Utf8Mb4ZhPinyinTiDbAsCs => 11,
            Self::GbkBin => 12,
            Self::GbkChineseCi => 13,
            Self::Gb18030Bin => 14,
            Self::Gb18030ChineseCi => 15,
        }
    }

    /// Decode only this tag domain. Unknown values are not normalized or
    /// interpreted as wire/registry IDs. A Pinyin tag is not implemented
    /// support.
    pub const fn from_tag(tag: i64) -> Option<Self> {
        Some(match tag {
            0 => Self::Binary,
            1 => Self::AsciiBin,
            2 => Self::Latin1Bin,
            3 => Self::Utf8Bin,
            4 => Self::Utf8GeneralCi,
            5 => Self::Utf8UnicodeCi,
            6 => Self::Utf8Mb4Bin,
            7 => Self::Utf8Mb4GeneralCi,
            8 => Self::Utf8Mb4UnicodeCi,
            9 => Self::Utf8Mb40900AiCi,
            10 => Self::Utf8Mb40900Bin,
            11 => Self::Utf8Mb4ZhPinyinTiDbAsCs,
            12 => Self::GbkBin,
            13 => Self::GbkChineseCi,
            14 => Self::Gb18030Bin,
            15 => Self::Gb18030ChineseCi,
            _ => return None,
        })
    }

    const fn gb_collation(self) -> Option<GbCollation> {
        Some(match self {
            Self::GbkBin => GbCollation::GbkBin,
            Self::GbkChineseCi => GbCollation::GbkChineseCi,
            Self::Gb18030Bin => GbCollation::Gb18030Bin,
            Self::Gb18030ChineseCi => GbCollation::Gb18030ChineseCi,
            _ => return None,
        })
    }

    /// Compare raw Go-string bytes with the explicit native policy. This does
    /// not consult the global mode and does not compare materialized keys.
    pub fn compare(self, left: &[u8], right: &[u8]) -> Result<Ordering> {
        match self.gb_collation() {
            Some(kind) => gb::compare(kind, GbPolicy::Native, left, right, false),
            None => with_native_collator!(self, C, C::sort_compare(left, right, false)),
        }
    }

    /// Produce the native key with explicit default/NoPad behavior. Consumers
    /// such as FIND compare NoPad keys, not the result of `compare`.
    pub fn key(self, value: &[u8], options: KeyOptions) -> Result<Vec<u8>> {
        match self.gb_collation() {
            Some(kind) => gb::key(kind, GbPolicy::Native, value, options),
            None => with_native_collator!(self, C, C::sort_key_with_options(value, options)),
        }
    }

    /// Retain native immutable-key ownership, including always-owned GB keys.
    pub fn key_cow(self, value: &[u8], options: KeyOptions) -> Result<Cow<'_, [u8]>> {
        match self.gb_collation() {
            Some(_) => self.key(value, options).map(Cow::Owned),
            None => with_native_collator!(self, C, C::sort_key_cow(value, options)),
        }
    }

    /// Historical source allocation estimate, not a new encoded-length promise.
    pub fn max_key_len(self, value: &[u8]) -> usize {
        with_native_collator!(self, C, C::max_sort_key_len(value))
    }

    /// Exact typed form of native `collation.rs::is_ci_collation`: the seven
    /// General/Unicode/0900-AI/GB Chinese CI identities. The selected kernels'
    /// constants agree with that source whitelist; no name suffix is inferred.
    pub const fn is_ci(self) -> bool {
        match self {
            Self::Utf8Mb4ZhPinyinTiDbAsCs => false,
            _ => with_native_collator!(self, C, C::IS_CASE_INSENSITIVE),
        }
    }

    /// The existing Pinyin capability query remains non-panicking and false.
    pub const fn can_use_raw_mem_as_key(self) -> bool {
        match self {
            Self::Utf8Mb4ZhPinyinTiDbAsCs => false,
            _ => with_native_collator!(self, C, C::CAN_USE_RAW_MEM_AS_KEY),
        }
    }

    /// Compile with the native source's escape and LIKE decoding policy.
    pub fn compile_pattern(self, value: &[u8], escape: u8) -> CompiledPattern {
        with_native_collator!(self, C, compile_native_pattern::<C>(value, escape))
    }

    /// Match without compiling or allocating target runes; no registry lookup.
    pub fn matches_pattern(self, value: &[u8], pattern: &[u8], escape: u8) -> Result<bool> {
        with_native_collator!(self, C, match_native_pattern::<C>(value, pattern, escape))
    }
}

fn pattern_options(escape: u8) -> MatchOptions {
    MatchOptions {
        escape: u32::from(escape),
        trailing_escape: TrailingEscape::Literal,
    }
}

fn compile_native_pattern<C: Collator>(value: &[u8], escape: u8) -> CompiledPattern {
    let options = pattern_options(escape);
    match C::LIKE_PATTERN_MODE {
        LikePatternMode::Bytes => pattern::compile::<
            CollatorBinary,
            <CollatorBinary as Collator>::Charset,
        >(value, options),
        LikePatternMode::BinaryRunes => pattern::compile::<
            CollatorUtf8Mb4BinNoPadding,
            <CollatorUtf8Mb4BinNoPadding as Collator>::Charset,
        >(value, options),
        LikePatternMode::CollatorDefined => pattern::compile::<C, C::Charset>(value, options),
    }
}

fn match_native_pattern<C: Collator>(value: &[u8], pattern: &[u8], escape: u8) -> Result<bool> {
    let options = pattern_options(escape);
    match C::LIKE_PATTERN_MODE {
        LikePatternMode::Bytes => pattern::matches_raw::<
            CollatorBinary,
            <CollatorBinary as Collator>::Charset,
        >(value, pattern, options),
        LikePatternMode::BinaryRunes => pattern::matches_raw::<
            CollatorUtf8Mb4BinNoPadding,
            <CollatorUtf8Mb4BinNoPadding as Collator>::Charset,
        >(value, pattern, options),
        LikePatternMode::CollatorDefined => {
            pattern::matches_raw::<C, C::Charset>(value, pattern, options)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_policy_tags_are_explicit_and_checked() {
        use NativeCollation::*;
        for (policy, tag) in [
            (Binary, 0),
            (AsciiBin, 1),
            (Latin1Bin, 2),
            (Utf8Bin, 3),
            (Utf8GeneralCi, 4),
            (Utf8UnicodeCi, 5),
            (Utf8Mb4Bin, 6),
            (Utf8Mb4GeneralCi, 7),
            (Utf8Mb4UnicodeCi, 8),
            (Utf8Mb40900AiCi, 9),
            (Utf8Mb40900Bin, 10),
            (Utf8Mb4ZhPinyinTiDbAsCs, 11),
            (GbkBin, 12),
            (GbkChineseCi, 13),
            (Gb18030Bin, 14),
            (Gb18030ChineseCi, 15),
        ] {
            assert_eq!(policy.tag(), tag);
            assert_eq!(NativeCollation::from_tag(tag), Some(policy));
            // Exact original native CI whitelist, including false for Pinyin.
            assert_eq!(policy.is_ci(), matches!(tag, 4 | 5 | 7 | 8 | 9 | 13 | 15));
        }
        for tag in [
            i64::MIN,
            -249,
            -63,
            -1,
            16,
            28,
            45,
            63,
            65,
            83,
            249,
            2048,
            i64::MAX,
        ] {
            assert_eq!(NativeCollation::from_tag(tag), None, "{tag}");
        }
    }

    #[test]
    fn native_policy_aliases_padding_and_gb_keep_existing_contracts() {
        use NativeCollation::*;
        for (a, b) in [
            (AsciiBin, Utf8Bin),
            (Utf8Bin, Utf8Mb4Bin),
            (Utf8GeneralCi, Utf8Mb4GeneralCi),
            (Utf8UnicodeCi, Utf8Mb4UnicodeCi),
        ] {
            assert_eq!(
                a.compare("中a ".as_bytes(), b"A").unwrap(),
                b.compare("中a ".as_bytes(), b"A").unwrap()
            );
            for options in [KeyOptions::Default, KeyOptions::NoPad] {
                assert_eq!(
                    a.key("中a ".as_bytes(), options).unwrap(),
                    b.key("中a ".as_bytes(), options).unwrap()
                );
            }
        }
        assert_eq!(Binary.compare(b"a ", b"a").unwrap(), Ordering::Greater);
        assert_eq!(Utf8Bin.compare(b"a ", b"a").unwrap(), Ordering::Equal);
        assert_eq!(Utf8Bin.key(b"a ", KeyOptions::NoPad).unwrap(), b"a ");
        assert_eq!(
            Utf8GeneralCi.compare("é".as_bytes(), b"E").unwrap(),
            Ordering::Equal
        );
        assert_eq!(
            GbkBin.key("€".as_bytes(), KeyOptions::Default).unwrap(),
            b"?"
        );
        assert_eq!(
            Gb18030Bin
                .key("\u{E78D}".as_bytes(), KeyOptions::NoPad)
                .unwrap(),
            [0x84, 0x31, 0x82, 0x36, 0]
        );
        assert_eq!(
            Gb18030Bin
                .compare("\u{80}".as_bytes(), "中".as_bytes())
                .unwrap(),
            Ordering::Less
        );
        for ci in [GbkChineseCi, Gb18030ChineseCi] {
            assert_eq!(ci.compare(b"A", b"a").unwrap(), Ordering::Equal);
            assert_eq!(ci.compare(b"\xff", b"x").unwrap(), Ordering::Equal);
        }
    }

    #[test]
    fn native_policy_pattern_and_cow_keep_selector_semantics() {
        use NativeCollation::*;
        for (policy, expected) in [
            (Binary, false),
            (Utf8Bin, true),
            (GbkBin, true),
            (Gb18030Bin, false),
        ] {
            let compiled = policy.compile_pattern(b"_", b'\\');
            assert_eq!(compiled.is_match("中".as_bytes()).unwrap(), expected);
            assert_eq!(
                policy
                    .matches_pattern("中".as_bytes(), b"_", b'\\')
                    .unwrap(),
                expected
            );
        }
        assert!(Utf8GeneralCi.matches_pattern(b"a", b"A", b'\\').unwrap());
        let binary_key = Binary.key_cow(b"a ", KeyOptions::Default).unwrap();
        assert!(matches!(&binary_key, Cow::Borrowed(_)));
        assert_eq!(binary_key.as_ref(), b"a ");
        let padded_key = Utf8Bin.key_cow(b"a ", KeyOptions::Default).unwrap();
        assert!(matches!(&padded_key, Cow::Borrowed(_)));
        assert_eq!(padded_key.as_ref(), b"a");
        for gb in [GbkBin, GbkChineseCi, Gb18030Bin, Gb18030ChineseCi] {
            assert!(matches!(
                gb.key_cow(b"a", KeyOptions::Default).unwrap(),
                Cow::Owned(_)
            ));
            assert!(!gb.can_use_raw_mem_as_key());
        }
        assert_eq!(GbkBin.max_key_len("中a ".as_bytes()), 6);
        assert_eq!(Gb18030Bin.max_key_len("中a ".as_bytes()), 12);
        assert!(Binary.can_use_raw_mem_as_key());
    }

    #[test]
    fn native_policy_pinyin_is_still_a_stub_not_a_fallback() {
        let pinyin = NativeCollation::Utf8Mb4ZhPinyinTiDbAsCs;
        assert!(!pinyin.can_use_raw_mem_as_key());
        assert!(std::panic::catch_unwind(|| pinyin.compare(b"a", b"a")).is_err());
        assert!(std::panic::catch_unwind(|| pinyin.key(b"a", KeyOptions::Default)).is_err());
        assert!(std::panic::catch_unwind(|| pinyin.key_cow(b"a", KeyOptions::Default)).is_err());
        assert!(std::panic::catch_unwind(|| pinyin.max_key_len(b"a")).is_err());
        assert!(std::panic::catch_unwind(|| pinyin.compile_pattern(b"a", b'\\')).is_err());
        assert!(std::panic::catch_unwind(|| pinyin.matches_pattern(b"a", b"a", b'\\')).is_err());
    }
}
