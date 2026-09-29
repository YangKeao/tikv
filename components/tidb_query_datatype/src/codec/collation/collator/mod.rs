// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

mod binary;
mod gb18030_collation;
mod gbk_collation;
mod latin1_bin;
mod utf8mb4_binary;
mod utf8mb4_general_ci;
mod utf8mb4_uca;

use std::{
    cmp::Ordering,
    hash::{Hash, Hasher},
};

pub use binary::*;
use codec::prelude::*;
pub use gb18030_collation::*;
pub use gbk_collation::*;
pub use latin1_bin::*;
pub use utf8mb4_binary::*;
pub use utf8mb4_general_ci::*;
pub use utf8mb4_uca::*;

use super::{Collator, KeyOptions, LikePatternMode, charset::*};
use crate::codec::Result;

pub const PADDING_SPACE: char = 0x20 as char;

pub(crate) fn trim_end_padding(mut s: &[u8]) -> &[u8] {
    while s.ends_with(&[PADDING_SPACE as u8]) {
        s = &s[..s.len() - 1];
    }
    s
}

fn prepare_padded_key(s: &[u8], options: KeyOptions) -> &[u8] {
    match options {
        KeyOptions::Default => trim_end_padding(s),
        KeyOptions::NoPad => s,
    }
}

pub(crate) fn next_utf8_char(s: &[u8]) -> Option<(char, &[u8])> {
    let (ch, width) = decode_utf8_rune_strict(s)?;
    Some((ch, &s[width..]))
}

#[cfg(test)]
mod tests {
    use std::{borrow::Cow, hash::Hasher};

    use super::*;
    use crate::{
        Collation,
        codec::collation::{Collator, SortKey},
        match_template_collator,
    };

    fn check_key<C: Collator>(input: &[u8], options: KeyOptions, expected: &[u8], borrowed: bool) {
        let mut output = vec![0xa5];
        let written = C::write_sort_key_with_options(&mut output, input, options).unwrap();
        assert_eq!(written, expected.len());
        assert_eq!(output[0], 0xa5);
        assert_eq!(&output[1..], expected);
        assert_eq!(C::sort_key_with_options(input, options).unwrap(), expected);
        let cow = C::sort_key_cow(input, options).unwrap();
        assert_eq!(cow.as_ref(), expected);
        assert_eq!(matches!(cow, Cow::Borrowed(_)), borrowed);
        if borrowed {
            assert_eq!(cow.as_ptr(), input.as_ptr());
        }
        if options == KeyOptions::Default {
            assert_eq!(C::sort_key(input).unwrap(), expected);
            let mut legacy_writer = Vec::new();
            assert_eq!(
                C::write_sort_key(&mut legacy_writer, input).unwrap(),
                expected.len()
            );
            assert_eq!(legacy_writer, expected);
        }
    }

    #[test]
    fn test_shared_key_options_and_cow() {
        for input in [
            b"".as_slice(),
            b"a ",
            b"a\t",
            b"a\0",
            b"\xff ",
            "a\u{a0}".as_bytes(),
            "a\u{3000}".as_bytes(),
        ] {
            for option in [KeyOptions::Default, KeyOptions::NoPad] {
                check_key::<CollatorBinary>(input, option, input, true);
                check_key::<CollatorUtf8Mb4BinNoPadding>(input, option, input, true);
                let padded = if option == KeyOptions::Default {
                    trim_end_padding(input)
                } else {
                    input
                };
                check_key::<CollatorUtf8Mb4Bin>(input, option, padded, true);
                check_key::<CollatorLatin1Bin>(input, option, padded, true);
            }
        }
        check_key::<CollatorUtf8Mb4GeneralCi>(b"a ", KeyOptions::Default, b"\0A", false);
        check_key::<CollatorUtf8Mb4GeneralCi>(b"a ", KeyOptions::NoPad, b"\0A\0 ", false);
        check_key::<CollatorUtf8Mb4UnicodeCi>(b"a ", KeyOptions::Default, b"\x0e\x33", false);
        check_key::<CollatorUtf8Mb4UnicodeCi>(b"a ", KeyOptions::NoPad, b"\x0e\x33\x02\x09", false);
        for option in [KeyOptions::Default, KeyOptions::NoPad] {
            check_key::<CollatorUtf8Mb40900AiCi>(b"a ", option, b"\x1c\x47\x02\x09", false);
        }
        assert!(CollatorBinary::CAN_USE_RAW_MEM_AS_KEY);
        assert!(CollatorUtf8Mb4BinNoPadding::CAN_USE_RAW_MEM_AS_KEY);
        assert!(!CollatorUtf8Mb4Bin::CAN_USE_RAW_MEM_AS_KEY);
        assert!(!CollatorLatin1Bin::CAN_USE_RAW_MEM_AS_KEY);
    }

    #[test]
    fn test_shared_max_key_len_go_rune_count() {
        for (input, runes) in [
            (b"a ".as_slice(), 2),
            ("中😀".as_bytes(), 2),
            (b"\xff", 1),
            (b"\xc3\x28", 2),
        ] {
            assert_eq!(CollatorBinary::max_sort_key_len(input), input.len());
            assert_eq!(CollatorUtf8Mb4Bin::max_sort_key_len(input), input.len());
            assert_eq!(CollatorLatin1Bin::max_sort_key_len(input), input.len());
            assert_eq!(CollatorUtf8Mb4GeneralCi::max_sort_key_len(input), runes * 2);
            assert_eq!(
                CollatorUtf8Mb4UnicodeCi::max_sort_key_len(input),
                runes * 16
            );
            assert_eq!(CollatorUtf8Mb40900AiCi::max_sort_key_len(input), runes * 16);
            assert_eq!(CollatorGbkBin::max_sort_key_len(input), runes * 2);
            assert_eq!(CollatorGb18030Bin::max_sort_key_len(input), runes * 4);
        }
    }

    #[test]
    fn test_shared_raw_kernels_do_not_validate_utf8() {
        assert!(SortKey::<_, CollatorUtf8Mb4GeneralCi>::new(b"\xff").is_err());
        assert_eq!(
            CollatorUtf8Mb4GeneralCi::sort_compare(b"\xff", b"x", false).unwrap(),
            Ordering::Equal
        );
        check_key::<CollatorUtf8Mb4GeneralCi>(b"\xff", KeyOptions::Default, b"", false);
        check_key::<CollatorUtf8Mb4UnicodeCi>(b"a\xffz", KeyOptions::Default, b"\x0e\x33", false);
        check_key::<CollatorUtf8Mb4Bin>(b"\xff ", KeyOptions::Default, b"\xff", true);
    }

    #[derive(Default)]
    struct RecordingHasher(Vec<String>);

    impl Hasher for RecordingHasher {
        fn finish(&self) -> u64 {
            0
        }
        fn write(&mut self, bytes: &[u8]) {
            self.0.push(format!("bytes:{bytes:?}"));
        }
        fn write_usize(&mut self, value: usize) {
            self.0.push(format!("usize:{value}"));
        }
        fn write_u16(&mut self, value: u16) {
            self.0.push(format!("u16:{value}"));
        }
        fn write_u128(&mut self, value: u128) {
            self.0.push(format!("u128:{value}"));
        }
    }

    #[test]
    fn test_shared_hash_protocol_is_not_key_hash() {
        let mut weights = RecordingHasher::default();
        CollatorUtf8Mb4GeneralCi::sort_hash(&mut weights, b"a ").unwrap();
        assert_eq!(weights.0, ["u16:65"]);
        let mut key_hash = RecordingHasher::default();
        CollatorUtf8Mb4GeneralCi::sort_key(b"a ")
            .unwrap()
            .hash(&mut key_hash);
        assert_eq!(key_hash.0, ["usize:2", "bytes:[0, 65]"]);
        let mut uca = RecordingHasher::default();
        CollatorUtf8Mb4UnicodeCi::sort_hash(&mut uca, b"a ").unwrap();
        assert_eq!(uca.0, ["u128:3635"]);
        let mut binary = RecordingHasher::default();
        CollatorBinary::sort_hash(&mut binary, b"a ").unwrap();
        assert_eq!(binary.0, ["usize:2", "bytes:[97, 32]"]);
    }

    #[test]
    fn test_shared_signed_ids_keep_padding_and_like_modes() {
        for (id, expected) in [
            (46, Ordering::Less),
            (-46, Ordering::Equal),
            (63, Ordering::Less),
            (-63, Ordering::Less),
            (-45, Ordering::Equal),
            (-192, Ordering::Equal),
            (-224, Ordering::Equal),
            (-255, Ordering::Less),
            (-309, Ordering::Less),
            (-47, Ordering::Equal),
            (-65, Ordering::Equal),
            (-83, Ordering::Equal),
        ] {
            let actual = match_template_collator! {
                TT, match Collation::from_i32(id).unwrap() {
                    Collation::TT => TT::sort_compare(b"a", b"a ", false).unwrap(),
                }
            };
            assert_eq!(actual, expected, "signed ID {id}");
        }
        assert!(Collation::from_i32(i32::MIN).is_err());
        assert_eq!(
            CollatorGb18030Bin::LIKE_PATTERN_MODE,
            LikePatternMode::Bytes
        );
        assert_eq!(
            CollatorGbkBin::LIKE_PATTERN_MODE,
            LikePatternMode::BinaryRunes
        );
        assert_eq!(
            CollatorLatin1Bin::LIKE_PATTERN_MODE,
            LikePatternMode::BinaryRunes
        );
    }

    #[test]
    #[allow(clippy::string_lit_as_bytes)]
    fn test_compare() {
        use std::{cmp::Ordering, collections::hash_map::DefaultHasher};

        let collations = [
            (Collation::Utf8Mb4Bin, 0),
            (Collation::Utf8Mb4BinNoPadding, 1),
            (Collation::Utf8Mb4GeneralCi, 2),
            (Collation::Utf8Mb4UnicodeCi, 3),
            (Collation::Latin1Bin, 4),
            (Collation::GbkBin, 5),
            (Collation::GbkChineseCi, 6),
            (Collation::Utf8Mb40900AiCi, 7),
            (Collation::Utf8Mb40900Bin, 8),
            (Collation::Gb18030Bin, 9),
            (Collation::Gb18030ChineseCi, 10),
        ];
        let cases = vec![
            // (sa, sb, [Utf8Mb4Bin, Utf8Mb4BinNoPadding, Utf8Mb4GeneralCi, Utf8Mb4UnicodeCi,
            // Latin1, GBKBin, GbkChineseCi])
            (
                "a".as_bytes(),
                "a".as_bytes(),
                [
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                ],
            ),
            (
                "a".as_bytes(),
                "a ".as_bytes(),
                [
                    Ordering::Equal,
                    Ordering::Less,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Equal,
                    Ordering::Equal,
                ],
            ),
            (
                "a".as_bytes(),
                "A ".as_bytes(),
                [
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Equal,
                    Ordering::Less,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Equal,
                ],
            ),
            (
                "aa ".as_bytes(),
                "a a".as_bytes(),
                [
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                ],
            ),
            (
                "A".as_bytes(),
                "a\t".as_bytes(),
                [
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                ],
            ),
            (
                "cAfe".as_bytes(),
                "café".as_bytes(),
                [
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Equal,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                ],
            ),
            (
                "cAfe ".as_bytes(),
                "café".as_bytes(),
                [
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Greater,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                ],
            ),
            (
                "ß".as_bytes(),
                "ss".as_bytes(),
                [
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Less,
                    Ordering::Equal,
                    Ordering::Greater,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Equal,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                ],
            ),
            (
                "中文".as_bytes(),
                "汉字".as_bytes(),
                [
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Greater,
                    Ordering::Greater,
                ],
            ),
            (
                "啊".as_bytes(),
                "把".as_bytes(),
                [
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                ],
            ),
            (
                &[0x3e, 0xfe, 0x3e, 0x3e],
                &[0x3e, 0xff],
                [
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Less,
                    Ordering::Greater,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Less,
                    Ordering::Greater,
                    Ordering::Equal,
                ],
            ),
            (
                "ʩ".as_bytes(),
                "F".as_bytes(),
                [
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Less, // `ʩ` is invalid character in GBK.
                    Ordering::Less, // `ʩ` is invalid character in GBK.
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                ],
            ),
        ];

        for (sa, sb, expected) in cases {
            for (collation, order_in_expected) in &collations {
                let (cmp, ha, hb) = match_template_collator! {
                    TT, match collation {
                        Collation::TT => {
                            let eval_hash = |s| {
                                let mut hasher = DefaultHasher::default();
                                TT::sort_hash(&mut hasher, s).unwrap();
                                hasher.finish()
                            };

                            let cmp = TT::sort_compare(sa, sb, false).unwrap();
                            let ha = eval_hash(sa);
                            let hb = eval_hash(sb);
                            (cmp, ha, hb)
                        }
                    }
                };

                assert_eq!(
                    cmp, expected[*order_in_expected],
                    "when comparing {:?} and {:?} by {:?}",
                    sa, sb, collation
                );

                if expected[*order_in_expected] == Ordering::Equal {
                    assert_eq!(
                        ha, hb,
                        "when comparing the hash of {:?} and {:?} by {:?}, which should be equal",
                        sa, sb, collation
                    );
                } else {
                    assert_ne!(
                        ha, hb,
                        "when comparing the hash of {:?} and {:?} by {:?}, which should not be equal",
                        sa, sb, collation
                    );
                }
            }
        }
    }

    #[test]
    fn test_utf8mb4_sort_key() {
        let collations = [
            (Collation::Utf8Mb4Bin, 0),
            (Collation::Utf8Mb4BinNoPadding, 1),
            (Collation::Utf8Mb4GeneralCi, 2),
            (Collation::Utf8Mb4UnicodeCi, 3),
            (Collation::Latin1Bin, 4),
            (Collation::GbkBin, 5),
            (Collation::GbkChineseCi, 6),
            (Collation::Utf8Mb40900AiCi, 7),
            (Collation::Utf8Mb40900Bin, 8),
            (Collation::Gb18030Bin, 9),
            (Collation::Gb18030ChineseCi, 10),
        ];
        let cases = vec![
            // (str, [Utf8Mb4Bin, Utf8Mb4BinNoPadding, Utf8Mb4GeneralCi, Utf8Mb4UnicodeCi, Latin1,
            // GBKBin, GbkChineseCi])
            (
                "a",
                [
                    vec![0x61],
                    vec![0x61],
                    vec![0x00, 0x41],
                    vec![0x0E, 0x33],
                    vec![0x61],
                    vec![0x61],
                    vec![0x41],
                    vec![0x1C, 0x47],
                    vec![0x61],
                    vec![0x61],
                    vec![0x41],
                ],
            ),
            (
                "A ",
                [
                    vec![0x41],
                    vec![0x41, 0x20],
                    vec![0x00, 0x41],
                    vec![0x0E, 0x33],
                    vec![0x41],
                    vec![0x41],
                    vec![0x41],
                    vec![0x1C, 0x47, 0x2, 0x9],
                    vec![0x41, 0x20],
                    vec![0x41],
                    vec![0x41],
                ],
            ),
            (
                "A",
                [
                    vec![0x41],
                    vec![0x41],
                    vec![0x00, 0x41],
                    vec![0x0E, 0x33],
                    vec![0x41],
                    vec![0x41],
                    vec![0x41],
                    vec![0x1C, 0x47],
                    vec![0x41],
                    vec![0x41],
                    vec![0x41],
                ],
            ),
            (
                "😃",
                [
                    vec![0xF0, 0x9F, 0x98, 0x83],
                    vec![0xF0, 0x9F, 0x98, 0x83],
                    vec![0xff, 0xfd],
                    vec![0xff, 0xfd],
                    vec![0xF0, 0x9F, 0x98, 0x83],
                    vec![0x3F],
                    vec![0x3F],
                    vec![0x15, 0xFE],
                    vec![0xF0, 0x9F, 0x98, 0x83],
                    vec![0x94, 0x39, 0xFC, 0x39],
                    vec![0xFF, 0x03, 0xD8, 0x4B],
                ],
            ),
            (
                "Foo © bar 𝌆 baz ☃ qux",
                [
                    vec![
                        0x46, 0x6F, 0x6F, 0x20, 0xC2, 0xA9, 0x20, 0x62, 0x61, 0x72, 0x20, 0xF0,
                        0x9D, 0x8C, 0x86, 0x20, 0x62, 0x61, 0x7A, 0x20, 0xE2, 0x98, 0x83, 0x20,
                        0x71, 0x75, 0x78,
                    ],
                    vec![
                        0x46, 0x6F, 0x6F, 0x20, 0xC2, 0xA9, 0x20, 0x62, 0x61, 0x72, 0x20, 0xF0,
                        0x9D, 0x8C, 0x86, 0x20, 0x62, 0x61, 0x7A, 0x20, 0xE2, 0x98, 0x83, 0x20,
                        0x71, 0x75, 0x78,
                    ],
                    vec![
                        0x00, 0x46, 0x00, 0x4f, 0x00, 0x4f, 0x00, 0x20, 0x00, 0xa9, 0x00, 0x20,
                        0x00, 0x42, 0x00, 0x41, 0x00, 0x52, 0x00, 0x20, 0xff, 0xfd, 0x00, 0x20,
                        0x00, 0x42, 0x00, 0x41, 0x00, 0x5a, 0x00, 0x20, 0x26, 0x3, 0x00, 0x20,
                        0x00, 0x51, 0x00, 0x55, 0x00, 0x58,
                    ],
                    vec![
                        0x0E, 0xB9, 0x0F, 0x82, 0x0F, 0x82, 0x02, 0x09, 0x02, 0xC5, 0x02, 0x09,
                        0x0E, 0x4A, 0x0E, 0x33, 0x0F, 0xC0, 0x02, 0x09, 0xFF, 0xFD, 0x02, 0x09,
                        0x0E, 0x4A, 0x0E, 0x33, 0x10, 0x6A, 0x02, 0x09, 0x06, 0xFF, 0x02, 0x09,
                        0x0F, 0xB4, 0x10, 0x1F, 0x10, 0x5A,
                    ],
                    vec![
                        0x46, 0x6F, 0x6F, 0x20, 0xC2, 0xA9, 0x20, 0x62, 0x61, 0x72, 0x20, 0xF0,
                        0x9D, 0x8C, 0x86, 0x20, 0x62, 0x61, 0x7A, 0x20, 0xE2, 0x98, 0x83, 0x20,
                        0x71, 0x75, 0x78,
                    ],
                    vec![
                        0x46, 0x6f, 0x6f, 0x20, 0x3f, 0x20, 0x62, 0x61, 0x72, 0x20, 0x3f, 0x20,
                        0x62, 0x61, 0x7a, 0x20, 0x3f, 0x20, 0x71, 0x75, 0x78,
                    ],
                    vec![
                        0x46, 0x4f, 0x4f, 0x20, 0x3f, 0x20, 0x42, 0x41, 0x52, 0x20, 0x3f, 0x20,
                        0x42, 0x41, 0x5a, 0x20, 0x3f, 0x20, 0x51, 0x55, 0x58,
                    ],
                    vec![
                        0x1C, 0xE5, 0x1D, 0xDD, 0x1D, 0xDD, 0x2, 0x9, 0x5, 0x84, 0x2, 0x9, 0x1C,
                        0x60, 0x1C, 0x47, 0x1E, 0x33, 0x2, 0x9, 0xE, 0xF0, 0x2, 0x9, 0x1C, 0x60,
                        0x1C, 0x47, 0x1F, 0x21, 0x2, 0x9, 0x9, 0x1B, 0x2, 0x9, 0x1E, 0x21, 0x1E,
                        0xB5, 0x1E, 0xFF,
                    ],
                    vec![
                        0x46, 0x6F, 0x6F, 0x20, 0xC2, 0xA9, 0x20, 0x62, 0x61, 0x72, 0x20, 0xF0,
                        0x9D, 0x8C, 0x86, 0x20, 0x62, 0x61, 0x7A, 0x20, 0xE2, 0x98, 0x83, 0x20,
                        0x71, 0x75, 0x78,
                    ],
                    vec![
                        0x46, 0x6F, 0x6F, 0x20, 0x81, 0x30, 0x84, 0x38, 0x20, 0x62, 0x61, 0x72,
                        0x20, 0x94, 0x32, 0xEF, 0x32, 0x20, 0x62, 0x61, 0x7A, 0x20, 0x81, 0x37,
                        0xA3, 0x30, 0x20, 0x71, 0x75, 0x78,
                    ],
                    vec![
                        0x46, 0x4F, 0x4F, 0x20, 0xFF, 0x00, 0x00, 0x26, 0x20, 0x42, 0x41, 0x52,
                        0x20, 0xFF, 0x03, 0xB5, 0x4E, 0x20, 0x42, 0x41, 0x5A, 0x20, 0xFF, 0x00,
                        0x23, 0xC8, 0x20, 0x51, 0x55, 0x58,
                    ],
                ],
            ),
            (
                "ﷻ",
                [
                    vec![0xEF, 0xB7, 0xBB],
                    vec![0xEF, 0xB7, 0xBB],
                    vec![0xFD, 0xFB],
                    vec![
                        0x13, 0x5E, 0x13, 0xAB, 0x02, 0x09, 0x13, 0x5E, 0x13, 0xAB, 0x13, 0x50,
                        0x13, 0xAB, 0x13, 0xB7,
                    ],
                    vec![0xEF, 0xB7, 0xBB],
                    vec![0x3f],
                    vec![0x3f],
                    vec![
                        0x23, 0x25, 0x23, 0x9C, 0x2, 0x9, 0x23, 0x25, 0x23, 0x9C, 0x23, 0xB, 0x23,
                        0x9C, 0x23, 0xB1,
                    ],
                    vec![0xEF, 0xB7, 0xBB],
                    vec![0x84, 0x30, 0xFE, 0x35],
                    vec![0xFF, 0x00, 0x98, 0x8F],
                ],
            ),
            (
                "中文",
                [
                    vec![0xE4, 0xB8, 0xAD, 0xE6, 0x96, 0x87],
                    vec![0xE4, 0xB8, 0xAD, 0xE6, 0x96, 0x87],
                    vec![0x4E, 0x2D, 0x65, 0x87],
                    vec![0xFB, 0x40, 0xCE, 0x2D, 0xFB, 0x40, 0xE5, 0x87],
                    vec![0xE4, 0xB8, 0xAD, 0xE6, 0x96, 0x87],
                    vec![0xD6, 0xD0, 0xCE, 0xC4],
                    vec![0xD3, 0x21, 0xC1, 0xAD],
                    vec![0xFB, 0x40, 0xCE, 0x2D, 0xFB, 0x40, 0xE5, 0x87],
                    vec![0xE4, 0xB8, 0xAD, 0xE6, 0x96, 0x87],
                    vec![0xD6, 0xD0, 0xCE, 0xC4],
                    vec![0xFF, 0xA0, 0x9B, 0xC1, 0xFF, 0xA0, 0x78, 0xBD],
                ],
            ),
        ];
        for (s, expected) in cases {
            for (collation, order_in_expected) in &collations {
                let code = match_template_collator! {
                    TT, match collation {
                        Collation::TT => TT::sort_key(s.as_bytes()).unwrap()
                    }
                };
                assert_eq!(
                    code, expected[*order_in_expected],
                    "when testing {} by {:?}",
                    s, collation
                );
            }
        }
    }

    #[test]
    fn test_latin1_bin() {
        use std::{cmp::Ordering, collections::hash_map::DefaultHasher, hash::Hasher};

        use crate::codec::collation::collator::CollatorLatin1Bin;

        let cases = vec![
            (
                vec![0xFF, 0x88, 0x00, 0x13],
                vec![0xFF, 0x88, 0x00, 0x13],
                Ordering::Equal,
            ),
            (
                vec![0xFF, 0x88, 0x00, 0x13, 0x20, 0x20, 0x20],
                vec![0xFF, 0x88, 0x00, 0x13],
                Ordering::Equal,
            ),
            (
                vec![0xFF, 0x88, 0x00, 0x13, 0x09, 0x09, 0x09],
                vec![0xFF, 0x88, 0x00, 0x13],
                Ordering::Greater,
            ),
        ];

        for (sa, sb, od) in cases {
            let eval_hash = |s| {
                let mut hasher = DefaultHasher::default();
                CollatorLatin1Bin::sort_hash(&mut hasher, s).unwrap();
                hasher.finish()
            };

            let cmp = CollatorLatin1Bin::sort_compare(sa.as_slice(), sb.as_slice(), false).unwrap();
            let ha = eval_hash(sa.as_slice());
            let hb = eval_hash(sb.as_slice());

            assert_eq!(cmp, od, "when comparing {:?} and {:?}", sa, sb);

            if od == Ordering::Equal {
                assert_eq!(
                    ha, hb,
                    "when comparing the hash of {:?} and {:?}, which should be equal",
                    sa, sb
                );
            } else {
                assert_ne!(
                    ha, hb,
                    "when comparing the hash of {:?} and {:?}, which should not be equal",
                    sa, sb
                );
            }
        }
    }
}
