// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native expression-only Decimal arithmetic from TiDB364aef2's decimal/mod.rs.
//! Fixed storage codecs stay in their existing crates. PB kernels are
//! unchanged.
use std::cmp::Ordering;

use super::ExpressionDecimal as Decimal;

const DIGITS_PER_WORD: usize = 9;
const CODEC_WORD_BUF_LEN: usize = 9;
const CODEC_MAX_DECIMAL_SCALE: i32 = 30;
const CODEC_POWERS10: [i32; 10] = [
    1, 10, 100, 1000, 10000, 100000, 1000000, 10000000, 100000000, 1000000000,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecimalCodecWarning {
    Overflow,
    Truncated,
}
fn digits_to_words(digits: usize) -> usize {
    digits.div_ceil(9)
}
fn fix_word_cnt_error(
    integer: usize,
    fraction: usize,
) -> (usize, usize, Option<DecimalCodecWarning>) {
    if integer > 9 {
        (9, 0, Some(DecimalCodecWarning::Overflow))
    } else if integer + fraction > 9 {
        (integer, 9 - integer, Some(DecimalCodecWarning::Truncated))
    } else {
        (integer, fraction, None)
    }
}

impl Decimal {
    pub(crate) fn valid(&self) -> bool {
        !self.coefficient.is_empty()
            && self.coefficient.iter().all(u8::is_ascii_digit)
            && self.coefficient.len() >= self.storage_scale as usize
            && self.result_scale <= self.storage_scale
    }
    fn digits(&self) -> &str {
        std::str::from_utf8(&self.coefficient).expect("checked decimal digits")
    }
    pub(crate) fn is_zero(&self) -> bool {
        self.coefficient.iter().all(|d| *d == b'0')
    }
    fn new(negative: bool, digits: String, scale: u32) -> Self {
        Self::new_with_storage(negative, digits, scale, scale)
    }
    fn new_with_storage(negative: bool, digits: String, scale: u32, storage: u32) -> Self {
        let mut value =
            Self::new_with_storage_preserving_zero_sign(negative, digits, scale, storage);
        if value.is_zero() {
            value.negative = false;
        }
        value
    }
    fn new_with_storage_preserving_zero_sign(
        negative: bool,
        digits: String,
        scale: u32,
        storage: u32,
    ) -> Self {
        let digits = digits.trim_start_matches('0');
        let minimum = (storage as usize).max(1);
        let coefficient = format!("{digits:0>minimum$}").into_bytes();
        Self {
            negative,
            coefficient,
            storage_scale: storage,
            result_scale: scale,
            declared_shape: None,
        }
    }
    fn max_or_min(negative: bool, precision: u32, scale: u32) -> Self {
        Self::new(negative, "9".repeat(precision as usize), scale)
    }
    fn negate(&self) -> Self {
        let mut result = self.clone();
        if !result.is_zero() {
            result.negative = !result.negative;
        }
        result
    }
    fn truncate_to_scale(&self, scale: i32) -> Self {
        self.round_or_truncate_to_scale_with_storage(scale, false, scale.max(0) as u32)
    }
    fn round_or_truncate_to_scale_with_storage(
        &self,
        target: i32,
        round: bool,
        storage: u32,
    ) -> Self {
        let scale = target.max(0) as u32;
        let storage = storage.max(scale);
        let shift = i64::from(self.storage_scale) - i64::from(target);
        if shift <= 0 {
            return Self::new_with_storage(
                self.negative,
                pad_scale(self.digits(), self.storage_scale, storage),
                scale,
                storage,
            );
        }
        let minimum = shift as usize + 1;
        let digits = format!("{:0>minimum$}", self.digits());
        let split = digits.len() - shift as usize;
        let mut kept = if round && digits.as_bytes()[split] >= b'5' {
            digit_add(&digits[..split], "1")
        } else {
            digits[..split].to_owned()
        };
        if target < 0 {
            kept.push_str(&"0".repeat((-target) as usize));
        }
        Self::new_with_storage(
            self.negative,
            pad_scale(&kept, scale, storage),
            scale,
            storage,
        )
    }
    pub(crate) fn add(&self, other: &Decimal) -> Decimal {
        let storage_scale = self.storage_scale.max(other.storage_scale);
        let scale = self.result_scale.max(other.result_scale);
        let a = pad_scale(self.digits(), self.storage_scale, storage_scale);
        let b = pad_scale(other.digits(), other.storage_scale, storage_scale);
        if self.negative == other.negative {
            return Decimal::new_with_storage(
                self.negative,
                digit_add(&a, &b),
                scale,
                storage_scale,
            );
        }
        match digit_cmp(&a, &b) {
            Ordering::Equal => {
                Decimal::new_with_storage(false, "0".to_string(), scale, storage_scale)
            }
            Ordering::Greater => {
                Decimal::new_with_storage(self.negative, digit_sub(&a, &b), scale, storage_scale)
            }
            Ordering::Less => {
                Decimal::new_with_storage(other.negative, digit_sub(&b, &a), scale, storage_scale)
            }
        }
    }

    pub(crate) fn add_mysql(&self, other: &Decimal) -> (Decimal, Option<DecimalCodecWarning>) {
        let result = self.add(other);
        // `DecimalAdd` reaches Go's `doAdd` only when both operands have the
        // same sign. Opposite-sign inputs use `doSub` and must be bounded from
        // the actual difference instead of this add-only heuristic.
        if self.negative == other.negative && add_leading_word_overflow(self, other) {
            return (
                Decimal::max_or_min(false, (CODEC_WORD_BUF_LEN * DIGITS_PER_WORD) as u32, 0),
                Some(DecimalCodecWarning::Overflow),
            );
        }
        self.bound_add_sub_result(result)
    }

    pub(crate) fn sub_mysql(&self, other: &Decimal) -> (Decimal, Option<DecimalCodecWarning>) {
        let result = self.add(&other.negate());
        // Go's DecimalSub reaches doAdd only when the operands have opposite
        // signs; in that branch the same leading-word carry heuristic applies
        // to the two magnitudes. Same-sign subtraction uses doSub and is
        // bounded from the actual result below.
        if self.negative != other.negative && add_leading_word_overflow(self, other) {
            return (
                Decimal::max_or_min(false, (CODEC_WORD_BUF_LEN * DIGITS_PER_WORD) as u32, 0),
                Some(DecimalCodecWarning::Overflow),
            );
        }
        self.bound_add_sub_result(result)
    }

    fn bound_add_sub_result(&self, result: Decimal) -> (Decimal, Option<DecimalCodecWarning>) {
        let split = result.coefficient.len() - result.storage_scale as usize;
        let integer_digits = result.digits()[..split].trim_start_matches('0').len();
        let words_int = digits_to_words(integer_digits);
        if words_int > CODEC_WORD_BUF_LEN {
            // `doAdd` calls `maxDecimal` before assigning the result sign.
            return (
                Decimal::max_or_min(false, (CODEC_WORD_BUF_LEN * DIGITS_PER_WORD) as u32, 0),
                Some(DecimalCodecWarning::Overflow),
            );
        }
        let words_frac = digits_to_words(result.storage_scale as usize);
        if words_int + words_frac <= CODEC_WORD_BUF_LEN {
            return (result, None);
        }
        let kept_scale = ((CODEC_WORD_BUF_LEN - words_int) * DIGITS_PER_WORD) as i32;
        (
            result.truncate_to_scale(kept_scale),
            Some(DecimalCodecWarning::Truncated),
        )
    }

    pub(crate) fn mul_mysql(&self, other: &Decimal) -> (Decimal, Option<DecimalCodecWarning>) {
        // The native fast path preserves hidden digits that the word path rounds.
        if let Some(product) = self.try_mul_mysql_fast(other) {
            return (product, None);
        }
        let left = MyDecimalWords::from_decimal(self);
        let right = MyDecimalWords::from_decimal(other);
        let words_int_left = digits_to_words(left.digits_int.max(0) as usize) as i32;
        let mut words_frac_left = digits_to_words(left.digits_frac.max(0) as usize) as i32;
        let mut words_int_right = digits_to_words(right.digits_int.max(0) as usize) as i32;
        let mut words_frac_right = digits_to_words(right.digits_frac.max(0) as usize) as i32;
        let requested_words_int =
            digits_to_words((left.digits_int + right.digits_int).max(0) as usize) as i32;
        let requested_words_frac = words_frac_left + words_frac_right;
        let (words_int, words_frac, warning) =
            fix_word_cnt_error(requested_words_int as usize, requested_words_frac as usize);
        let words_int = words_int as i32;
        let words_frac = words_frac as i32;
        let result_scale =
            (self.result_scale + other.result_scale).min(CODEC_MAX_DECIMAL_SCALE as u32);

        if warning == Some(DecimalCodecWarning::Overflow) {
            // Go assigns `to.negative` before returning ErrOverflow.  The
            // fixed-word receiver therefore retains a signed zero when the
            // operands have opposite signs; preserve that observable
            // `ToString` result instead of normalizing it away.
            return (
                Decimal::new_with_storage_preserving_zero_sign(
                    self.negative != other.negative,
                    "0".to_owned(),
                    result_scale,
                    result_scale,
                ),
                warning,
            );
        }

        let mut tmp_int = requested_words_int;
        let mut tmp_frac = requested_words_frac;
        if warning.is_some() {
            if tmp_int > words_int {
                tmp_int -= words_int;
                tmp_frac = tmp_int >> 1;
                words_int_right -= tmp_int - tmp_frac;
                words_frac_left = 0;
                words_frac_right = 0;
            } else {
                tmp_frac -= words_frac;
                tmp_int = tmp_frac >> 1;
                if words_frac_left <= words_frac_right {
                    words_frac_left -= tmp_int;
                    words_frac_right -= tmp_frac - tmp_int;
                } else {
                    words_frac_right -= tmp_int;
                    words_frac_left -= tmp_frac - tmp_int;
                }
            }
        }

        let mut product = MyDecimalWords {
            negative: left.negative != right.negative,
            digits_int: words_int * DIGITS_PER_WORD as i32,
            digits_frac: (left.digits_frac + right.digits_frac)
                .min(words_frac * DIGITS_PER_WORD as i32),
            word_buf: [0; CODEC_WORD_BUF_LEN],
        };

        let mut start_to = words_int + words_frac - 1;
        let start_right = words_int_right + words_frac_right - 1;
        let mut index_left = words_int_left + words_frac_left - 1;
        while index_left >= 0 {
            let mut carry = 0;
            let mut index_to = start_to;
            let mut index_right = start_right;
            while index_right >= 0 {
                let value = i64::from(left.word_buf[index_left as usize])
                    * i64::from(right.word_buf[index_right as usize]);
                let base = i64::from(CODEC_POWERS10[DIGITS_PER_WORD]);
                let high = (value / base) as i32;
                let low = (value - i64::from(high) * base) as i32;
                (product.word_buf[index_to as usize], carry) =
                    add_two_decimal_words(product.word_buf[index_to as usize], low, carry);
                carry += high;
                index_right -= 1;
                index_to -= 1;
            }
            if carry > 0 {
                if index_to < 0 {
                    return (
                        Decimal::new_with_storage_preserving_zero_sign(
                            self.negative != other.negative,
                            "0".to_owned(),
                            result_scale,
                            result_scale,
                        ),
                        Some(DecimalCodecWarning::Overflow),
                    );
                }
                (product.word_buf[index_to as usize], carry) =
                    add_decimal_words(product.word_buf[index_to as usize], 0, carry);
            }
            index_to -= 1;
            while carry > 0 {
                if index_to < 0 {
                    return (
                        Decimal::new_with_storage_preserving_zero_sign(
                            self.negative != other.negative,
                            "0".to_owned(),
                            result_scale,
                            result_scale,
                        ),
                        Some(DecimalCodecWarning::Overflow),
                    );
                }
                (product.word_buf[index_to as usize], carry) =
                    add_decimal_words(product.word_buf[index_to as usize], 0, carry);
                index_to -= 1;
            }
            start_to -= 1;
            index_left -= 1;
        }

        if product.word_buf[..(words_int + words_frac) as usize]
            .iter()
            .all(|word| *word == 0)
        {
            return (Decimal::new(false, "0".to_owned(), result_scale), warning);
        }

        let value = product.to_decimal();
        let storage_scale = value.storage_scale.max(result_scale);
        (
            value.round_or_truncate_to_scale_with_storage(result_scale as i32, true, storage_scale),
            warning,
        )
    }

    fn try_mul_mysql_fast(&self, other: &Decimal) -> Option<Decimal> {
        let result_scale = self.result_scale.checked_add(other.result_scale)?;
        if result_scale > CODEC_MAX_DECIMAL_SCALE as u32 {
            return None;
        }
        let left = self.digits().parse::<i128>().ok()?;
        let right = other.digits().parse::<i128>().ok()?;
        let product = left.checked_mul(right)?;
        let negative = (product < 0) || (self.negative != other.negative && product != 0);
        let storage_scale = self.storage_scale.checked_add(other.storage_scale)?;
        Some(Decimal::new_with_storage(
            negative,
            product.unsigned_abs().to_string(),
            result_scale,
            storage_scale,
        ))
    }

    pub(crate) fn div_mysql_with_warning(
        &self,
        other: &Decimal,
        frac_increment: u32,
    ) -> Option<(Decimal, Option<DecimalCodecWarning>)> {
        self.div_mysql_unbounded(other, frac_increment)
            .map(bound_decimal_codec_result)
    }

    fn div_mysql_unbounded(&self, other: &Decimal, frac_increment: u32) -> Option<Decimal> {
        if other.is_zero() {
            return None;
        }
        let result_scale = (self.result_scale + frac_increment).min(CODEC_MAX_DECIMAL_SCALE as u32);
        if self.is_zero() {
            return Some(Decimal::new(false, "0".to_owned(), result_scale));
        }
        let frac1 = word_scale(self.storage_scale);
        let frac2 = word_scale(other.storage_scale);
        let padding = (frac1 - self.storage_scale) + (frac2 - other.storage_scale);
        let adjusted_increment = frac_increment.saturating_sub(padding);
        let storage_scale = word_scale(frac1 + frac2 + adjusted_increment);

        let common_scale = self.storage_scale.max(other.storage_scale);
        let numerator = pad_scale(
            &pad_scale(self.digits(), self.storage_scale, common_scale),
            common_scale,
            common_scale + storage_scale,
        );
        let divisor = pad_scale(other.digits(), other.storage_scale, common_scale);
        let (quotient, _) = digit_divmod(&numerator, &divisor);
        Some(Decimal::new_with_storage(
            self.negative != other.negative,
            quotient,
            result_scale,
            storage_scale,
        ))
    }
}

fn bound_decimal_codec_result(value: Decimal) -> (Decimal, Option<DecimalCodecWarning>) {
    if value.is_zero() {
        return (value, None);
    }
    let split = value
        .coefficient
        .len()
        .saturating_sub(value.storage_scale as usize);
    let integer_digits = value.digits()[..split].trim_start_matches('0').len();
    let words_int = digits_to_words(integer_digits);
    let words_frac = digits_to_words(value.storage_scale as usize);
    let (_, fixed_frac, warning) = fix_word_cnt_error(words_int, words_frac);
    match warning {
        None => (value, None),
        Some(DecimalCodecWarning::Overflow) => (
            Decimal::max_or_min(
                value.negative,
                (CODEC_WORD_BUF_LEN * DIGITS_PER_WORD) as u32,
                0,
            ),
            warning,
        ),
        Some(DecimalCodecWarning::Truncated) => {
            let target_storage = (fixed_frac * DIGITS_PER_WORD) as u32;
            let target_storage = target_storage.max(value.result_scale);
            let discarded = value.storage_scale.saturating_sub(target_storage) as usize;
            let digits = if discarded == 0 {
                value.digits().to_owned()
            } else {
                value.digits()[..value.coefficient.len() - discarded].to_owned()
            };
            (
                Decimal::new_with_storage(
                    value.negative,
                    digits,
                    value.result_scale,
                    target_storage,
                ),
                warning,
            )
        }
    }
}

fn pad_scale(digits: &str, scale: u32, target: u32) -> String {
    let mut s = digits.to_string();
    s.push_str(&"0".repeat((target - scale) as usize));
    s
}

fn word_scale(scale: u32) -> u32 {
    scale.div_ceil(9) * 9
}

fn pad_equal(a: &str, b: &str) -> (String, String) {
    let len = a.len().max(b.len());
    (format!("{a:0>len$}"), format!("{b:0>len$}"))
}

fn digit_cmp(a: &str, b: &str) -> Ordering {
    let (a, b) = pad_equal(a, b);
    a.cmp(&b)
}

fn digit_add(a: &str, b: &str) -> String {
    let (a, b) = pad_equal(a, b);
    let mut out = Vec::with_capacity(a.len() + 1);
    let mut carry = 0u8;
    for (x, y) in a.bytes().rev().zip(b.bytes().rev()) {
        let sum = (x - b'0') + (y - b'0') + carry;
        out.push(b'0' + sum % 10);
        carry = sum / 10;
    }
    if carry > 0 {
        out.push(b'0' + carry);
    }
    out.reverse();
    String::from_utf8(out).expect("digits are ASCII")
}

fn digit_sub(a: &str, b: &str) -> String {
    let (a, b) = pad_equal(a, b);
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i8;
    for (x, y) in a.bytes().rev().zip(b.bytes().rev()) {
        let mut diff = (x as i8 - b'0' as i8) - (y as i8 - b'0' as i8) - borrow;
        if diff < 0 {
            diff += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(b'0' + diff as u8);
    }
    out.reverse();
    String::from_utf8(out).expect("digits are ASCII")
}

fn strip_leading_zeros(s: &str) -> String {
    let trimmed = s.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

fn digit_divmod(a: &str, b: &str) -> (String, String) {
    let mut quotient = String::with_capacity(a.len());
    let mut rem = "0".to_string();
    for ch in a.bytes() {
        rem = strip_leading_zeros(&format!("{rem}{}", ch as char));
        let mut count = 0u8;
        while digit_cmp(&rem, b) != Ordering::Less {
            rem = strip_leading_zeros(&digit_sub(&rem, b));
            count += 1;
        }
        quotient.push((b'0' + count) as char);
    }
    (strip_leading_zeros(&quotient), rem)
}

fn add_decimal_words(left: i32, right: i32, carry: i32) -> (i32, i32) {
    let base = CODEC_POWERS10[DIGITS_PER_WORD];
    let sum = left + right + carry;
    if sum >= base {
        (sum - base, 1)
    } else {
        (sum, 0)
    }
}

fn add_two_decimal_words(left: i32, right: i32, carry: i32) -> (i32, i32) {
    let base = i64::from(CODEC_POWERS10[DIGITS_PER_WORD]);
    let mut sum = i64::from(left) + i64::from(right) + i64::from(carry);
    let mut next_carry = 0;
    if sum >= base {
        next_carry = 1;
        sum -= base;
    }
    if sum >= base {
        next_carry += 1;
        sum -= base;
    }
    (sum as i32, next_carry)
}

fn add_leading_word_overflow(left: &Decimal, right: &Decimal) -> bool {
    let left = MyDecimalWords::from_decimal(left);
    let right = MyDecimalWords::from_decimal(right);
    let left_words = digits_to_words(left.digits_int.max(0) as usize);
    let right_words = digits_to_words(right.digits_int.max(0) as usize);
    let leading = if left_words > right_words {
        left.word_buf[0]
    } else if right_words > left_words {
        right.word_buf[0]
    } else {
        left.word_buf[0].saturating_add(right.word_buf[0])
    };
    // `doAdd` increments the destination word count when this leading
    // word can carry.  That is only an overflow once the increment would
    // exceed the fixed nine-word buffer; smaller values (for example,
    // `999999999 + 1`) legitimately grow from one word to two.
    let carry = leading > CODEC_POWERS10[DIGITS_PER_WORD] - 2;
    left_words.max(right_words) + usize::from(carry) > CODEC_WORD_BUF_LEN
}

impl std::fmt::Display for Decimal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.storage_scale > self.result_scale {
            return write!(
                f,
                "{}",
                self.round_or_truncate_to_scale_with_storage(
                    self.result_scale as i32,
                    true,
                    self.result_scale
                )
            );
        }
        let sign = if self.negative { "-" } else { "" };
        let split = self.coefficient.len() - self.storage_scale as usize;
        let integer = if split == 0 {
            "0"
        } else {
            &self.digits()[..split]
        };
        if self.result_scale == 0 {
            write!(f, "{sign}{integer}")
        } else {
            write!(
                f,
                "{sign}{integer}.{}",
                &self.digits()[split..split + self.result_scale as usize]
            )
        }
    }
}

struct MyDecimalWords {
    negative: bool,
    digits_int: i32,
    digits_frac: i32,
    word_buf: [i32; 9],
}
impl MyDecimalWords {
    fn from_decimal(d: &Decimal) -> Self {
        let original_int = d.coefficient.len() - d.storage_scale as usize;
        let (wi, wf, warning) = fix_word_cnt_error(
            digits_to_words(original_int),
            digits_to_words(d.storage_scale as usize),
        );
        let digits_int = if warning == Some(DecimalCodecWarning::Overflow) {
            wi * 9
        } else {
            original_int
        };
        let digits_frac = if warning.is_some() {
            wf * 9
        } else {
            d.storage_scale as usize
        };
        let mut word_buf = [0; 9];
        let integer = &d.digits()[original_int - digits_int..original_int];
        let first = if digits_int % 9 == 0 {
            9
        } else {
            digits_int % 9
        };
        let mut offset = 0;
        for (i, word) in word_buf.iter_mut().take(wi).enumerate() {
            let width = if i == 0 { first } else { 9 };
            *word = integer[offset..offset + width].parse().unwrap();
            offset += width;
        }
        let fraction = &d.digits()[original_int..original_int + digits_frac];
        for (i, part) in fraction.as_bytes().chunks(9).enumerate() {
            let value = std::str::from_utf8(part).unwrap().parse::<i32>().unwrap();
            word_buf[wi + i] = value * CODEC_POWERS10[9 - part.len()];
        }
        Self {
            negative: d.negative,
            digits_int: digits_int as i32,
            digits_frac: digits_frac as i32,
            word_buf,
        }
    }
    fn to_decimal(&self) -> Decimal {
        let integer = self.digits_int.max(0) as usize;
        let fraction = self.digits_frac.max(0) as usize;
        let wi = digits_to_words(integer);
        let mut digits = String::new();
        for word in &self.word_buf[..wi] {
            digits.push_str(&format!("{word:09}"));
        }
        if wi != 0 {
            digits = digits[wi * 9 - integer..].to_owned();
        }
        let mut frac = String::new();
        for word in &self.word_buf[wi..wi + digits_to_words(fraction)] {
            frac.push_str(&format!("{word:09}"));
        }
        digits.push_str(&frac[..fraction]);
        if digits.is_empty() {
            digits.push('0');
        }
        if fraction == 0 {
            Decimal::new(self.negative, digits, 0)
        } else {
            Decimal::new_with_storage_preserving_zero_sign(
                self.negative,
                digits,
                fraction as u32,
                fraction as u32,
            )
        }
    }
}
