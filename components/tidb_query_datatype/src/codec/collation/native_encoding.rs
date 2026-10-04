// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native charset byte operations. Error/result presentation stays with
//! callers; wire charset implementations and native case-mapping tables are
//! unchanged.

use std::ops::{BitOr, BitOrAssign};

use super::{
    decode_utf8_rune_strict,
    gb::{self, GbEncoding},
};

/// Exact source encoding-base operation bits (contains tests any overlap).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransformOp(u16);

impl TransformOp {
    pub const FROM_UTF8: Self = Self(1 << 0);
    pub const TO_UTF8: Self = Self(1 << 1);
    pub const TRUNCATE_TRIM: Self = Self(1 << 2);
    pub const TRUNCATE_REPLACE: Self = Self(1 << 3);
    pub const COLLECT_FROM: Self = Self(1 << 4);
    pub const COLLECT_TO: Self = Self(1 << 5);
    pub const SKIP_ERROR: Self = Self(1 << 6);
    pub const REPLACE_NO_ERR: Self = Self(
        Self::FROM_UTF8.0 | Self::TRUNCATE_REPLACE.0 | Self::COLLECT_FROM.0 | Self::SKIP_ERROR.0,
    );
    pub const REPLACE: Self =
        Self(Self::FROM_UTF8.0 | Self::TRUNCATE_REPLACE.0 | Self::COLLECT_FROM.0);
    pub const ENCODE: Self = Self(Self::FROM_UTF8.0 | Self::TRUNCATE_TRIM.0 | Self::COLLECT_TO.0);
    pub const ENCODE_NO_ERR: Self = Self(Self::ENCODE.0 | Self::SKIP_ERROR.0);
    pub const ENCODE_REPLACE: Self =
        Self(Self::FROM_UTF8.0 | Self::TRUNCATE_REPLACE.0 | Self::COLLECT_TO.0);
    pub const DECODE: Self = Self(Self::TO_UTF8.0 | Self::TRUNCATE_TRIM.0 | Self::COLLECT_TO.0);
    pub const DECODE_NO_ERR: Self = Self(Self::DECODE.0 | Self::SKIP_ERROR.0);
    pub const DECODE_REPLACE: Self =
        Self(Self::TO_UTF8.0 | Self::TRUNCATE_REPLACE.0 | Self::COLLECT_TO.0);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

impl BitOr for TransformOp {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for TransformOp {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// The single source policy: retain the first error, trim before replacement,
/// then collect source before converted bytes. Callers stop when push is false.
pub struct TransformPolicy<E, F: Fn(&[u8]) -> E> {
    op: TransformOp,
    bytes: Vec<u8>,
    first_error: Option<E>,
    make_error: F,
}

impl<E, F: Fn(&[u8]) -> E> TransformPolicy<E, F> {
    pub fn new(capacity: usize, op: TransformOp, make_error: F) -> Self {
        Self {
            op,
            bytes: Vec::with_capacity(capacity),
            first_error: None,
            make_error,
        }
    }

    pub fn push(&mut self, from: &[u8], to: &[u8], valid: bool) -> bool {
        if !valid {
            if self.first_error.is_none() && !self.op.contains(TransformOp::SKIP_ERROR) {
                self.first_error = Some((self.make_error)(from));
            }
            if self.op.contains(TransformOp::TRUNCATE_TRIM) {
                return false;
            }
            if self.op.contains(TransformOp::TRUNCATE_REPLACE) {
                self.bytes.push(b'?');
                return true;
            }
        }
        if self.op.contains(TransformOp::COLLECT_FROM) {
            self.bytes.extend_from_slice(from);
        } else if self.op.contains(TransformOp::COLLECT_TO) {
            self.bytes.extend_from_slice(to);
        }
        true
    }

    pub fn finish(self) -> (Vec<u8>, Option<E>) {
        (self.bytes, self.first_error)
    }
}

/// Native byte-operation dispatch only; no case tables or wire policy aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedNativeEncoding {
    Utf8,
    Utf8Mb3Strict,
    Ascii,
    Latin1,
    Binary,
    Gbk,
    Gb18030,
}

impl SharedNativeEncoding {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Utf8 | Self::Utf8Mb3Strict => "utf8mb4",
            Self::Ascii => "ascii",
            Self::Latin1 => "latin1",
            Self::Binary => "binary",
            Self::Gbk => "gbk",
            Self::Gb18030 => "gb18030",
        }
    }

    pub fn peek(self, source: &[u8]) -> &[u8] {
        match self {
            Self::Utf8 | Self::Utf8Mb3Strict => utf8_peek(source),
            Self::Ascii | Self::Latin1 | Self::Binary => source.get(..1).unwrap_or(source),
            Self::Gbk => gb::peek_native(GbEncoding::Gbk, source),
            Self::Gb18030 => gb::peek_native(GbEncoding::Gb18030, source),
        }
    }

    pub fn mb_len(self, source: &[u8]) -> usize {
        match self {
            Self::Utf8 | Self::Utf8Mb3Strict => {
                let (width, valid) = decode_utf8_group(source);
                if valid && width > 1 { width } else { 0 }
            }
            Self::Gbk => gb::mb_len_native(GbEncoding::Gbk, source),
            Self::Gb18030 => gb::mb_len_native(GbEncoding::Gb18030, source),
            Self::Ascii | Self::Latin1 | Self::Binary => 0,
        }
    }

    /// Checks representability of UTF-8 input, not validity of encoded GB
    /// bytes.
    pub fn is_valid(self, source: &[u8]) -> bool {
        match self {
            Self::Ascii => source.iter().all(|byte| *byte <= 0x7f),
            Self::Latin1 | Self::Binary => true,
            _ => {
                let mut valid = true;
                self.foreach(source, TransformOp::FROM_UTF8, |_, _, ok| {
                    valid = ok;
                    ok
                });
                valid
            }
        }
    }

    pub fn foreach<F>(self, source: &[u8], operation: TransformOp, mut visit: F)
    where
        F: FnMut(&[u8], &[u8], bool) -> bool,
    {
        match self {
            Self::Utf8 | Self::Utf8Mb3Strict => {
                let strict_mb3 = self == Self::Utf8Mb3Strict;
                let mut offset = 0;
                while offset < source.len() {
                    let (width, valid) = decode_utf8_group(&source[offset..]);
                    let end = offset + width;
                    let ok = valid && (!strict_mb3 || width <= 3);
                    if !visit(&source[offset..end], &source[offset..end], ok) {
                        return;
                    }
                    offset = end;
                }
            }
            Self::Ascii => {
                let mut offset = 0;
                while offset < source.len() {
                    let mut width = 1;
                    let mut ok = true;
                    if source[offset] > 0x7f {
                        width = utf8_peek(&source[offset..]).len();
                        ok = false;
                    }
                    let group = &source[offset..offset + width];
                    if !visit(group, group, ok) {
                        return;
                    }
                    offset += width;
                }
            }
            Self::Latin1 | Self::Binary => {
                for byte in source {
                    let group = std::slice::from_ref(byte);
                    if !visit(group, group, true) {
                        break;
                    }
                }
            }
            Self::Gbk | Self::Gb18030 => gb::foreach_native(
                if self == Self::Gbk {
                    GbEncoding::Gbk
                } else {
                    GbEncoding::Gb18030
                },
                source,
                operation.contains(TransformOp::FROM_UTF8),
                visit,
            ),
        }
    }

    /// Source-byte length of the valid prefix, stopping at its first bad group.
    pub fn count_valid(self, source: &[u8], operation: TransformOp) -> usize {
        let mut count = 0;
        self.foreach(source, operation, |from, _, valid| {
            if valid {
                count += from.len();
            }
            valid
        });
        count
    }

    /// Generic Encoding.transform: unlike ASCII/UTF8 leaf transforms, valid
    /// input still follows the operation bits. Only Latin1/Binary bypass them.
    pub fn transform<E, F>(
        self,
        source: &[u8],
        operation: TransformOp,
        make_error: F,
    ) -> (Vec<u8>, Option<E>)
    where
        F: Fn(&[u8]) -> E,
    {
        match self {
            Self::Latin1 | Self::Binary => (source.to_vec(), None),
            _ => {
                let mut policy = TransformPolicy::new(source.len(), operation, make_error);
                self.foreach(source, operation, |from, to, valid| {
                    policy.push(from, to, valid)
                });
                policy.finish()
            }
        }
    }
}

/// Specialized ASCII transform's valid-input fast path is independent of flags.
pub fn native_ascii_transform<E, F>(
    source: &[u8],
    operation: TransformOp,
    make_error: F,
) -> (Vec<u8>, Option<E>)
where
    F: Fn(&[u8]) -> E,
{
    if SharedNativeEncoding::Ascii.is_valid(source) {
        return (source.to_vec(), None);
    }
    SharedNativeEncoding::Ascii.transform(source, operation, make_error)
}

/// Specialized UTF8/strict-mb3 transforms also return valid input unchanged.
pub fn native_utf8_transform<E, F>(
    source: &[u8],
    operation: TransformOp,
    strict_mb3: bool,
    make_error: F,
) -> (Vec<u8>, Option<E>)
where
    F: Fn(&[u8]) -> E,
{
    let encoding = if strict_mb3 {
        SharedNativeEncoding::Utf8Mb3Strict
    } else {
        SharedNativeEncoding::Utf8
    };
    if encoding.is_valid(source) {
        return (source.to_vec(), None);
    }
    encoding.transform(source, operation, make_error)
}

// Source lead-byte grouping, NOT a decoder. ASCII's invalid groups use it too.
fn utf8_peek(source: &[u8]) -> &[u8] {
    if source.is_empty() {
        return source;
    }
    let expected = if source[0] < 0x80 {
        1
    } else if source[0] < 0xe0 {
        2
    } else if source[0] < 0xf0 {
        3
    } else {
        4
    };
    &source[..expected.min(source.len())]
}

fn decode_utf8_group(source: &[u8]) -> (usize, bool) {
    if source.is_empty() {
        return (0, true);
    }
    // The existing strict scalar decoder enforces the identical lead ranges,
    // continuations, overlong/surrogate/max-codepoint exclusions. Invalid Go
    // decode groups advance one byte, unlike utf8_peek's lead-width grouping.
    decode_utf8_rune_strict(source).map_or((1, false), |(_, width)| (width, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_encoding_preserves_grouping_policy_and_distinct_leaf_fast_paths() {
        use SharedNativeEncoding::*;
        let error = |group: &[u8]| group.to_vec();
        assert_eq!(format!("{:?}", TransformOp::ENCODE), "TransformOp(37)");
        let mut flags = TransformOp::COLLECT_TO;
        flags |= TransformOp::COLLECT_FROM;
        let mut policy = TransformPolicy::new(0, flags, error);
        assert!(policy.push(b"from", b"to", true));
        assert!(policy.push(b"bad", b"BAD", false));
        assert!(policy.push(b"later", b"LATER", false));
        assert_eq!(
            policy.finish(),
            (b"frombadlater".to_vec(), Some(b"bad".to_vec()))
        );
        let mut trim = TransformPolicy::new(
            0,
            TransformOp::TRUNCATE_TRIM | TransformOp::TRUNCATE_REPLACE,
            error,
        );
        assert!(!trim.push(b"bad", b"to", false));
        assert_eq!(trim.finish(), (vec![], Some(b"bad".to_vec())));
        for encoding in [Ascii, Utf8, Utf8Mb3Strict] {
            assert_eq!(
                encoding.transform(b"abc", TransformOp::default(), error),
                (vec![], None)
            );
        }
        assert_eq!(
            native_ascii_transform(b"abc", TransformOp::default(), error),
            (b"abc".to_vec(), None)
        );
        for strict in [false, true] {
            assert_eq!(
                native_utf8_transform(b"abc", TransformOp::default(), strict, error),
                (b"abc".to_vec(), None)
            );
        }
        let malformed = b"a\xc3b\xffc";
        assert_eq!(
            Ascii.transform(malformed, TransformOp::REPLACE, error),
            (b"a??".to_vec(), Some(b"\xc3b".to_vec()))
        );
        assert_eq!(
            Utf8.transform(malformed, TransformOp::REPLACE, error),
            (b"a?b?c".to_vec(), Some(b"\xc3".to_vec()))
        );
        assert_eq!(Utf8.peek(b"\xffabc"), b"\xffabc");
        assert_eq!(Ascii.peek(b"\xffabc"), b"\xff");
        assert_eq!(Utf8.mb_len(b"\xc3"), 0);
        for invalid in [
            b"\xc0\x80".as_slice(),
            b"\xed\xa0\x80",
            b"\xf4\x90\x80\x80",
            b"\xe0\x80",
        ] {
            assert!(!Utf8.is_valid(invalid));
            assert_eq!(Utf8.mb_len(invalid), 0);
        }
        assert!(Utf8.is_valid("�".as_bytes()));
        assert_eq!(Utf8.count_valid(malformed, TransformOp::FROM_UTF8), 1);
        assert_eq!(Gbk.count_valid(b"\xd2\xbb", TransformOp::TO_UTF8), 2);
        assert_eq!(Utf8Mb3Strict.mb_len("😂".as_bytes()), 4);
        assert!(!Utf8Mb3Strict.is_valid("😂".as_bytes()));
        assert_eq!(
            native_utf8_transform("a😂z".as_bytes(), TransformOp::REPLACE_NO_ERR, true, error),
            (b"a?z".to_vec(), None)
        );
        assert_eq!(
            Utf8.transform(b"a\xffb", TransformOp::DECODE_NO_ERR, error),
            (b"a".to_vec(), None)
        );
        let mut count = 0;
        Utf8.foreach(b"a\xffb", TransformOp::default(), |_, _, _| {
            count += 1;
            false
        });
        assert_eq!(count, 1);
        for encoding in [Latin1, Binary] {
            assert_eq!(
                encoding.transform(b"\xff", TransformOp::default(), error),
                (vec![255], None)
            );
        }
        assert_eq!(
            Gbk.transform("一".as_bytes(), TransformOp::ENCODE, error),
            (b"\xd2\xbb".to_vec(), None)
        );
        assert_eq!(
            Gbk.transform(b"\xd2\xbb", TransformOp::DECODE, error),
            ("一".as_bytes().to_vec(), None)
        );
        assert_eq!(
            Gb18030.transform("€".as_bytes(), TransformOp::ENCODE, error),
            (b"\xa2\xe3".to_vec(), None)
        );
        assert!(std::panic::catch_unwind(|| Gb18030.mb_len(b"\x81\x30")).is_err());
    }
}
