// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native integer conversion and its ordered diagnostic effects. The byte
//! scanner is shared with native float conversion, not with the wire parser.
//! JSON string routing deliberately precedes trimming and ignores target
//! bounds.
use super::{
    mysql::json::native_binary_json_string_bytes, native_float_parse::native_valid_float_prefix,
    native_type_name::NativeTypeNameCode,
};

/// Best-effort numeric-helper parsing errors, distinct from StrToInt's
/// floating-prefix/scientific policy and its conversion diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeStringToIntError {
    Truncated,
    BadNumber,
}
impl std::fmt::Display for NativeStringToIntError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Truncated => "truncated",
            Self::BadNumber => "bad number",
        })
    }
}
impl std::error::Error for NativeStringToIntError {}
/// Native types.strToInt: Unicode trim, ASCII digit scan, and signed
/// saturation. Accumulator/signed-limit failure takes precedence over trailing
/// junk.
pub fn native_string_to_int(value: &str) -> Result<i64, (i64, NativeStringToIntError)> {
    let value = value.trim();
    if value.is_empty() {
        return Err((0, NativeStringToIntError::Truncated));
    }
    let bytes = value.as_bytes();
    let (negative, mut index) = match bytes[0] {
        b'-' => (true, 1),
        b'+' => (false, 1),
        _ => (false, 0),
    };
    let mut magnitude = 0_u64;
    let mut has_number = false;
    let mut trailing = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if !byte.is_ascii_digit() {
            trailing = true;
            break;
        }
        has_number = true;
        let Some(next) = magnitude
            .checked_mul(10)
            .and_then(|number| number.checked_add(u64::from(byte - b'0')))
        else {
            return Err((
                if negative { i64::MIN } else { i64::MAX },
                NativeStringToIntError::BadNumber,
            ));
        };
        magnitude = next;
        index += 1;
    }
    if !has_number {
        return Err((0, NativeStringToIntError::Truncated));
    }
    let limit = i64::MAX as u64 + u64::from(negative);
    if magnitude > limit {
        return Err((
            if negative { i64::MIN } else { i64::MAX },
            NativeStringToIntError::BadNumber,
        ));
    }
    let output = if negative {
        (0_u64.wrapping_sub(magnitude)) as i64
    } else {
        magnitude as i64
    };
    if trailing {
        Err((output, NativeStringToIntError::Truncated))
    } else {
        Ok(output)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeIntegerConverted<T> {
    pub value: T,
    pub event: Option<NativeIntegerEvent>,
}
impl<T> NativeIntegerConverted<T> {
    pub fn exact(value: T) -> Self {
        Self { value, event: None }
    }
    pub fn truncated(value: T) -> Self {
        Self {
            value,
            event: Some(NativeIntegerEvent::Truncated),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeIntegerEvent {
    Truncated,
    Overflow(NativeIntegerError),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeIntegerError {
    Overflow {
        value: String,
        target: NativeTypeNameCode,
    },
    InvalidUnsignedInteger(String),
}
/// Requests only the original generic diagnostic effects, at their original
/// execution points. Borrowed temporary subjects cannot escape a callback.
#[derive(Clone, Copy, Debug)]
pub enum NativeIntegerDiagnostic<'a> {
    TruncatedNumericInput(&'a str),
    ParsedInteger(&'a NativeIntegerEvent),
    ErrorOverflow(&'a str),
    ReplaceUnsignedOverflow(&'a str),
}
use NativeTypeNameCode::Known;
fn overflow(value: impl ToString, target: NativeTypeNameCode) -> NativeIntegerError {
    NativeIntegerError::Overflow {
        value: value.to_string(),
        target,
    }
}
fn converted_result<T>(result: Result<T, (T, NativeIntegerError)>) -> NativeIntegerConverted<T> {
    match result {
        Ok(value) => NativeIntegerConverted::exact(value),
        Err((value, error)) => NativeIntegerConverted {
            value,
            event: Some(NativeIntegerEvent::Overflow(error)),
        },
    }
}

pub const fn native_integer_unsigned_upper_bound(target: NativeTypeNameCode) -> u64 {
    match target {
        Known(1) => u8::MAX as u64,
        Known(2) => u16::MAX as u64,
        Known(9) => 0x00ff_ffff,
        Known(3) => u32::MAX as u64,
        Known(8 | 16 | 248) => u64::MAX,
        Known(247) => u16::MAX as u64,
        _ => panic!("input is not a MySQL integer type"),
    }
}
pub const fn native_integer_signed_upper_bound(target: NativeTypeNameCode) -> i64 {
    match target {
        Known(1) => i8::MAX as i64,
        Known(2) => i16::MAX as i64,
        Known(9) => 0x007f_ffff,
        Known(3) => i32::MAX as i64,
        Known(8) => i64::MAX,
        Known(247) => u16::MAX as i64,
        _ => panic!("input is not a MySQL signed integer type"),
    }
}
pub const fn native_integer_signed_lower_bound(target: NativeTypeNameCode) -> i64 {
    match target {
        Known(1) => i8::MIN as i64,
        Known(2) => i16::MIN as i64,
        Known(9) => -0x0080_0000,
        Known(3) => i32::MIN as i64,
        Known(8) => i64::MIN,
        Known(247) => 0,
        _ => panic!("input is not a MySQL integer type"),
    }
}
/// The original round_float is ties-even, despite its old caller's comment.
pub fn native_convert_float_to_int(
    value: f64,
    lower_bound: i64,
    upper_bound: i64,
    target: NativeTypeNameCode,
) -> Result<i64, (i64, NativeIntegerError)> {
    let rounded = value.round_ties_even();
    if rounded < lower_bound as f64 {
        return Err((lower_bound, overflow(rounded, target)));
    }
    if rounded >= upper_bound as f64 {
        if rounded == upper_bound as f64 {
            return Ok(upper_bound);
        }
        return Err((upper_bound, overflow(rounded, target)));
    }
    Ok(rounded as i64)
}
pub fn native_convert_int_to_int(
    value: i64,
    lower_bound: i64,
    upper_bound: i64,
    target: NativeTypeNameCode,
) -> Result<i64, (i64, NativeIntegerError)> {
    if value < lower_bound {
        Err((lower_bound, overflow(value, target)))
    } else if value > upper_bound {
        Err((upper_bound, overflow(value, target)))
    } else {
        Ok(value)
    }
}
pub fn native_convert_uint_to_int(
    value: u64,
    upper_bound: i64,
    target: NativeTypeNameCode,
) -> Result<i64, (i64, NativeIntegerError)> {
    if value > upper_bound as u64 {
        Err((upper_bound, overflow(value, target)))
    } else {
        Ok(value as i64)
    }
}
pub fn native_convert_int_to_uint(
    flags: u16,
    value: i64,
    upper_bound: u64,
    target: NativeTypeNameCode,
) -> Result<u64, (u64, NativeIntegerError)> {
    if value < 0 && flags & (1 << 2) == 0 {
        return Err((0, overflow(value, target)));
    }
    let converted = value as u64;
    if converted > upper_bound {
        Err((upper_bound, overflow(value, target)))
    } else {
        Ok(converted)
    }
}
pub fn native_convert_uint_to_uint(
    value: u64,
    upper_bound: u64,
    target: NativeTypeNameCode,
) -> Result<u64, (u64, NativeIntegerError)> {
    if value > upper_bound {
        Err((upper_bound, overflow(value, target)))
    } else {
        Ok(value)
    }
}
pub fn native_convert_float_to_uint(
    flags: u16,
    value: f64,
    upper_bound: u64,
    target: NativeTypeNameCode,
) -> Result<u64, (u64, NativeIntegerError)> {
    let rounded = value.round_ties_even();
    assert!(!rounded.is_nan(), "Float.SetFloat64(NaN)");
    if rounded < 0.0 {
        if flags & (1 << 2) == 0 {
            return Err((0, overflow(rounded, target)));
        }
        let converted = (rounded as i64) as u64;
        return Err((converted, overflow(rounded, target)));
    }
    if !rounded.is_finite() || rounded >= u64::MAX as f64 {
        return Err((upper_bound, overflow(rounded, target)));
    }
    let converted = rounded as u64;
    if converted > upper_bound {
        Err((upper_bound, overflow(rounded, target)))
    } else {
        Ok(converted)
    }
}

pub fn native_round_integer_string(next_fraction_digit: u8, integer: &str) -> String {
    if next_fraction_digit < b'5' {
        return integer.to_owned();
    }
    let mut result = integer.as_bytes().to_vec();
    let mut index = result.len() - 1;
    while index >= 1 {
        if result[index] != b'9' {
            result[index] += 1;
            return String::from_utf8(result).expect("integer input is ASCII");
        }
        result[index] = b'0';
        index -= 1;
    }
    match result[0] {
        b'9' => {
            result[0] = b'1';
            result.push(b'0');
        }
        b'0'..=b'8' => result[0] += 1,
        b'+' | b'-' => {
            result[1] = b'1';
            result.push(b'0');
        }
        _ => unreachable!("integer input is valid"),
    }
    String::from_utf8(result).expect("integer input is ASCII")
}
/// Original bounded exponent expansion, retaining saturated text on failure.
/// Do not replace with the separate, unbounded scientific-notation expander.
pub fn native_float_string_to_integer_string(
    valid_float: &str,
    original: &str,
) -> Result<String, (String, NativeIntegerError)> {
    let bytes = valid_float.as_bytes();
    let dot_index = bytes.iter().position(|byte| *byte == b'.');
    let exponent_index = bytes.iter().position(|byte| matches!(byte, b'e' | b'E'));
    let Some(exponent_index) = exponent_index else {
        let Some(mut dot_index) = dot_index else {
            return Ok(valid_float.to_owned());
        };
        let signed = matches!(bytes.first(), Some(b'+' | b'-'));
        let digits = if signed {
            dot_index -= 1;
            &bytes[1..]
        } else {
            bytes
        };
        let mut integer = if dot_index == 0 {
            "0".to_owned()
        } else {
            String::from_utf8(digits[..dot_index].to_vec()).expect("numeric input is ASCII")
        };
        if digits.len() > dot_index + 1 {
            integer = native_round_integer_string(digits[dot_index + 1], &integer);
        }
        if (integer.len() > 1 || integer.as_bytes()[0] != b'0') && bytes.first() == Some(&b'-') {
            integer.insert(0, '-');
        }
        return Ok(integer);
    };
    let mut digits = Vec::with_capacity(valid_float.len());
    let mut integer_count;
    if let Some(dot_index) = dot_index {
        digits.extend_from_slice(&bytes[..dot_index]);
        integer_count = digits.len() as i128;
        digits.extend_from_slice(&bytes[dot_index + 1..exponent_index]);
    } else {
        digits.extend_from_slice(&bytes[..exponent_index]);
        integer_count = digits.len() as i128;
    }
    let exponent = valid_float[exponent_index + 1..]
        .parse::<i128>()
        .map_err(|_| {
            let saturated = if digits.first() == Some(&b'-') {
                i64::MIN.to_string()
            } else {
                u64::MAX.to_string()
            };
            (saturated, overflow(original, Known(8)))
        })?;
    // Preserve the original arithmetic/panic boundary, after any diagnostic
    // already emitted by the calling reported conversion.
    integer_count += exponent;
    if exponent >= 0 && !(0..=21).contains(&integer_count) {
        let saturated = if digits.first() == Some(&b'-') {
            i64::MIN.to_string()
        } else {
            u64::MAX.to_string()
        };
        return Err((saturated, overflow(original, Known(8))));
    }
    if integer_count <= 0 {
        let mut integer = "0".to_owned();
        if integer_count == 0 && digits.first().is_some_and(u8::is_ascii_digit) {
            integer = native_round_integer_string(digits[0], &integer);
        }
        return Ok(integer);
    }
    if integer_count == 1 && matches!(digits.first(), Some(b'+' | b'-')) {
        let mut integer = "0".to_owned();
        if digits.len() > 1 {
            integer = native_round_integer_string(digits[1], &integer);
        }
        if integer.starts_with('1') {
            integer.insert(0, digits[0] as char);
        }
        return Ok(integer);
    }
    if integer_count <= digits.len() as i128 {
        let count = integer_count as usize;
        let mut integer =
            String::from_utf8(digits[..count].to_vec()).expect("numeric input is ASCII");
        if count < digits.len() {
            integer = native_round_integer_string(digits[count], &integer);
        }
        Ok(integer)
    } else {
        let mut integer = String::from_utf8(digits).expect("numeric input is ASCII");
        integer.push_str(&"0".repeat(integer_count as usize - integer.len()));
        Ok(integer)
    }
}
fn function_cast_integer_prefix(input: &str) -> (String, bool) {
    let mut valid_len = 0;
    for (index, byte) in input.bytes().enumerate() {
        if matches!(byte, b'+' | b'-') && index == 0 {
            continue;
        }
        if byte.is_ascii_digit() {
            valid_len = index + 1;
            continue;
        }
        break;
    }
    let consumed_all = valid_len != 0 && valid_len == input.len();
    let prefix = if valid_len == 0 {
        "0".to_owned()
    } else {
        input[..valid_len].to_owned()
    };
    (prefix, consumed_all)
}
pub fn native_valid_integer_prefix(
    input: &str,
    is_function_cast: bool,
    truncate_as_warning: bool,
) -> Result<NativeIntegerConverted<String>, (String, NativeIntegerError)> {
    if !is_function_cast {
        let float = native_valid_float_prefix(input, false);
        if float.truncated && !truncate_as_warning {
            return Err((
                float.value.to_owned(),
                NativeIntegerError::InvalidUnsignedInteger(input.to_owned()),
            ));
        }
        let event = float.truncated.then_some(NativeIntegerEvent::Truncated);
        return native_float_string_to_integer_string(float.value, input)
            .map(|value| NativeIntegerConverted { value, event });
    }
    let (value, consumed_all) = function_cast_integer_prefix(input);
    if !consumed_all {
        if truncate_as_warning {
            return Ok(NativeIntegerConverted::truncated(value));
        }
        return Err((
            value,
            NativeIntegerError::InvalidUnsignedInteger(input.to_owned()),
        ));
    }
    Ok(NativeIntegerConverted::exact(value))
}

pub fn native_str_to_int(input: &str, is_function_cast: bool) -> NativeIntegerConverted<i64> {
    native_str_to_int_reported(input, is_function_cast, true, |_| {})
}
pub fn native_str_to_uint(input: &str, is_function_cast: bool) -> NativeIntegerConverted<u64> {
    native_str_to_uint_reported(input, is_function_cast, true, |_| {})
}
pub fn native_str_to_int_reported(
    input: &str,
    is_function_cast: bool,
    truncate_as_warning: bool,
    mut report: impl for<'a> FnMut(NativeIntegerDiagnostic<'a>),
) -> NativeIntegerConverted<i64> {
    let input = input.trim();
    let float = native_valid_float_prefix(input, is_function_cast);
    if float.truncated {
        report(NativeIntegerDiagnostic::TruncatedNumericInput(input));
    }
    let mut function_cast_consumed_all = true;
    let integer = if is_function_cast {
        let (prefix, consumed_all) = function_cast_integer_prefix(input);
        function_cast_consumed_all = consumed_all;
        prefix
    } else if float.truncated && !truncate_as_warning {
        float.value.to_owned()
    } else {
        match native_float_string_to_integer_string(float.value, input) {
            Ok(value) => value,
            Err((value, error)) => {
                let event = NativeIntegerEvent::Overflow(error);
                report(NativeIntegerDiagnostic::ParsedInteger(&event));
                return NativeIntegerConverted {
                    value: value.parse().unwrap_or_else(|_| {
                        if value.starts_with('-') {
                            i64::MIN
                        } else {
                            i64::MAX
                        }
                    }),
                    event: Some(event),
                };
            }
        }
    };
    match integer.parse::<i64>() {
        Ok(value) if float.truncated || (is_function_cast && !function_cast_consumed_all) => {
            NativeIntegerConverted::truncated(value)
        }
        Ok(value) => NativeIntegerConverted::exact(value),
        Err(error) => {
            let value = match error.kind() {
                std::num::IntErrorKind::PosOverflow => i64::MAX,
                std::num::IntErrorKind::NegOverflow => i64::MIN,
                _ => 0,
            };
            let event = NativeIntegerEvent::Overflow(overflow(&integer, Known(8)));
            report(NativeIntegerDiagnostic::ParsedInteger(&event));
            NativeIntegerConverted {
                value,
                event: Some(event),
            }
        }
    }
}
pub fn native_str_to_uint_reported(
    input: &str,
    is_function_cast: bool,
    truncate_as_warning: bool,
    mut report: impl for<'a> FnMut(NativeIntegerDiagnostic<'a>),
) -> NativeIntegerConverted<u64> {
    let input = input.trim();
    let float = native_valid_float_prefix(input, is_function_cast);
    if float.truncated {
        report(NativeIntegerDiagnostic::TruncatedNumericInput(input));
    }
    let mut function_cast_consumed_all = true;
    let mut prefix_error = None;
    let integer = if is_function_cast {
        let (prefix, consumed_all) = function_cast_integer_prefix(input);
        function_cast_consumed_all = consumed_all;
        prefix
    } else if float.truncated && !truncate_as_warning {
        float.value.to_owned()
    } else {
        match native_float_string_to_integer_string(float.value, input) {
            Ok(value) => value,
            Err((value, error)) => {
                report(NativeIntegerDiagnostic::ErrorOverflow(input));
                prefix_error = Some(NativeIntegerEvent::Overflow(error));
                value
            }
        }
    };
    let unsigned = integer.strip_prefix('+').unwrap_or(&integer);
    if let Some(magnitude) = unsigned.strip_prefix('-') {
        if magnitude.bytes().any(|byte| byte != b'0') {
            report(NativeIntegerDiagnostic::ReplaceUnsignedOverflow(&integer));
            return NativeIntegerConverted {
                value: 0,
                event: Some(NativeIntegerEvent::Overflow(overflow(&integer, Known(8)))),
            };
        }
        return if float.truncated {
            NativeIntegerConverted::truncated(0)
        } else {
            NativeIntegerConverted::exact(0)
        };
    }
    match unsigned.parse::<u64>() {
        Ok(value) if float.truncated || (is_function_cast && !function_cast_consumed_all) => {
            NativeIntegerConverted {
                value,
                event: prefix_error.or(Some(NativeIntegerEvent::Truncated)),
            }
        }
        Ok(value) => NativeIntegerConverted {
            value,
            event: prefix_error,
        },
        Err(error) => {
            report(NativeIntegerDiagnostic::ReplaceUnsignedOverflow(unsigned));
            NativeIntegerConverted {
                value: if matches!(error.kind(), std::num::IntErrorKind::PosOverflow) {
                    u64::MAX
                } else {
                    0
                },
                event: Some(NativeIntegerEvent::Overflow(overflow(&integer, Known(8)))),
            }
        }
    }
}

/// Convert the raw native binary representation. Scalar width/string-accessor
/// failures retain the original expect panics rather than wire validation.
pub fn native_json_to_int(
    type_code: u8,
    bytes: &[u8],
    unsigned: bool,
    target: NativeTypeNameCode,
    flags: u16,
) -> NativeIntegerConverted<i64> {
    if matches!(type_code, 0x01 | 0x03 | 0x0d | 0x0e | 0x0f | 0x10 | 0x11) {
        return NativeIntegerConverted::truncated(0);
    }
    match type_code {
        0x04 => match bytes.first().copied() {
            Some(2) => NativeIntegerConverted::exact(0),
            Some(0) | None => NativeIntegerConverted::truncated(0),
            Some(_) => NativeIntegerConverted::exact(1),
        },
        0x09 => {
            let value = <&[u8; 8]>::try_from(bytes)
                .ok()
                .map(|bytes| i64::from_le_bytes(*bytes))
                .expect("validated binary JSON integer");
            if unsigned {
                let converted = converted_result(native_convert_int_to_uint(
                    flags,
                    value,
                    native_integer_unsigned_upper_bound(target),
                    target,
                ));
                NativeIntegerConverted {
                    value: converted.value as i64,
                    event: converted.event,
                }
            } else {
                converted_result(native_convert_int_to_int(
                    value,
                    native_integer_signed_lower_bound(target),
                    native_integer_signed_upper_bound(target),
                    target,
                ))
            }
        }
        0x0a => {
            let value = <&[u8; 8]>::try_from(bytes)
                .ok()
                .map(|bytes| u64::from_le_bytes(*bytes))
                .expect("validated binary JSON integer");
            if unsigned {
                let converted = converted_result(native_convert_uint_to_uint(
                    value,
                    native_integer_unsigned_upper_bound(target),
                    target,
                ));
                NativeIntegerConverted {
                    value: converted.value as i64,
                    event: converted.event,
                }
            } else {
                converted_result(native_convert_uint_to_int(
                    value,
                    native_integer_signed_upper_bound(target),
                    target,
                ))
            }
        }
        0x0b => {
            let value = <&[u8; 8]>::try_from(bytes)
                .ok()
                .map(|bytes| f64::from_bits(u64::from_le_bytes(*bytes)))
                .expect("validated binary JSON float");
            if unsigned {
                let converted = converted_result(native_convert_float_to_uint(
                    flags,
                    value,
                    native_integer_unsigned_upper_bound(target),
                    target,
                ));
                NativeIntegerConverted {
                    value: converted.value as i64,
                    event: converted.event,
                }
            } else {
                converted_result(native_convert_float_to_int(
                    value,
                    native_integer_signed_lower_bound(target),
                    native_integer_signed_upper_bound(target),
                    target,
                ))
            }
        }
        0x0c => {
            let text = std::str::from_utf8(
                native_binary_json_string_bytes(type_code, bytes)
                    .expect("validated binary JSON string"),
            )
            .unwrap_or("");
            if text.len() > 1 && text.starts_with('-') {
                native_str_to_int(text, false)
            } else {
                let converted = native_str_to_uint(text, false);
                NativeIntegerConverted {
                    value: converted.value as i64,
                    event: converted.event,
                }
            }
        }
        _ => NativeIntegerConverted::truncated(0),
    }
}
pub fn native_json_to_int64(
    type_code: u8,
    value: &[u8],
    unsigned: bool,
    flags: u16,
) -> NativeIntegerConverted<i64> {
    native_json_to_int(type_code, value, unsigned, Known(8), flags)
}

#[cfg(test)]
mod numeric_helper_tests {
    use super::*;
    #[test]
    fn native_numeric_helpers_integer_keep_best_effort_error_precedence_and_original_names() {
        use NativeStringToIntError::{BadNumber, Truncated};
        assert_eq!(Truncated.to_string(), "truncated");
        assert_eq!(BadNumber.to_string(), "bad number");
        assert_eq!(format!("{Truncated:?}"), "Truncated");
        assert_eq!(format!("{BadNumber:?}"), "BadNumber");
        for (text, expected) in [
            ("\u{2003}+12\u{2003}", 12),
            ("-0", 0),
            ("9223372036854775807", i64::MAX),
            ("-9223372036854775808", i64::MIN),
        ] {
            assert_eq!(native_string_to_int(text), Ok(expected));
        }
        assert_eq!(
            native_string_to_int(&format!("{}1", "0".repeat(100))),
            Ok(1)
        );
        for (text, value, error) in [
            ("", 0, Truncated),
            (" \t", 0, Truncated),
            ("+", 0, Truncated),
            ("-", 0, Truncated),
            ("１２", 0, Truncated),
            ("1e2", 1, Truncated),
            (".5", 0, Truncated),
            ("12\0tail", 12, Truncated),
            ("1 2", 1, Truncated),
            ("9223372036854775807x", i64::MAX, Truncated),
            ("-9223372036854775808x", i64::MIN, Truncated),
            ("9223372036854775808x", i64::MAX, BadNumber),
            ("-9223372036854775809x", i64::MIN, BadNumber),
            ("18446744073709551615", i64::MAX, BadNumber),
            ("18446744073709551616x", i64::MAX, BadNumber),
            ("-18446744073709551616x", i64::MIN, BadNumber),
            ("x18446744073709551616", 0, Truncated),
        ] {
            assert_eq!(native_string_to_int(text), Err((value, error)), "{text:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn trace(diagnostic: NativeIntegerDiagnostic<'_>, out: &mut Vec<(&'static str, String)>) {
        match diagnostic {
            NativeIntegerDiagnostic::TruncatedNumericInput(value) => {
                out.push(("truncated", value.into()))
            }
            NativeIntegerDiagnostic::ParsedInteger(NativeIntegerEvent::Overflow(
                NativeIntegerError::Overflow { value, .. },
            )) => out.push(("parsed", value.clone())),
            NativeIntegerDiagnostic::ParsedInteger(_) => {
                panic!("only original overflow sites report parsed_integer")
            }
            NativeIntegerDiagnostic::ErrorOverflow(value) => out.push(("error", value.into())),
            NativeIntegerDiagnostic::ReplaceUnsignedOverflow(value) => {
                out.push(("replace unsigned", value.into()))
            }
        }
    }
    #[test]
    fn native_integer_text_preserves_values_overflow_payloads_and_ordered_effects() {
        assert_eq!(native_round_integer_string(b'5', "-99"), "-100");
        assert_eq!(
            native_float_string_to_integer_string("-0.5", "-0.5"),
            Ok("-1".into())
        );
        assert_eq!(
            native_float_string_to_integer_string("1.5e2", "1.5e2"),
            Ok("150".into())
        );
        assert_eq!(
            native_str_to_int(" 1.5 ", false),
            NativeIntegerConverted::exact(2)
        );
        assert_eq!(
            native_str_to_int("1.5", true),
            NativeIntegerConverted::truncated(1)
        );
        assert_eq!(
            native_str_to_uint("-0.1", true),
            NativeIntegerConverted::exact(0)
        );
        assert_eq!(
            native_str_to_int("9223372036854775808", false),
            NativeIntegerConverted {
                value: i64::MAX,
                event: Some(NativeIntegerEvent::Overflow(NativeIntegerError::Overflow {
                    value: "9223372036854775808".into(),
                    target: Known(8)
                }))
            }
        );
        assert_eq!(
            native_valid_integer_prefix("123..34", false, false),
            Err((
                "123.".into(),
                NativeIntegerError::InvalidUnsignedInteger("123..34".into())
            ))
        );
        assert_eq!(
            native_valid_integer_prefix("123..34", false, true),
            Ok(NativeIntegerConverted::truncated("123".into()))
        );
        let mut effects = Vec::new();
        let converted =
            native_str_to_int_reported("1.2x", false, false, |d| trace(d, &mut effects));
        assert_eq!(
            converted,
            NativeIntegerConverted {
                value: 0,
                event: Some(NativeIntegerEvent::Overflow(NativeIntegerError::Overflow {
                    value: "1.2".into(),
                    target: Known(8)
                }))
            }
        );
        assert_eq!(
            effects,
            vec![("truncated", "1.2x".into()), ("parsed", "1.2".into())]
        );
        effects.clear();
        let converted =
            native_str_to_uint_reported("-1e30x", false, true, |d| trace(d, &mut effects));
        assert_eq!(
            converted,
            NativeIntegerConverted {
                value: 0,
                event: Some(NativeIntegerEvent::Overflow(NativeIntegerError::Overflow {
                    value: "-9223372036854775808".into(),
                    target: Known(8)
                }))
            }
        );
        assert_eq!(
            effects,
            vec![
                ("truncated", "-1e30x".into()),
                ("error", "-1e30x".into()),
                ("replace unsigned", "-9223372036854775808".into())
            ]
        );
        effects.clear();
        assert_eq!(
            native_str_to_int_reported("1.5", true, true, |d| trace(d, &mut effects)),
            NativeIntegerConverted::truncated(1)
        );
        assert!(effects.is_empty()); // function-prefix truncation is not a float-prefix diagnostic
        // In debug, the original i128 addition panics; the prefix diagnostic
        // must already have run, not remain in a deferred trace/result.
        if cfg!(debug_assertions) {
            effects.clear();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                native_str_to_int_reported(
                    "1e170141183460469231731687303715884105727x",
                    false,
                    true,
                    |d| trace(d, &mut effects),
                )
            }));
            assert!(result.is_err());
            assert_eq!(
                effects,
                vec![(
                    "truncated",
                    "1e170141183460469231731687303715884105727x".into()
                )]
            );
        }
    }
    #[test]
    fn native_json_integer_preserves_raw_strings_bounds_flags_and_panic_classes() {
        assert_eq!(native_integer_unsigned_upper_bound(Known(16)), u64::MAX);
        assert_eq!(native_integer_signed_lower_bound(Known(247)), 0);
        assert_eq!(native_integer_signed_upper_bound(Known(9)), 8_388_607);
        assert!(
            std::panic::catch_unwind(|| native_integer_signed_upper_bound(
                NativeTypeNameCode::Unknown(8)
            ))
            .is_err()
        );
        assert_eq!(
            native_convert_float_to_int(2.5, i64::MIN, i64::MAX, Known(8)),
            Ok(2)
        );
        assert_eq!(
            native_convert_float_to_int(i64::MAX as f64, i64::MIN, i64::MAX, Known(8)),
            Ok(i64::MAX)
        );
        assert_eq!(
            native_convert_float_to_int(f64::NAN, i64::MIN, i64::MAX, Known(8)),
            Ok(0)
        );
        assert!(
            std::panic::catch_unwind(|| native_convert_float_to_uint(
                0,
                f64::NAN,
                u64::MAX,
                Known(8)
            ))
            .is_err()
        );
        assert_eq!(
            native_convert_int_to_uint(4, -1, 255, Known(1)),
            Err((
                255,
                NativeIntegerError::Overflow {
                    value: "-1".into(),
                    target: Known(1)
                }
            ))
        );
        assert_eq!(
            native_convert_float_to_uint(4, -1.0, 255, Known(1)),
            Err((
                u64::MAX,
                NativeIntegerError::Overflow {
                    value: "-1".into(),
                    target: Known(1)
                }
            ))
        );
        assert_eq!(
            native_json_to_int(12, b"\x1418446744073709551615", false, Known(1), 0),
            NativeIntegerConverted::exact(-1)
        );
        assert_eq!(
            native_json_to_int(12, b"\x02-1", true, Known(1), 0),
            NativeIntegerConverted::exact(-1)
        );
        assert_eq!(
            native_json_to_int(12, b"\x03 -1", false, Known(1), 0),
            NativeIntegerConverted {
                value: 0,
                event: Some(NativeIntegerEvent::Overflow(NativeIntegerError::Overflow {
                    value: "-1".into(),
                    target: Known(8)
                }))
            }
        );
        assert_eq!(
            native_json_to_int64(12, b"\x01\xff", false, 0),
            NativeIntegerConverted::truncated(0)
        );
        assert_eq!(
            native_json_to_int(4, &[255], false, NativeTypeNameCode::Unknown(8), 0),
            NativeIntegerConverted::exact(1)
        );
        assert_eq!(
            native_json_to_int64(4, &[], false, 0),
            NativeIntegerConverted::truncated(0)
        );
        assert_eq!(
            native_json_to_int64(3, &[255], false, 0),
            NativeIntegerConverted::truncated(0)
        );
        assert_eq!(
            native_json_to_int64(10, &[255; 8], false, 0),
            NativeIntegerConverted {
                value: i64::MAX,
                event: Some(NativeIntegerEvent::Overflow(NativeIntegerError::Overflow {
                    value: "18446744073709551615".into(),
                    target: Known(8)
                }))
            }
        );
        for (tag, bytes, message) in [
            (9, &[][..], "validated binary JSON integer"),
            (11, &[][..], "validated binary JSON float"),
            (12, &[][..], "validated binary JSON string"),
        ] {
            let panic = std::panic::catch_unwind(|| native_json_to_int64(tag, bytes, false, 0))
                .unwrap_err();
            let actual = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied());
            assert_eq!(actual, Some(message));
        }
        let nan = [0, 0, 0, 0, 0, 0, 248, 127];
        assert_eq!(
            native_json_to_int64(11, &nan, false, 0),
            NativeIntegerConverted::exact(0)
        );
        assert!(std::panic::catch_unwind(|| native_json_to_int64(11, &nan, true, 0)).is_err());
    }
}
