// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Shared LIKE tokenization and matching. Raw and compiled patterns use the
//! same backtracking loop; neither path materializes the target's characters.

use std::convert::Infallible;

use super::{
    Charset, Collator, LikePatternMode,
    charset::{CharsetBinary, CharsetUtf8mb4},
    collator::CollatorUtf8Mb4BinNoPadding,
};
use crate::codec::Result;

/// The public, normalized wildcard token kinds used by TiDB's string utilities.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatternType {
    /// One literal character or byte.
    Match,
    /// `_`, consuming exactly one decoded unit.
    One,
    /// `%`, consuming zero or more decoded units.
    Any,
}

/// The JSON search helper and normal SQL LIKE differ at a trailing escape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrailingEscape {
    /// Treat the final escape character as a literal (normal LIKE).
    Literal,
    /// A final escape without a following character cannot match (JSON search).
    Reject,
}

/// Pattern syntax, independent of collation and character decoding.
#[derive(Clone, Copy, Debug)]
pub struct MatchOptions {
    /// Keep the caller's integer width; TiKV LIKE casts its i64 escape to u32.
    pub escape: u32,
    /// How to interpret an escape at the end of the pattern.
    pub trailing_escape: TrailingEscape,
}

type Decoder = fn(&[u8]) -> Option<(u32, usize)>;
type LiteralEqual = for<'a, 'b> fn(Unit<'a>, Unit<'b>) -> Result<bool>;

#[derive(Clone, Copy, Debug)]
struct Unit<'a> {
    code: u32,
    bytes: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
enum Token<T> {
    Literal(T),
    One,
    Any,
    Reject,
}

fn decode<CS: Charset>(bytes: &[u8]) -> Option<(u32, usize)> {
    CS::decode_one(bytes).map(|(ch, width)| (ch.into(), width))
}

// This is the only escape tokenizer. Compiling stores its output rather than
// reinterpreting syntax; importantly, escape wins even when it is '%' or '_'.
fn next_raw_token<'a>(
    pattern: &'a [u8],
    pos: usize,
    decoder: Decoder,
    options: MatchOptions,
) -> Option<(Token<Unit<'a>>, usize)> {
    let (mut code, width) = decoder(&pattern[pos..])?;
    let mut start = pos;
    let mut end = pos + width;
    let escaped = code == options.escape;
    if escaped {
        if end < pattern.len() {
            start = end;
            let (next, width) = decoder(&pattern[start..])?;
            code = next;
            end += width;
        } else if options.trailing_escape == TrailingEscape::Reject {
            return Some((Token::Reject, end));
        }
    }
    let token = match code {
        0x5f if !escaped => Token::One,
        0x25 if !escaped => Token::Any,
        _ => Token::Literal(Unit {
            code,
            bytes: &pattern[start..end],
        }),
    };
    Some((token, end))
}

// One loop serves raw byte offsets, stored token indices, and the public
// stringutil tuple representation. Backtracking always uses the target decoder.
fn match_tokens<T: Copy, E>(
    target: &[u8],
    pattern_len: usize,
    next_token: impl Fn(usize) -> Option<(Token<T>, usize)>,
    decoder: Decoder,
    equal: impl Fn(Unit<'_>, T) -> std::result::Result<bool, E>,
) -> std::result::Result<bool, E> {
    let (mut px, mut tx) = (0, 0);
    let mut backtrack = None;
    while px < pattern_len || tx < target.len() {
        if let Some((token, next_px)) = next_token(px) {
            match token {
                Token::Any => {
                    px = next_px;
                    if px == pattern_len {
                        return Ok(true);
                    }
                    backtrack = Some((px, tx));
                    continue;
                }
                Token::One | Token::Literal(_) => {
                    if let Some((code, width)) = decoder(&target[tx..]) {
                        let matched = match token {
                            Token::One => true,
                            Token::Literal(literal) => equal(
                                Unit {
                                    code,
                                    bytes: &target[tx..tx + width],
                                },
                                literal,
                            )?,
                            _ => unreachable!(),
                        };
                        if matched {
                            px = next_px;
                            tx += width;
                            continue;
                        }
                    }
                }
                Token::Reject => {}
            }
        }
        if let Some((next_px, next_tx)) = backtrack {
            if next_tx < target.len() {
                let width = decoder(&target[next_tx..]).map_or(1, |(_, width)| width);
                tx = next_tx + width;
                px = next_px;
                backtrack = Some((next_px, tx));
                continue;
            }
        }
        return Ok(false);
    }
    Ok(true)
}

fn char_bytes_for_compare<C: Collator>(unit: Unit<'_>) -> &[u8] {
    if <C::Charset as Charset>::charset() == crate::Charset::Utf8Mb4
        && unit.code == char::REPLACEMENT_CHARACTER as u32
        && unit.bytes.len() == 1
    {
        b"\xef\xbf\xbd"
    } else {
        unit.bytes
    }
}

fn literal_equal<C: Collator>(left: Unit<'_>, right: Unit<'_>) -> Result<bool> {
    if C::LIKE_PATTERN_MODE == LikePatternMode::Bytes {
        Ok(left.bytes == right.bytes)
    } else {
        C::like_pattern_compare(
            char_bytes_for_compare::<C>(left),
            char_bytes_for_compare::<C>(right),
        )
    }
}

/// Match directly without allocating a compiled pattern or a decoded target.
/// The caller selects C/CS using its original signed-ID/argument-charset rules.
pub fn matches_raw<C: Collator, CS: Charset>(
    target: &[u8],
    pattern: &[u8],
    options: MatchOptions,
) -> Result<bool> {
    match_tokens(
        target,
        pattern.len(),
        |pos| next_raw_token(pattern, pos, decode::<CS>, options),
        decode::<CS>,
        literal_equal::<C>,
    )
}

#[derive(Clone, Copy, Debug)]
struct StoredLiteral {
    code: u32,
    start: usize,
    end: usize,
}

fn compile_tokens(
    pattern: &[u8],
    decoder: Decoder,
    options: MatchOptions,
) -> Vec<Token<StoredLiteral>> {
    let mut tokens = Vec::new();
    let mut pos = 0;
    while let Some((token, end)) = next_raw_token(pattern, pos, decoder, options) {
        let token = match token {
            Token::Literal(unit) => Token::Literal(StoredLiteral {
                code: unit.code,
                start: end - unit.bytes.len(),
                end,
            }),
            Token::Any if matches!(tokens.last(), Some(Token::Any)) => {
                pos = end;
                continue;
            }
            Token::One if matches!(tokens.last(), Some(Token::Any)) => {
                *tokens.last_mut().unwrap() = Token::One;
                Token::Any
            }
            Token::One => Token::One,
            Token::Any => Token::Any,
            Token::Reject => Token::Reject,
        };
        tokens.push(token);
        pos = end;
    }
    if pos < pattern.len() {
        // Do not turn a prefix rejected by a custom Charset into a shorter,
        // successfully compiled pattern.
        tokens.push(Token::Reject);
    }
    tokens
}

/// Immutable compiled LIKE pattern. Literal bytes are retained alongside their
/// decoded codepoints, including for mixed binary-charset/Unicode collators.
#[derive(Clone, Debug)]
pub struct CompiledPattern {
    pattern: Vec<u8>,
    tokens: Vec<Token<StoredLiteral>>,
    decoder: Decoder,
    equal: LiteralEqual,
}

/// Compile with the same decoder/literal policy as matches_raw.
pub fn compile<C: Collator, CS: Charset>(pattern: &[u8], options: MatchOptions) -> CompiledPattern {
    CompiledPattern {
        pattern: pattern.to_vec(),
        tokens: compile_tokens(pattern, decode::<CS>, options),
        decoder: decode::<CS>,
        equal: literal_equal::<C>,
    }
}

impl CompiledPattern {
    /// Match an arbitrary byte string without allocating a decoded target.
    pub fn is_match(&self, target: &[u8]) -> Result<bool> {
        match_tokens(
            target,
            self.tokens.len(),
            |pos| self.tokens.get(pos).copied().map(|token| (token, pos + 1)),
            self.decoder,
            |left, right| {
                (self.equal)(
                    left,
                    Unit {
                        code: right.code,
                        bytes: &self.pattern[right.start..right.end],
                    },
                )
            },
        )
    }
}

fn compile_units<T: From<u8>>(
    pattern: &[u8],
    escape: u8,
    decoder: Decoder,
    unit: impl Fn(u32) -> T,
) -> (Vec<T>, Vec<PatternType>) {
    compile_tokens(
        pattern,
        decoder,
        MatchOptions {
            escape: u32::from(escape),
            trailing_escape: TrailingEscape::Literal,
        },
    )
    .into_iter()
    .map(|token| match token {
        Token::Literal(literal) => (unit(literal.code), PatternType::Match),
        Token::One => (T::from(b'_'), PatternType::One),
        Token::Any => (T::from(b'%'), PatternType::Any),
        Token::Reject => unreachable!("Literal trailing escape never rejects"),
    })
    .unzip()
}

/// Compile Go-rune pattern tuples, preserving stringutil's normalization.
pub fn compile_runes(pattern: &[u8], escape: u8) -> (Vec<char>, Vec<PatternType>) {
    compile_units(pattern, escape, decode::<CharsetUtf8mb4>, |code| {
        char::from_u32(code).unwrap()
    })
}

/// Compile byte pattern tuples, preserving stringutil's normalization.
pub fn compile_bytes(pattern: &[u8], escape: u8) -> (Vec<u8>, Vec<PatternType>) {
    compile_units(pattern, escape, decode::<CharsetBinary>, |code| code as u8)
}

fn tuple_token<T: Copy>(
    units: &[T],
    types: &[PatternType],
    pos: usize,
) -> Option<(Token<T>, usize)> {
    let literal = *units.get(pos)?;
    let token = match types[pos] {
        PatternType::Match => Token::Literal(literal),
        PatternType::One => Token::One,
        PatternType::Any => Token::Any,
    };
    Some((token, pos + 1))
}

/// Match normalized rune tuples with a caller-provided character equivalence.
pub fn matches_compiled_runes_with(
    target: &[u8],
    units: &[char],
    types: &[PatternType],
    equal: impl Fn(char, char) -> bool,
) -> bool {
    debug_assert_eq!(units.len(), types.len());
    match_tokens(
        target,
        units.len(),
        |pos| tuple_token(units, types, pos),
        decode::<CharsetUtf8mb4>,
        |left, right| Ok::<_, Infallible>(equal(char::from_u32(left.code).unwrap(), right)),
    )
    .unwrap()
}

/// Match normalized byte tuples without Unicode decoding.
pub fn matches_compiled_bytes(target: &[u8], units: &[u8], types: &[PatternType]) -> bool {
    debug_assert_eq!(units.len(), types.len());
    match_tokens(
        target,
        units.len(),
        |pos| tuple_token(units, types, pos),
        decode::<CharsetBinary>,
        |left, right| Ok::<_, Infallible>(left.code == u32::from(right)),
    )
    .unwrap()
}

/// Exact Go-rune matching, including JSON's explicit trailing-escape policy.
pub fn matches_runes(target: &[u8], pattern: &[u8], options: MatchOptions) -> bool {
    matches_raw::<CollatorUtf8Mb4BinNoPadding, CharsetUtf8mb4>(target, pattern, options)
        .expect("binary-rune literal comparison cannot fail")
}

#[cfg(test)]
mod tests {
    use super::{super::collator::*, *};

    fn options(escape: u32, trailing_escape: TrailingEscape) -> MatchOptions {
        MatchOptions {
            escape,
            trailing_escape,
        }
    }

    #[test]
    fn test_shared_pattern_trailing_escape_policy() {
        for (text, pattern, escape, literal, reject) in [
            ("\\", "\\", '\\', true, false),
            ("a%b", "a\\%b", '\\', true, true),
            ("a", "a\\", '\\', false, false),
            ("é", "é", 'é', true, false),
            ("", "%", '%', false, false),
        ] {
            for (policy, expected) in [
                (TrailingEscape::Literal, literal),
                (TrailingEscape::Reject, reject),
            ] {
                let opts = options(escape as u32, policy);
                assert_eq!(
                    matches_runes(text.as_bytes(), pattern.as_bytes(), opts),
                    expected
                );
                assert_eq!(
                    compile::<CollatorUtf8Mb4BinNoPadding, CharsetUtf8mb4>(
                        pattern.as_bytes(),
                        opts
                    )
                    .is_match(text.as_bytes())
                    .unwrap(),
                    expected
                );
            }
        }
    }

    fn check<C: Collator, CS: Charset>(text: &[u8], pattern: &[u8], expected: bool) {
        let opts = options(b'\\' as u32, TrailingEscape::Literal);
        assert_eq!(matches_raw::<C, CS>(text, pattern, opts).unwrap(), expected);
        assert_eq!(
            compile::<C, CS>(pattern, opts).is_match(text).unwrap(),
            expected
        );
    }

    #[test]
    fn test_shared_pattern_character_equality() {
        check::<CollatorBinary, CharsetBinary>(&[0xe4], &[0xaa], false);
        check::<CollatorUtf8Mb4BinNoPadding, CharsetUtf8mb4>(&[0xe4], &[0xaa], true);
        check::<CollatorUtf8Mb4GeneralCi, CharsetUtf8mb4>("😀".as_bytes(), "😁".as_bytes(), true);
        check::<CollatorUtf8Mb4UnicodeCi, CharsetUtf8mb4>("😀".as_bytes(), "😁".as_bytes(), false);
        check::<CollatorUtf8Mb4UnicodeCi, CharsetUtf8mb4>("ß".as_bytes(), b"ss", false);
        check::<CollatorUtf8Mb4UnicodeCi, CharsetUtf8mb4>(
            "\u{321d}".as_bytes(),
            "\u{321d}".as_bytes(),
            true,
        );
        check::<CollatorUtf8Mb4UnicodeCi, CharsetUtf8mb4>(
            "\u{321d}".as_bytes(),
            "\u{321e}".as_bytes(),
            false,
        );
        check::<CollatorUtf8Mb4UnicodeCi, CharsetUtf8mb4>("\u{3000}".as_bytes(), b" ", true);
        check::<CollatorUtf8Mb4UnicodeCi, CharsetUtf8mb4>(b" ", b"\0", false);
        check::<CollatorBinary, CharsetBinary>("中X".as_bytes(), b"%__X", true);
        check::<CollatorUtf8Mb4BinNoPadding, CharsetUtf8mb4>("中X".as_bytes(), b"%__X", false);
    }

    #[test]
    fn test_shared_pattern_mixed_charset_preserves_literal_bytes() {
        // With binary arguments, General CI receives the original malformed
        // single-byte literal, NOT the UTF-8 encoding of U+00E4/U+00AA.
        check::<CollatorUtf8Mb4GeneralCi, CharsetBinary>(&[0xe4], &[0xaa], true);
    }

    #[test]
    fn test_shared_pattern_raw_compiled_equivalence() {
        let alphabet = [b'a', b'%', b'_', b'\\', 0, 0xff];
        let mut strings = vec![Vec::new()];
        for &a in &alphabet {
            strings.push(vec![a]);
            for &b in &alphabet {
                strings.push(vec![a, b]);
            }
        }
        for &escape in &alphabet {
            let opts = options(u32::from(escape), TrailingEscape::Literal);
            for pattern in &strings {
                let compiled = compile::<CollatorUtf8Mb4GeneralCi, CharsetUtf8mb4>(pattern, opts);
                let (units, types) = compile_runes(pattern, escape);
                for target in &strings {
                    assert_eq!(
                        matches_raw::<CollatorUtf8Mb4GeneralCi, CharsetUtf8mb4>(
                            target, pattern, opts
                        )
                        .unwrap(),
                        compiled.is_match(target).unwrap(),
                        "target={target:?}, pattern={pattern:?}, escape={escape}"
                    );
                    assert_eq!(
                        matches_runes(target, pattern, opts),
                        matches_compiled_runes_with(target, &units, &types, |a, b| a == b)
                    );
                }
            }
        }
        assert_eq!(
            compile_bytes(b"%%_", b'\\'),
            (b"_%".to_vec(), vec![PatternType::One, PatternType::Any])
        );
    }
}
