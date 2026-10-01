// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Shared regexp algorithms, not an evaluator or a compilation/cache policy.
//! Callers retain their distinct coercion, NULL, validation and compile order,
//! compiler representation, statement caches and SQL diagnostic mapping.

use std::collections::HashSet;

use regex::{Captures, Match, Regex};

/// Actual policy failures; frontends map variants to their original
/// diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegexpPolicyError {
    InvalidMatchType(char),
    InvalidPosition { pos: i64, count: usize },
    InvalidSubstitution(usize),
    InvalidReplacementUtf8(std::str::Utf8Error),
}

/// Single-digit backslash capture references, shared by both replacement
/// caches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeReplacementPart {
    Group(usize),
    Literal(Vec<u8>),
}

/// Native strings validate each selected replacement; wire output permits
/// bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegexpReplacementEncoding {
    NativeUtf8,
    WireBytes,
}

/// Reduce flags with rightmost `i`/`c` precedence and the caller's initial case
/// flag. Wire callers retain HashSet iteration for inline flags; native callers
/// inspect membership and retain their original RegexBuilder representation.
pub fn regexp_match_flags(
    match_type: &str,
    initial_ci: bool,
) -> Result<HashSet<char>, RegexpPolicyError> {
    let mut flags = HashSet::new();
    if initial_ci {
        flags.insert('i');
    }
    for flag in match_type.chars() {
        match flag {
            'c' => {
                flags.remove(&'i');
            }
            'i' | 'm' | 's' => {
                flags.insert(flag);
            }
            _ => return Err(RegexpPolicyError::InvalidMatchType(flag)),
        }
    }
    Ok(flags)
}

/// Return the byte offset and suffix for a one-based character position.
/// The start of an empty string is valid; the position after a nonempty string
/// is not. Kept separate from compilation so each frontend preserves
/// precedence.
pub fn regexp_trim_at(text: &str, pos: i64) -> Result<(usize, &str), RegexpPolicyError> {
    if pos >= 1 {
        if let Some((byte, _)) = text.char_indices().nth((pos - 1) as usize) {
            return Ok((byte, &text[byte..]));
        }
        if pos == 1 {
            return Ok((0, text));
        }
    }
    Err(RegexpPolicyError::InvalidPosition {
        pos,
        count: text.chars().count(),
    })
}

fn nth_match<'a>(regexp: &Regex, text: &'a str, occurrence: i64) -> Option<Match<'a>> {
    regexp.find_iter(text).nth((occurrence.max(1) - 1) as usize)
}

/// Find a substring in already-trimmed text, normalizing the raw occurrence.
pub fn regexp_substr_match<'a>(regexp: &Regex, text: &'a str, occurrence: i64) -> Option<&'a str> {
    nth_match(regexp, text, occurrence).map(|matched| matched.as_str())
}

/// Find a one-based character position in already-trimmed text. The caller has
/// validated return_option at its original point, before or after compilation.
pub fn regexp_instr_match(
    regexp: &Regex,
    text: &str,
    pos: i64,
    occurrence: i64,
    return_option: i64,
) -> i64 {
    match nth_match(regexp, text, occurrence) {
        Some(matched) => {
            let byte = if return_option == 0 {
                matched.start()
            } else {
                matched.end()
            };
            text[..byte].chars().count() as i64 + pos
        }
        None => 0,
    }
}

/// Tokenize the original wire replacement syntax: exactly one digit after a
/// slash is a capture reference, other escaped bytes are literal, trailing
/// slash is ignored. In particular `$1` is literal and `\12` is group 1 then
/// `2`.
pub fn regexp_replacement_parts(replacement: &[u8]) -> Vec<NativeReplacementPart> {
    let mut parts = Vec::new();
    let mut literal = Vec::new();
    let mut index = 0;
    while index < replacement.len() {
        if replacement[index] == b'\\' {
            if index + 1 >= replacement.len() {
                break;
            }
            if replacement[index + 1].is_ascii_digit() {
                if !literal.is_empty() {
                    parts.push(NativeReplacementPart::Literal(std::mem::take(&mut literal)));
                }
                parts.push(NativeReplacementPart::Group(
                    (replacement[index + 1] - b'0') as usize,
                ));
            } else {
                literal.push(replacement[index + 1]);
            }
            index += 2;
        } else {
            literal.push(replacement[index]);
            index += 1;
        }
    }
    if !literal.is_empty() {
        parts.push(NativeReplacementPart::Literal(literal));
    }
    parts
}

fn render_replacement(
    capture: &Captures<'_>,
    parts: &[NativeReplacementPart],
    encoding: RegexpReplacementEncoding,
) -> Result<Vec<u8>, RegexpPolicyError> {
    let mut rendered = Vec::new();
    for part in parts {
        match part {
            NativeReplacementPart::Group(group) => {
                let matched = capture
                    .get(*group)
                    .ok_or(RegexpPolicyError::InvalidSubstitution(*group))?;
                rendered.extend_from_slice(matched.as_str().as_bytes());
            }
            NativeReplacementPart::Literal(literal) => rendered.extend_from_slice(literal),
        }
    }
    if encoding == RegexpReplacementEncoding::NativeUtf8 {
        std::str::from_utf8(&rendered).map_err(RegexpPolicyError::InvalidReplacementUtf8)?;
    }
    Ok(rendered)
}

/// Replace matches in an already-trimmed suffix and prepend the original
/// prefix. Raw negative occurrences select the first match; zero selects all
/// matches. Validation runs only for each selected replacement, before
/// advancing to the next capture. Allocation strategy is not an allocator-peak
/// or OOM guarantee.
pub fn regexp_replace_matches(
    prefix: &str,
    text: &str,
    regexp: &Regex,
    parts: &[NativeReplacementPart],
    occurrence: i64,
    encoding: RegexpReplacementEncoding,
) -> Result<Vec<u8>, RegexpPolicyError> {
    let occurrence = if occurrence < 0 { 1 } else { occurrence };
    let mut output = Vec::new();
    output.extend_from_slice(prefix.as_bytes());
    let mut last_match = 0;
    if occurrence == 0 {
        for capture in regexp.captures_iter(text) {
            // Capture zero always exists for a reported match.
            let matched = capture.get(0).unwrap();
            output.extend_from_slice(&text.as_bytes()[last_match..matched.start()]);
            last_match = matched.end();
            output.extend_from_slice(&render_replacement(&capture, parts, encoding)?);
        }
    } else if let Some(capture) = regexp.captures_iter(text).nth((occurrence - 1) as usize) {
        let matched = capture.get(0).unwrap();
        output.extend_from_slice(&text.as_bytes()[..matched.start()]);
        last_match = matched.end();
        output.extend_from_slice(&render_replacement(&capture, parts, encoding)?);
    }
    output.extend_from_slice(&text.as_bytes()[last_match..]);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-derived fixed policy boundaries, not recorded implementation output.
    #[test]
    fn regexp_policy_flags_positions_and_nth_match() {
        assert!(regexp_match_flags("", true).unwrap().contains(&'i'));
        assert!(!regexp_match_flags("ic", true).unwrap().contains(&'i'));
        assert_eq!(
            regexp_match_flags("cims", false).unwrap(),
            HashSet::from(['i', 'm', 's'])
        );
        assert_eq!(
            regexp_match_flags("x", false),
            Err(RegexpPolicyError::InvalidMatchType('x'))
        );
        assert_eq!(regexp_trim_at("", 1).unwrap(), (0, ""));
        assert_eq!(regexp_trim_at("你好", 2).unwrap(), (3, "好"));
        assert_eq!(
            regexp_trim_at("你好", 3),
            Err(RegexpPolicyError::InvalidPosition { pos: 3, count: 2 })
        );
        assert!(regexp_trim_at("", 0).is_err());
        let regexp = Regex::new(".").unwrap();
        assert_eq!(regexp_substr_match(&regexp, "你好", i64::MIN), Some("你"));
        assert_eq!(regexp_substr_match(&regexp, "你好", 2), Some("好"));
        assert_eq!(regexp_substr_match(&regexp, "你好", 3), None);
        assert_eq!(regexp_instr_match(&regexp, "好", 2, 0, 0), 2);
        assert_eq!(regexp_instr_match(&regexp, "好", 2, -1, 1), 3);
        assert_eq!(regexp_instr_match(&regexp, "好", 2, 2, 1), 0);
    }

    #[test]
    fn regexp_policy_single_digit_tokens_and_capture_errors() {
        assert_eq!(
            regexp_replacement_parts(b"z\\1\\12\\x\\"),
            vec![
                NativeReplacementPart::Literal(b"z".to_vec()),
                NativeReplacementPart::Group(1),
                NativeReplacementPart::Group(1),
                NativeReplacementPart::Literal(b"2x".to_vec()),
            ]
        );
        let regexp = Regex::new("(a)(b)?").unwrap();
        let parts = regexp_replacement_parts(b"\\1$1");
        assert_eq!(
            regexp_replace_matches(
                "前",
                "ab ab",
                &regexp,
                &parts,
                -5,
                RegexpReplacementEncoding::NativeUtf8
            )
            .unwrap(),
            "前a$1 ab".as_bytes()
        );
        assert_eq!(
            regexp_replace_matches(
                "",
                "ab ab",
                &regexp,
                &parts,
                0,
                RegexpReplacementEncoding::WireBytes
            )
            .unwrap(),
            b"a$1 a$1"
        );
        assert_eq!(
            regexp_replace_matches(
                "",
                "a",
                &regexp,
                &regexp_replacement_parts(b"\\2"),
                1,
                RegexpReplacementEncoding::WireBytes
            ),
            Err(RegexpPolicyError::InvalidSubstitution(2))
        );
    }

    #[test]
    fn regexp_policy_utf8_is_checked_only_for_each_selected_replacement() {
        let regexp = Regex::new("(a)|(b)").unwrap();
        let parts = vec![
            NativeReplacementPart::Literal(vec![0xff]),
            NativeReplacementPart::Group(1),
        ];
        // The first selected replacement is invalid UTF-8. Waiting until the
        // whole result was built would wrongly report the second missing group.
        assert!(matches!(
            regexp_replace_matches(
                "",
                "ab",
                &regexp,
                &parts,
                0,
                RegexpReplacementEncoding::NativeUtf8
            ),
            Err(RegexpPolicyError::InvalidReplacementUtf8(_))
        ));
        assert_eq!(
            regexp_replace_matches(
                "",
                "ab",
                &regexp,
                &parts,
                0,
                RegexpReplacementEncoding::WireBytes
            ),
            Err(RegexpPolicyError::InvalidSubstitution(1))
        );
        assert_eq!(
            regexp_replace_matches(
                "",
                "a",
                &regexp,
                &parts,
                1,
                RegexpReplacementEncoding::WireBytes
            )
            .unwrap(),
            vec![0xff, b'a']
        );
        assert_eq!(
            regexp_replace_matches(
                "前",
                "z",
                &regexp,
                &parts,
                0,
                RegexpReplacementEncoding::NativeUtf8
            )
            .unwrap(),
            "前z".as_bytes()
        );
        assert_eq!(
            regexp_replace_matches(
                "",
                "a",
                &regexp,
                &parts,
                2,
                RegexpReplacementEncoding::NativeUtf8
            )
            .unwrap(),
            b"a"
        );
    }
}
