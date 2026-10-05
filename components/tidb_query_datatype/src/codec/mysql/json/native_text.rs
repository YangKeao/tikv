// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native SDK raw JSON display and escape policies. These deliberately differ
//! from serde expression formatting and wire JSON unquoting.

use std::fmt;

use serde_json::Value;

use super::native_policy::{
    NativeJsonError, NativeJsonNode, decode_native_binary_json_node,
    decode_native_binary_json_value, decode_native_json_uvarint, native_json_opaque,
};
use crate::codec::mysql::{Duration, Time};

/// Projects the native SDK string view without full validation. Trailing
/// payload bytes are accepted, and UTF-8 remains the caller's original
/// admission step.
pub fn native_binary_json_string_bytes(type_code: u8, value: &[u8]) -> Option<&[u8]> {
    if type_code != 0x0c {
        return None;
    }
    let (length, prefix) = decode_native_json_uvarint(value).ok()?;
    value.get(prefix..prefix + length)
}

/// Writes the original native BinaryJSON display. A root nonfinite double is a
/// formatting error; malformed containers (including nested nonfinite doubles)
/// display as empty text after full decoding fails. Callers using to_string's
/// contract must propagate its formatting-error panic, not return SQL NULL.
pub fn write_native_binary_json_text<W: fmt::Write + ?Sized>(
    out: &mut W,
    type_code: u8,
    value: &[u8],
) -> fmt::Result {
    if let Ok((opaque_type, bytes)) = native_json_opaque(type_code, value) {
        return write!(
            out,
            "\"base64:type{}:{}\"",
            opaque_type,
            encode_base64(bytes)
        );
    }
    if matches!(type_code, 0x0e | 0x0f | 0x10) {
        if let Ok(raw) = <[u8; 8]>::try_from(value) {
            let mut text = String::new();
            Time::write_native_core_display(
                u64::from_le_bytes(raw),
                type_code == 0x0e,
                6,
                &mut text,
            )
            .expect("a Display implementation returned an error unexpectedly");
            return out.write_str(&quote_native_json_string(&text));
        }
    }
    if type_code == 0x11 && value.len() == 12 {
        let nanos = i64::from_le_bytes(value[..8].try_into().unwrap());
        let mut text = String::new();
        // The native source reconstructs at FSP 6, ignoring stored FSP entirely.
        Duration::write_native_display(nanos, 6, &mut text)
            .expect("a Display implementation returned an error unexpectedly");
        return out.write_str(&quote_native_json_string(&text));
    }
    if type_code == 0x0b {
        if let Ok(raw) = <[u8; 8]>::try_from(value) {
            let value = f64::from_bits(u64::from_le_bytes(raw));
            return out.write_str(&format_float64(value).ok_or(fmt::Error)?);
        }
    }
    if matches!(type_code, 0x03 | 0x01) {
        return match decode_native_binary_json_node(type_code, value) {
            Ok(node) => out.write_str(&format_node(&node)),
            Err(_) => Ok(()),
        };
    }
    match decode_native_binary_json_value(type_code, value) {
        Ok(value) => out.write_str(&format_value(&value)),
        Err(_) => Ok(()),
    }
}

/// Native BinaryJSON.Unquote: string projection admits UTF-8 before the
/// optional second unescape; every other value uses the original Display path.
pub fn native_unquote_binary_json(type_code: u8, value: &[u8]) -> Result<String, NativeJsonError> {
    match native_binary_json_string_bytes(type_code, value) {
        Some(bytes) => {
            let text = std::str::from_utf8(bytes).map_err(|_| NativeJsonError::InvalidBinary)?;
            unquote_native_json_string(text).map_err(|_| NativeJsonError::InvalidText)
        }
        None => Ok(NativeBinaryJsonDisplay { type_code, value }.to_string()),
    }
}

// ToString's standard formatting-error panic is part of the original boundary.
struct NativeBinaryJsonDisplay<'a> {
    type_code: u8,
    value: &'a [u8],
}

impl fmt::Display for NativeBinaryJsonDisplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_native_binary_json_text(formatter, self.type_code, self.value)
    }
}

fn format_float64(value: f64) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    let absolute = value.abs();
    if absolute != 0.0 && !(1e-15..1e15).contains(&absolute) {
        return Some(format!("{value:e}"));
    }
    let mut output = value.to_string();
    if !output.contains('.') {
        output.push_str(".0");
    }
    Some(output)
}

/// The SDK's optional outer-quote removal and permissive second unescape. This
/// is not the SQL expression's strict JSON-document string parser.
pub fn unquote_native_json_string(text: &str) -> Result<String, NativeJsonError> {
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        return unquote_native_json_escaped_string(&text[1..text.len() - 1]);
    }
    Ok(text.to_owned())
}

/// Quotes a native SDK path key unless the original byte-based identifier rule
/// accepts it without any JSON escaping.
pub fn quote_native_json_string(text: &str) -> String {
    let quoted = marshal_json_string(text);
    if is_ecmascript_identifier(text)
        && quoted.as_bytes()[1..quoted.len() - 1] == text.as_bytes()[..]
    {
        text.to_owned()
    } else {
        quoted
    }
}

fn marshal_json_string(text: &str) -> String {
    let quoted = serde_json::to_string(text).expect("Rust string is valid JSON text");
    if !quoted
        .chars()
        .any(|ch| matches!(ch, '\u{2028}' | '\u{2029}'))
    {
        return quoted;
    }
    let mut escaped = String::with_capacity(quoted.len() + 5);
    for ch in quoted.chars() {
        match ch {
            '\u{2028}' => escaped.push_str("\\u2028"),
            '\u{2029}' => escaped.push_str("\\u2029"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Decodes native SDK JSON_UNQUOTE escapes, including unknown-escape slash
/// removal and the source's adjacent surrogate-pair requirement.
pub fn unquote_native_json_escaped_string(text: &str) -> Result<String, NativeJsonError> {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        let (_, escaped) = chars.next().ok_or(NativeJsonError::InvalidText)?;
        match escaped {
            '"' => output.push('"'),
            'b' => output.push('\u{8}'),
            'f' => output.push('\u{c}'),
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            '\\' => output.push('\\'),
            'u' => {
                let mut first = [0_u8; 4];
                for byte in &mut first {
                    *byte = chars
                        .next()
                        .and_then(|(_, ch)| ch.is_ascii().then_some(ch as u8))
                        .ok_or(NativeJsonError::InvalidText)?;
                }
                let first = decode_hex_u16(&first)?;
                let scalar = if (0xd800..=0xdbff).contains(&first)
                    || (0xdc00..=0xdfff).contains(&first)
                {
                    // A surrogate requires an ADJACENT escape; an invalid pair
                    // consumes both escapes and substitutes U+FFFD.
                    let adjacent_escape = chars.next().map(|(_, ch)| ch) == Some('\\')
                        && chars.next().map(|(_, ch)| ch) == Some('u');
                    if !adjacent_escape {
                        return Err(NativeJsonError::InvalidText);
                    }
                    let mut second = [0_u8; 4];
                    for byte in &mut second {
                        *byte = chars
                            .next()
                            .and_then(|(_, ch)| ch.is_ascii().then_some(ch as u8))
                            .ok_or(NativeJsonError::InvalidText)?;
                    }
                    let second = decode_hex_u16(&second)?;
                    if (0xd800..=0xdbff).contains(&first) && (0xdc00..=0xdfff).contains(&second) {
                        0x10000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00)
                    } else {
                        0xFFFD
                    }
                } else {
                    u32::from(first)
                };
                output.push(char::from_u32(scalar).ok_or(NativeJsonError::InvalidText)?);
            }
            other => output.push(other),
        }
    }
    Ok(output)
}

/// The SDK's separate four/eight-hex-digit Unicode helper, retaining its final
/// false flag and its substitution behavior for lone or invalid surrogates.
pub fn decode_native_json_escaped_unicode(
    hex: &[u8],
) -> Result<([u8; 4], usize, bool), NativeJsonError> {
    if hex.len() != 4 && hex.len() != 8 {
        return Err(NativeJsonError::InvalidText);
    }
    let first = decode_hex_u16(hex.get(..4).ok_or(NativeJsonError::InvalidText)?)?;
    let scalar = if hex.len() == 8 {
        let second = decode_hex_u16(hex.get(4..).ok_or(NativeJsonError::InvalidText)?)?;
        if (0xd800..=0xdbff).contains(&first) && (0xdc00..=0xdfff).contains(&second) {
            0x10000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00)
        } else {
            0xFFFD
        }
    } else if (0xd800..=0xdfff).contains(&first) {
        0xFFFD
    } else {
        u32::from(first)
    };
    let ch = char::from_u32(scalar).ok_or(NativeJsonError::InvalidText)?;
    let mut output = [0_u8; 4];
    let size = ch.encode_utf8(&mut output).len();
    Ok((output, size, false))
}

fn decode_hex_u16(hex: &[u8]) -> Result<u16, NativeJsonError> {
    if hex.len() != 4 {
        return Err(NativeJsonError::InvalidText);
    }
    hex.iter().try_fold(0_u16, |value, byte| {
        let digit = (*byte as char)
            .to_digit(16)
            .ok_or(NativeJsonError::InvalidText)?;
        Ok((value << 4) | digit as u16)
    })
}

fn is_ecmascript_identifier(value: &str) -> bool {
    let bytes = value.as_bytes();
    let Some(&first) = bytes.first() else {
        return false;
    };
    let is_letter = |byte: u8| {
        byte.is_ascii_alphabetic()
            || matches!(byte, 0xAA | 0xB5 | 0xBA | 0xC0..=0xD6 | 0xD8..=0xF6 | 0xF8..=0xFF)
    };
    (is_letter(first) || first == b'$' || first == b'_')
        && bytes[1..]
            .iter()
            .all(|byte| is_letter(*byte) || byte.is_ascii_digit() || matches!(byte, b'$' | b'_'))
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(TABLE[((value >> 18) & 0x3f) as usize] as char);
        output.push(TABLE[((value >> 12) & 0x3f) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((value >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(value & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => marshal_json_string(value),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(format_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => {
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| format!(
                        "{}: {}",
                        marshal_json_string(key),
                        format_value(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

fn format_node(value: &NativeJsonNode<(u8, Vec<u8>)>) -> String {
    match value {
        NativeJsonNode::Scalar((type_code, value)) => {
            let mut text = String::new();
            write_native_binary_json_text(&mut text, *type_code, value)
                .expect("a Display implementation returned an error unexpectedly");
            text
        }
        NativeJsonNode::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(format_node)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        NativeJsonNode::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!("{}: {}", marshal_json_string(key), format_node(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(type_code: u8, value: &[u8]) -> Result<String, fmt::Error> {
        let mut text = String::new();
        write_native_binary_json_text(&mut text, type_code, value)?;
        Ok(text)
    }

    #[test]
    fn sdk_escape_policy_and_direct_string_projection_remain_distinct() {
        let payload = [4, b'"', b'\\', b'n', b'"', 0xff];
        let contents = native_binary_json_string_bytes(0x0c, &payload).unwrap();
        assert_eq!(contents, b"\"\\n\"");
        assert_eq!(
            unquote_native_json_string(std::str::from_utf8(contents).unwrap()).unwrap(),
            "\n"
        );
        assert_eq!(
            native_binary_json_string_bytes(0x0c, &[1, 0xff]),
            Some(&[0xff][..])
        );
        assert_eq!(native_binary_json_string_bytes(0x0c, &[4, b'a']), None);
        assert_eq!(native_binary_json_string_bytes(0x03, &payload), None);
        assert_eq!(unquote_native_json_string(r#""a\qb\/c""#).unwrap(), "aqb/c");
        assert_eq!(unquote_native_json_string(r"\uD800").unwrap(), r"\uD800");
        assert_eq!(
            unquote_native_json_escaped_string(r"\uD800"),
            Err(NativeJsonError::InvalidText)
        );
        assert_eq!(
            unquote_native_json_escaped_string("end\\"),
            Err(NativeJsonError::InvalidText)
        );
        assert_eq!(
            unquote_native_json_escaped_string(r"\uD800\u0041").unwrap(),
            "\u{fffd}"
        );
        assert_eq!(
            unquote_native_json_escaped_string(r"\uD83D\uDE00").unwrap(),
            "😀"
        );
        let (bytes, size, flag) = decode_native_json_escaped_unicode(b"d800").unwrap();
        assert_eq!(&bytes[..size], "\u{fffd}".as_bytes());
        assert!(!flag);
        assert_eq!(
            decode_native_json_escaped_unicode(b"xyz!"),
            Err(NativeJsonError::InvalidText)
        );
        assert_eq!(quote_native_json_string("identifier"), "identifier");
        assert_eq!(quote_native_json_string("µ"), "µ");
        assert_eq!(quote_native_json_string("\u{2028}"), "\"\\u2028\"");
    }

    #[test]
    fn raw_display_keeps_order_tags_temporal_bytes_and_nonfinite_error_boundary() {
        let mut object = vec![
            2, 0, 0, 0, 32, 0, 0, 0, 30, 0, 0, 0, 1, 0, 31, 0, 0, 0, 1, 0, 0x04, 1, 0, 0, 0, 0x04,
            2, 0, 0, 0, b'b', b'a',
        ];
        assert_eq!(text(0x01, &object).unwrap(), r#"{"b": true, "a": false}"#);
        *object.last_mut().unwrap() = b'b';
        assert_eq!(text(0x01, &object).unwrap(), r#"{"b": true, "b": false}"#);
        assert_eq!(
            text(0x0d, &[15, 2, 0xff, 0]).unwrap(),
            "\"base64:type15:/wA=\""
        );
        assert_eq!(
            text(0x0c, &[6, b'<', b'>', b'&', 0xe2, 0x80, 0xa8]).unwrap(),
            "\"<>&\\u2028\""
        );
        assert_eq!(text(0x0e, &0_u64.to_le_bytes()).unwrap(), "\"0000-00-00\"");
        assert_eq!(
            text(0x0f, &(0x0f_ffff_u64 << 4).to_le_bytes()).unwrap(),
            "\"0000-00-00 00:00:00.104857\""
        );
        let mut duration = (-1_i64).to_le_bytes().to_vec();
        duration.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(text(0x11, &duration).unwrap(), "\"-00:00:00.000000\"");
        assert_eq!(text(0x0b, &(1_u64 << 63).to_le_bytes()).unwrap(), "-0.0");
        assert_eq!(text(0xfe, &[1]).unwrap(), "");
        assert_eq!(text(0x03, &[0]).unwrap(), "");
        let nan = 0x7ff8_0000_0000_0042_u64.to_le_bytes().to_vec();
        assert_eq!(text(0x0b, &nan), Err(fmt::Error));
        let node = NativeJsonNode::Array(vec![NativeJsonNode::Scalar((0x0b, nan))]);
        let (kind, payload) =
            super::super::encode_native_binary_json_node(&node, |value| (value.0, &value.1))
                .unwrap();
        assert_eq!(text(kind, &payload).unwrap(), "");
    }
}

#[cfg(test)]
#[test]
fn binary_json_unquote_keeps_projection_errors_second_unescape_and_display_panic() {
    assert_eq!(
        native_unquote_binary_json(0x0c, &[3, b'a', b'b', b'c', 0xff]).unwrap(),
        "abc"
    );
    assert_eq!(
        native_unquote_binary_json(0x0c, &[4, b'"', b'\\', b'n', b'"']).unwrap(),
        "\n"
    );
    assert_eq!(
        native_unquote_binary_json(0x09, &[41, 0, 0, 0, 0, 0, 0, 0]).unwrap(),
        "41"
    );
    assert_eq!(
        native_unquote_binary_json(0x0e, &[0; 8]).unwrap(),
        "\"0000-00-00\""
    );
    assert_eq!(
        native_unquote_binary_json(0x0c, &[1, 0xff]),
        Err(NativeJsonError::InvalidBinary)
    );
    assert_eq!(
        native_unquote_binary_json(0x0c, &[5, b'"', b'\\', b'u', b'x', b'"']),
        Err(NativeJsonError::InvalidText)
    );
    assert_eq!(native_unquote_binary_json(0xfe, &[1]).unwrap(), "");
    assert_eq!(native_unquote_binary_json(0x0c, &[2, b'a']).unwrap(), "");
    let panic =
        std::panic::catch_unwind(|| native_unquote_binary_json(0x0b, &f64::INFINITY.to_le_bytes()))
            .unwrap_err();
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .expect("standard formatting panic text");
    assert!(message.contains("a Display implementation returned an error unexpectedly"));
}
