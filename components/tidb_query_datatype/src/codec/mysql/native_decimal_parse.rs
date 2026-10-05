// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native digit-string decimal construction, parse disposition and bounded
//! shift policy. This is not the fixed-word MyDecimal or the wire parser.

use smallvec::SmallVec;

use super::{decimal::Decimal, native_decimal_codec::DecimalCodecWarning};

const DIGITS_PER_WORD: usize = 9;

/// Source MyDecimal.FromString's single non-fatal/fatal disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecimalParseError {
    /// A valid numeric prefix was accepted and trailing or excess digits lost.
    Truncated,
    /// The fixed MyDecimal integer buffer could not hold the result.
    Overflow,
    /// Integer exponent parsing exceeded its representable range.
    BadNumber,
    /// No decimal digits were present.
    TruncatedWrongValue,
}

/// Borrowed storage, not an admission or normalization boundary. In particular,
/// a zero shift must copy even a noncanonical coefficient without inspecting
/// it.
#[derive(Clone, Copy, Debug)]
pub struct NativeDecimalParseRef<'a> {
    pub negative: bool,
    pub digits: &'a [u8],
    pub scale: u32,
    pub storage_scale: u32,
    pub declared_shape: Option<(i64, i64)>,
}

/// Owned coefficient transport with the native inline width and exact metadata.
/// No SQL text or fixed-word projection is involved in crossing the boundary.
#[derive(Clone, Debug)]
pub struct NativeDecimalParseValue {
    negative: bool,
    digits: SmallVec<[u8; 24]>,
    scale: u32,
    storage_scale: u32,
    declared_shape: Option<(i64, i64)>,
}

impl NativeDecimalParseValue {
    /// Moves the coefficient allocation (or inline bytes) to its native facade.
    pub fn into_raw_parts(self) -> (bool, SmallVec<[u8; 24]>, u32, u32, Option<(i64, i64)>) {
        (
            self.negative,
            self.digits,
            self.scale,
            self.storage_scale,
            self.declared_shape,
        )
    }

    fn as_ref(&self) -> NativeDecimalParseRef<'_> {
        NativeDecimalParseRef {
            negative: self.negative,
            digits: &self.digits,
            scale: self.scale,
            storage_scale: self.storage_scale,
            declared_shape: self.declared_shape,
        }
    }

    /// Exact unsigned coefficient construction using the native inline width.
    pub fn coefficient_from_unsigned(mut value: u128) -> SmallVec<[u8; 24]> {
        let mut digits = SmallVec::<[u8; 24]>::new();
        if value == 0 {
            digits.push(b'0');
        } else {
            while value != 0 {
                digits.push(b'0' + (value % 10) as u8);
                value /= 10;
            }
            digits.reverse();
        }
        digits
    }

    /// Native signed-integer constructor, shared by parsing and its facade.
    pub fn from_int(value: i64) -> Self {
        native_decimal_normalize(
            value < 0,
            Self::coefficient_from_unsigned(u128::from(value.unsigned_abs())),
            0,
            0,
            false,
        )
    }

    /// Native unsigned-integer constructor without a signed intermediate.
    pub fn from_uint(value: u64) -> Self {
        native_decimal_normalize(
            false,
            Self::coefficient_from_unsigned(u128::from(value)),
            0,
            0,
            false,
        )
    }

    /// Native saturating magnitude constructor, including zero precision.
    pub fn max_or_min(negative: bool, precision: u32, frac: u32) -> Self {
        if precision == 0 {
            return Self::from_int(0);
        }
        native_decimal_normalize(
            negative,
            digits_from_string("9".repeat(precision as usize)),
            frac,
            frac,
            false,
        )
    }

    fn round_to_scale(&self, target_scale: i32) -> Self {
        // Reuse the existing shared rounder and its exact coefficient extractor;
        // never substitute visible text or the nine-word MyDecimal carrier.
        let result_scale = target_scale.max(0) as u32;
        Decimal::try_from_native_digits(
            self.negative,
            digit_str(&self.digits).as_bytes(),
            self.storage_scale,
            self.scale,
            usize::MAX,
        )
        .and_then(|value| {
            value.try_native_round_with_storage(target_scale, true, result_scale, usize::MAX)
        })
        .and_then(|value| {
            let digits = value.native_canonical_coefficient_digits(usize::MAX)?;
            let parts = value.words();
            Ok(Self {
                negative: parts.negative,
                digits: SmallVec::from_vec(digits),
                scale: parts.result_frac,
                storage_scale: parts.storage_frac,
                declared_shape: None,
            })
        })
        .expect("shared native decimal rounding failed")
    }
}

impl NativeDecimalParseRef<'_> {
    fn copy_raw(self) -> NativeDecimalParseValue {
        NativeDecimalParseValue {
            negative: self.negative,
            digits: SmallVec::from_slice(self.digits),
            scale: self.scale,
            storage_scale: self.storage_scale,
            declared_shape: self.declared_shape,
        }
    }
}

fn digit_str(digits: &[u8]) -> &str {
    std::str::from_utf8(digits).expect("decimal coefficients are ASCII digits")
}

fn digits_from_string(digits: String) -> SmallVec<[u8; 24]> {
    debug_assert!(digits.bytes().all(|digit| digit.is_ascii_digit()));
    SmallVec::from_vec(digits.into_bytes())
}

fn digits_to_words(digits: usize) -> usize {
    digits.div_ceil(DIGITS_PER_WORD)
}

/// The single native constructor normalization policy. The coefficient is
/// already owned; this does not impose new ASCII validation on raw storage.
/// Fresh results clear declared shape; the optional zero sign is preserved only
/// for the existing sign-preserving constructor.
pub fn native_decimal_normalize(
    negative: bool,
    mut digits: SmallVec<[u8; 24]>,
    scale: u32,
    storage_scale: u32,
    preserve_zero_sign: bool,
) -> NativeDecimalParseValue {
    debug_assert!(storage_scale >= scale);
    while (digit_str(&digits).len() as u32) < storage_scale {
        digits.insert(0, b'0');
    }
    let min_len = storage_scale.max(1) as usize;
    while digit_str(&digits).len() > min_len && digit_str(&digits).as_bytes()[0] == b'0' {
        digits.remove(0);
    }
    let is_zero = digit_str(&digits).bytes().all(|b| b == b'0');
    NativeDecimalParseValue {
        negative: negative && (preserve_zero_sign || !is_zero),
        digits,
        scale,
        storage_scale,
        declared_shape: None,
    }
}

/// Native canonical-literal constructor, including its original representation
/// preconditions. Do not alias this to the stricter wire canonical parser.
pub fn native_decimal_from_literal(text: &str) -> NativeDecimalParseValue {
    let (negative, magnitude) = match text.strip_prefix('-') {
        Some(magnitude) => (true, magnitude),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (int_part, frac_part) = magnitude.split_once('.').unwrap_or((magnitude, ""));
    let int_stripped = int_part.trim_start_matches('0');
    let int_norm = if int_stripped.is_empty() {
        "0"
    } else {
        int_stripped
    };
    let scale = frac_part.len() as u32;
    native_decimal_normalize(
        negative,
        digits_from_string(format!("{int_norm}{frac_part}")),
        scale,
        scale,
        false,
    )
}

/// Source native digit-string parser with an explicit word limit. Counts the
/// original leading integer zeroes before applying the fixed-word disposition.
pub fn native_decimal_parse_mysql(
    text: &str,
    word_limit: usize,
) -> (NativeDecimalParseValue, Option<DecimalParseError>) {
    let input = text.trim_start_matches([' ', '\t']);
    if input.is_empty() {
        return (
            NativeDecimalParseValue::from_int(0),
            Some(DecimalParseError::TruncatedWrongValue),
        );
    }
    let bytes = input.as_bytes();
    let (negative, start) = match bytes[0] {
        b'-' => (true, 1),
        b'+' => (false, 1),
        _ => (false, 0),
    };
    let mut cursor = start;
    while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
        cursor += 1;
    }
    let integer_end = cursor;
    let mut end = cursor;
    if cursor < bytes.len() && bytes[cursor] == b'.' {
        end += 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
    }
    let integer_digits = integer_end - start;
    let fraction_start = if integer_end < end {
        integer_end + 1
    } else {
        end
    };
    let fraction_digits = end - fraction_start;
    if integer_digits + fraction_digits == 0 {
        return (
            NativeDecimalParseValue::from_int(0),
            Some(DecimalParseError::TruncatedWrongValue),
        );
    }
    let words_int = digits_to_words(integer_digits);
    let words_frac = digits_to_words(fraction_digits);
    let mut disposition = None;
    let (kept_integer_digits, kept_fraction_digits) = if words_int + words_frac <= word_limit {
        (integer_digits, fraction_digits)
    } else if words_int > word_limit {
        disposition = Some(DecimalParseError::Overflow);
        (word_limit * DIGITS_PER_WORD, 0)
    } else {
        disposition = Some(DecimalParseError::Truncated);
        (integer_digits, (word_limit - words_int) * DIGITS_PER_WORD)
    };
    let int_begin = integer_end.saturating_sub(kept_integer_digits);
    let integer = &input[int_begin..integer_end];
    let fraction_end = (fraction_start + kept_fraction_digits).min(end);
    let fraction = &input[fraction_start..fraction_end];
    let magnitude = if fraction.is_empty() {
        if integer.is_empty() {
            "0".to_owned()
        } else {
            integer.to_owned()
        }
    } else {
        format!(
            "{}.{fraction}",
            if integer.is_empty() { "0" } else { integer }
        )
    };
    let signed_magnitude = if negative {
        format!("-{magnitude}")
    } else {
        magnitude
    };
    let mut value = native_decimal_from_literal(&signed_magnitude);
    if end < input.len() && matches!(bytes[end], b'e' | b'E') {
        let (exponent, exponent_error) = parse_mysql_exponent(&input[end + 1..]);
        match exponent_error {
            Some(DecimalParseError::BadNumber) => {
                // Preserve error precedence: exponent bounds below can replace
                // BadNumber after the parsed value was zeroed.
                value = NativeDecimalParseValue::from_int(0);
                disposition = Some(DecimalParseError::BadNumber);
            }
            Some(DecimalParseError::Truncated) => disposition = Some(DecimalParseError::Truncated),
            _ => {}
        }
        if exponent > i64::from(i32::MAX) / 2 {
            let max = NativeDecimalParseValue::max_or_min(
                negative,
                (word_limit * DIGITS_PER_WORD) as u32,
                0,
            );
            return (max, Some(DecimalParseError::Overflow));
        }
        if exponent < i64::from(i32::MIN) / 2 {
            return (
                NativeDecimalParseValue::from_int(0),
                Some(DecimalParseError::Truncated),
            );
        }
        let (shifted, shift_warning) =
            native_decimal_shift_mysql(value.as_ref(), exponent as i32, word_limit);
        value = shifted;
        if let Some(warning) = shift_warning {
            disposition = Some(match warning {
                DecimalCodecWarning::Truncated => DecimalParseError::Truncated,
                DecimalCodecWarning::Overflow => DecimalParseError::Overflow,
            });
            if warning == DecimalCodecWarning::Overflow {
                value = NativeDecimalParseValue::max_or_min(
                    negative,
                    (word_limit * DIGITS_PER_WORD) as u32,
                    0,
                );
            }
        }
    } else if !input[end..].trim().is_empty() {
        disposition = Some(DecimalParseError::Truncated);
    }
    (value, disposition)
}

/// Native Shift's digit-string policy. Zero shifts and integer overflow return
/// an exact copy of the input, including declared shape and noncanonical bytes.
pub fn native_decimal_shift_mysql(
    value: NativeDecimalParseRef<'_>,
    shift: i32,
    word_limit: usize,
) -> (NativeDecimalParseValue, Option<DecimalCodecWarning>) {
    if shift == 0 {
        return (value.copy_raw(), None);
    }
    if digit_str(value.digits).bytes().all(|b| b == b'0') {
        return (NativeDecimalParseValue::from_int(0), None);
    }
    let mut digits = SmallVec::<[u8; 24]>::from_slice(value.digits);
    let mut scale = i64::from(value.storage_scale) - i64::from(shift);
    if scale < 0 {
        digits.extend_from_slice("0".repeat((-scale) as usize).as_bytes());
        scale = 0;
    }
    while scale > 0 && digit_str(&digits).ends_with('0') {
        digits.pop();
        scale -= 1;
    }
    while digits.len() < scale as usize {
        digits.insert(0, b'0');
    }
    let exact = native_decimal_normalize(value.negative, digits, scale as u32, scale as u32, false);
    let split = exact.digits.len() - exact.storage_scale as usize;
    let integer_digits = digit_str(&exact.digits)[..split]
        .trim_start_matches('0')
        .len();
    let words_int = digits_to_words(integer_digits);
    if words_int > word_limit {
        return (value.copy_raw(), Some(DecimalCodecWarning::Overflow));
    }
    let words_frac = digits_to_words(exact.storage_scale as usize);
    if words_int + words_frac <= word_limit {
        return (exact, None);
    }
    let kept_scale = ((word_limit - words_int) * DIGITS_PER_WORD) as i32;
    let rounded = exact.round_to_scale(kept_scale);
    // Source checks pre-round bounds: a carry must not resurrect a value all
    // of whose source digits lay below the retained fractional boundary.
    let discarded_digits = exact.storage_scale.saturating_sub(kept_scale as u32) as usize;
    let retained_len = exact.digits.len().saturating_sub(discarded_digits);
    if digit_str(&exact.digits)[..retained_len]
        .bytes()
        .all(|digit| digit == b'0')
    {
        return (
            NativeDecimalParseValue::from_int(0),
            Some(DecimalCodecWarning::Truncated),
        );
    }
    if digit_str(&rounded.digits).bytes().all(|b| b == b'0') {
        return (
            NativeDecimalParseValue::from_int(0),
            Some(DecimalCodecWarning::Truncated),
        );
    }
    let rounded_split = rounded.digits.len() - rounded.storage_scale as usize;
    let rounded_integer_digits = digit_str(&rounded.digits)[..rounded_split]
        .trim_start_matches('0')
        .len();
    if digits_to_words(rounded_integer_digits) > word_limit {
        return (value.copy_raw(), Some(DecimalCodecWarning::Overflow));
    }
    (rounded, Some(DecimalCodecWarning::Truncated))
}

fn parse_mysql_exponent(text: &str) -> (i64, Option<DecimalParseError>) {
    let text = text.trim();
    if text.is_empty() {
        return (0, Some(DecimalParseError::Truncated));
    }
    let bytes = text.as_bytes();
    let (negative, mut index) = match bytes[0] {
        b'-' => (true, 1),
        b'+' => (false, 1),
        _ => (false, 0),
    };
    let mut magnitude = 0_u64;
    let mut has_digit = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if !byte.is_ascii_digit() {
            let bounded = magnitude.min(i64::MAX as u64) as i64;
            return (
                if negative { -bounded } else { bounded },
                Some(DecimalParseError::Truncated),
            );
        }
        has_digit = true;
        let Some(next) = magnitude
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(byte - b'0')))
        else {
            return (0, Some(DecimalParseError::BadNumber));
        };
        magnitude = next;
        index += 1;
    }
    if !has_digit {
        return (0, Some(DecimalParseError::Truncated));
    }
    let limit = i64::MAX as u64 + u64::from(negative);
    if magnitude > limit {
        return (
            if negative { i64::MIN } else { i64::MAX },
            Some(DecimalParseError::BadNumber),
        );
    }
    (
        if negative {
            (0_u64.wrapping_sub(magnitude)) as i64
        } else {
            magnitude as i64
        },
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_decimal_parse_keeps_normalization_word_limits_and_status_precedence() {
        use DecimalParseError::{BadNumber, Overflow, Truncated, TruncatedWrongValue};
        // Source-derived coefficient/scale/disposition expectations, not another
        // provider's rendering or a parse of the result under test.
        for (input, words, expected, scale, negative, error) in [
            ("-0.00", 9, "00", 2, false, None),
            ("\n1", 9, "0", 0, false, Some(TruncatedWrongValue)),
            ("  \t-.50x", 9, "50", 2, true, Some(Truncated)),
            ("1\u{00a0}", 9, "1", 0, false, None),
            ("1e\u{00a0}5", 9, "100000", 0, false, None),
            ("1234567890", 1, "234567890", 0, false, Some(Overflow)),
            ("1234567890x", 1, "234567890", 0, false, Some(Truncated)),
            ("1.25", 1, "1", 0, false, Some(Truncated)),
            (".1234567891", 1, "123456789", 9, false, Some(Truncated)),
            ("1e18446744073709551616", 1, "0", 0, false, Some(BadNumber)),
            (
                "1e9223372036854775808",
                1,
                "999999999",
                0,
                false,
                Some(Overflow),
            ),
            ("1e-9223372036854775809", 1, "0", 0, false, Some(Truncated)),
            ("1e", 1, "1", 0, false, Some(Truncated)),
            ("1e-9", 1, "000000001", 9, false, None),
        ] {
            let (value, actual_error) = native_decimal_parse_mysql(input, words);
            assert_eq!(actual_error, error, "{input}");
            assert_eq!(value.negative, negative, "{input}");
            assert_eq!(value.digits.as_slice(), expected.as_bytes(), "{input}");
            assert_eq!(
                (value.scale, value.storage_scale),
                (scale, scale),
                "{input}"
            );
            assert_eq!(value.declared_shape, None, "{input}");
        }
        let canonical = native_decimal_from_literal("-0001.2500");
        assert!(canonical.negative);
        assert_eq!(canonical.digits.as_slice(), b"12500");
        assert_eq!((canonical.scale, canonical.storage_scale), (4, 4));
        let signed_zero = native_decimal_normalize(true, SmallVec::from_slice(b"000"), 1, 2, true);
        assert!(signed_zero.negative);
        assert_eq!(signed_zero.digits.as_slice(), b"00");
        assert_eq!((signed_zero.scale, signed_zero.storage_scale), (1, 2));
        let empty = native_decimal_normalize(true, SmallVec::new(), 0, 0, false);
        assert!(empty.digits.is_empty());
        assert!(!empty.negative);
        assert!(
            std::panic::catch_unwind(|| {
                native_decimal_normalize(false, SmallVec::from_slice(&[0xff]), 0, 2, false)
            })
            .is_err()
        );
        let unsigned = NativeDecimalParseValue::from_uint(u64::MAX);
        assert_eq!(unsigned.digits.as_slice(), b"18446744073709551615");
        assert!(!unsigned.digits.spilled());
        let signed = NativeDecimalParseValue::from_int(i64::MIN);
        assert!(signed.negative);
        assert_eq!(signed.digits.as_slice(), b"9223372036854775808");
        let wide = NativeDecimalParseValue::coefficient_from_unsigned(u128::MAX);
        assert_eq!(wide.as_slice(), b"340282366920938463463374607431768211455");
        assert!(wide.spilled());
        let max = NativeDecimalParseValue::max_or_min(true, 5, 2);
        assert!(max.negative);
        assert_eq!(max.digits.as_slice(), b"99999");
        assert_eq!((max.scale, max.storage_scale), (2, 2));
    }

    #[test]
    fn native_decimal_shift_keeps_raw_identity_and_source_rounding_edges() {
        for digits in [&[0xff, b'-', b'0'][..], &b""[..]] {
            let raw = NativeDecimalParseRef {
                negative: true,
                digits,
                scale: 7,
                storage_scale: 2,
                declared_shape: Some((20, 7)),
            };
            let (value, warning) = native_decimal_shift_mysql(raw, 0, 0);
            assert_eq!(warning, None);
            assert_eq!(
                value.into_raw_parts(),
                (true, SmallVec::from_slice(digits), 7, 2, Some((20, 7)))
            );
        }
        let raw = NativeDecimalParseRef {
            negative: true,
            digits: b"00001234567891234",
            scale: 2,
            storage_scale: 4,
            declared_shape: Some((20, 2)),
        };
        let (overflow, warning) = native_decimal_shift_mysql(raw, 1, 1);
        assert_eq!(warning, Some(DecimalCodecWarning::Overflow));
        assert_eq!(
            overflow.into_raw_parts(),
            (true, SmallVec::from_slice(raw.digits), 2, 4, Some((20, 2)))
        );
        let hidden = NativeDecimalParseRef {
            negative: false,
            digits: b"12500",
            scale: 1,
            storage_scale: 4,
            declared_shape: Some((10, 1)),
        };
        let (shifted, warning) = native_decimal_shift_mysql(hidden, 1, 9);
        assert_eq!(warning, None);
        assert_eq!(
            shifted.into_raw_parts(),
            (false, SmallVec::from_slice(b"125"), 1, 1, None)
        );
        let small = native_decimal_from_literal("0.000000005");
        let (discarded, warning) = native_decimal_shift_mysql(small.as_ref(), -1, 1);
        assert_eq!(warning, Some(DecimalCodecWarning::Truncated));
        assert_eq!(
            discarded.into_raw_parts(),
            (false, SmallVec::from_slice(b"0"), 0, 0, None)
        );
        let fraction = native_decimal_from_literal("1.234567895");
        let (rounded, warning) = native_decimal_shift_mysql(fraction.as_ref(), -1, 1);
        assert_eq!(warning, Some(DecimalCodecWarning::Truncated));
        assert_eq!(
            rounded.into_raw_parts(),
            (false, SmallVec::from_slice(b"123456790"), 9, 9, None)
        );
    }
}
