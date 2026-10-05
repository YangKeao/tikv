// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native fixed nine-word MyDecimal, independent of the wire Decimal policy.
//! The private representation and all value-plus-error algorithms retain the
//! source layout and mutation behavior. Raw-parts projection is a storage
//! bridge, not validation or normalization.

use smallvec::SmallVec;

use super::native_decimal_codec::{DecimalCodecError, checked_bin_size, write_bin};

pub const MAX_WORD_BUF_LEN: usize = 9;
const DIGITS_PER_WORD: i32 = 9;
const WORD_BASE: u64 = 1_000_000_000;
const DIG_MASK: i32 = 100_000_000;
const POWERS10: [i32; 10] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
];
const WORD_MAX: i32 = (WORD_BASE as i32) - 1;
const FRAC_MAX: [i32; 8] = [
    900_000_000,
    990_000_000,
    999_000_000,
    999_900_000,
    999_990_000,
    999_999_000,
    999_999_900,
    999_999_990,
];

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MyDecimal {
    digits_int: i8,
    digits_frac: i8,
    result_frac: i8,
    negative: bool,
    word_buf: [i32; MAX_WORD_BUF_LEN],
}

pub const MYDECIMAL_STRUCT_SIZE: usize = 40;
const _: () = assert!(std::mem::size_of::<MyDecimal>() == MYDECIMAL_STRUCT_SIZE);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum RoundMode {
    Ceiling = 0,
    HalfUp = 5,
    Truncate = 10,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecimalError {
    Truncated,
    TruncatedWrongValue,
    Overflow,
    BadNumber,
}

/// The source digit-to-word formula, also used by native facade tests.
pub fn digits_to_words(digits: i32) -> i32 {
    (digits + DIGITS_PER_WORD - 1) / DIGITS_PER_WORD
}

fn count_leading_zeroes(mut i: i32, word: i32) -> i32 {
    let mut leading = 0;
    while word < POWERS10[i as usize] {
        i -= 1;
        leading += 1;
    }
    leading
}

fn pow10(exp: i32) -> f64 {
    format!("1e{exp}")
        .parse::<f64>()
        .expect("decimal power literal is parsable")
}

pub fn format_float_g_shortest(f: f64) -> String {
    super::Decimal::native_format_float_g_shortest(f)
}

fn is_space(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

fn count_trailing_zeroes(mut i: i32, word: i32) -> i32 {
    let mut trailing = 0;
    while word % POWERS10[i as usize] == 0 {
        i += 1;
        trailing += 1;
    }
    trailing
}

fn fix_word_cnt_error(words_int: i32, words_frac: i32) -> (i32, i32, Option<DecimalError>) {
    let buf_len = MAX_WORD_BUF_LEN as i32;
    if words_int + words_frac > buf_len {
        if words_int > buf_len {
            return (buf_len, 0, Some(DecimalError::Overflow));
        }
        return (
            words_int,
            buf_len - words_int,
            Some(DecimalError::Truncated),
        );
    }
    (words_int, words_frac, None)
}

fn add_word(a: i32, b: i32, carry: i32) -> (i32, i32) {
    let sum = a + b + carry;
    if sum >= WORD_BASE as i32 {
        (sum - WORD_BASE as i32, 1)
    } else {
        (sum, 0)
    }
}

fn max_decimal(precision: i32, frac: i32, to: &mut MyDecimal) {
    let mut digits_int = precision - frac;
    to.negative = false;
    to.digits_int = digits_int as i8;
    let mut idx = 0usize;
    if digits_int > 0 {
        let first_word_digits = digits_int % DIGITS_PER_WORD;
        if first_word_digits > 0 {
            to.word_buf[idx] = POWERS10[first_word_digits as usize] - 1;
            idx += 1;
        }
        digits_int /= DIGITS_PER_WORD;
        while digits_int > 0 {
            to.word_buf[idx] = WORD_MAX;
            idx += 1;
            digits_int -= 1;
        }
    }
    to.digits_frac = frac as i8;
    let mut frac = frac;
    if frac > 0 {
        let last_digits = frac % DIGITS_PER_WORD;
        frac /= DIGITS_PER_WORD;
        while frac > 0 {
            to.word_buf[idx] = WORD_MAX;
            idx += 1;
            frac -= 1;
        }
        if last_digits > 0 {
            to.word_buf[idx] = FRAC_MAX[(last_digits - 1) as usize];
        }
    }
}

fn str_to_int(str: &[u8]) -> (i64, Option<DecimalError>) {
    const MAX_UINT: u64 = u64::MAX;
    const UINT_CUT_OFF: u64 = MAX_UINT / 10 + 1;
    const INT_CUT_OFF: u64 = (i64::MAX as u64) + 1;
    let trimmed = trim_go_space(str);
    if trimmed.is_empty() {
        return (0, Some(DecimalError::Truncated));
    }
    let mut negative = false;
    let mut i = 0usize;
    if trimmed[0] == b'-' {
        negative = true;
        i = 1;
    } else if trimmed[0] == b'+' {
        i = 1;
    }
    let mut err = None;
    let mut has_num = false;
    let mut r: u64 = 0;
    while i < trimmed.len() {
        if !trimmed[i].is_ascii_digit() {
            err = Some(DecimalError::Truncated);
            break;
        }
        has_num = true;
        if r >= UINT_CUT_OFF {
            r = 0;
            err = Some(DecimalError::BadNumber);
            break;
        }
        r *= 10;
        let r1 = r.wrapping_add(u64::from(trimmed[i] - b'0'));
        if r1 < r {
            r = 0;
            err = Some(DecimalError::BadNumber);
            break;
        }
        r = r1;
        i += 1;
    }
    if !has_num {
        err = Some(DecimalError::Truncated);
    }
    if !negative && r >= INT_CUT_OFF {
        return (i64::MAX, Some(DecimalError::BadNumber));
    }
    if negative && r > INT_CUT_OFF {
        return (i64::MIN, Some(DecimalError::BadNumber));
    }
    if negative {
        r = r.wrapping_neg();
    }
    (r as i64, err)
}

fn trim_go_space(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end {
        let Some(width) = utf8_char_width(bytes[start]) else {
            break;
        };
        if start + width > end {
            break;
        }
        let Ok(text) = std::str::from_utf8(&bytes[start..start + width]) else {
            break;
        };
        let mut chars = text.chars();
        let Some(ch) = chars.next() else {
            break;
        };
        if chars.next().is_some() || !ch.is_whitespace() {
            break;
        }
        start += width;
    }
    while end > start {
        let mut char_start = end - 1;
        while char_start > start && (bytes[char_start] & 0xc0) == 0x80 {
            char_start -= 1;
        }
        let Ok(text) = std::str::from_utf8(&bytes[char_start..end]) else {
            break;
        };
        let mut chars = text.chars();
        let Some(ch) = chars.next() else {
            break;
        };
        if chars.next().is_some() || !ch.is_whitespace() {
            break;
        }
        end = char_start;
    }
    &bytes[start..end]
}

fn utf8_char_width(first: u8) -> Option<usize> {
    match first {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

impl MyDecimal {
    /// Exact storage projection; no validation or normalization.
    pub fn from_raw_parts(parts: (i8, i8, i8, bool, [i32; 9])) -> Self {
        let (digits_int, digits_frac, result_frac, negative, word_buf) = parts;
        Self {
            digits_int,
            digits_frac,
            result_frac,
            negative,
            word_buf,
        }
    }

    pub fn raw_parts(self) -> (i8, i8, i8, bool, [i32; 9]) {
        (
            self.digits_int,
            self.digits_frac,
            self.result_frac,
            self.negative,
            self.word_buf,
        )
    }

    pub fn from_decimal_parts(
        negative: bool,
        coefficient: &str,
        storage_scale: u32,
        result_frac: u32,
        minimum_integer_digit: bool,
    ) -> Result<MyDecimal, DecimalError> {
        let bytes = coefficient.as_bytes();
        if bytes.is_empty() || bytes.iter().any(|byte| !byte.is_ascii_digit()) {
            return Err(DecimalError::BadNumber);
        }
        let storage_scale = usize::try_from(storage_scale).map_err(|_| DecimalError::Overflow)?;
        let result_frac = usize::try_from(result_frac).map_err(|_| DecimalError::Overflow)?;
        if storage_scale > bytes.len() || result_frac > storage_scale {
            return Err(DecimalError::BadNumber);
        }
        let integer_len = bytes.len() - storage_scale;
        let stored_integer_len = if minimum_integer_digit {
            integer_len.max(1)
        } else {
            integer_len
        };
        let words_int = digits_to_words(stored_integer_len as i32) as usize;
        let words_frac = digits_to_words(storage_scale as i32) as usize;
        if words_int + words_frac > MAX_WORD_BUF_LEN {
            return Err(DecimalError::Overflow);
        }
        let mut value = MyDecimal {
            digits_int: stored_integer_len as i8,
            digits_frac: storage_scale as i8,
            result_frac: result_frac as i8,
            negative: negative && bytes.iter().any(|byte| *byte != b'0'),
            word_buf: [0; MAX_WORD_BUF_LEN],
        };
        let mut word_idx = usize::from(minimum_integer_digit && integer_len == 0);
        let first_digits = integer_len % DIGITS_PER_WORD as usize;
        let mut integer_offset = 0usize;
        if first_digits > 0 {
            value.word_buf[word_idx] = parse_decimal_group(&bytes[..first_digits]);
            word_idx += 1;
            integer_offset = first_digits;
        }
        while integer_offset < integer_len {
            value.word_buf[word_idx] = parse_decimal_group(
                &bytes[integer_offset..integer_offset + DIGITS_PER_WORD as usize],
            );
            word_idx += 1;
            integer_offset += DIGITS_PER_WORD as usize;
        }
        let mut fraction_offset = integer_len;
        while fraction_offset < bytes.len() {
            let remaining = bytes.len() - fraction_offset;
            let take = remaining.min(DIGITS_PER_WORD as usize);
            let mut word = parse_decimal_group(&bytes[fraction_offset..fraction_offset + take]);
            if take < DIGITS_PER_WORD as usize {
                word *= POWERS10[DIGITS_PER_WORD as usize - take];
            }
            value.word_buf[word_idx] = word;
            word_idx += 1;
            fraction_offset += take;
        }
        Ok(value)
    }

    #[must_use]
    pub fn from_scaled_i128(
        value: i128,
        storage_scale: u32,
        result_frac: u32,
    ) -> Option<MyDecimal> {
        if result_frac > storage_scale || storage_scale > 38 {
            return None;
        }
        let magnitude = value.unsigned_abs();
        if storage_scale <= 18 {
            if let Ok(magnitude) = u64::try_from(magnitude) {
                return Self::from_scaled_u64(magnitude, value < 0, storage_scale, result_frac);
            }
        }
        let scale_pow = 10u128.checked_pow(storage_scale)?;
        let mut integer_part = magnitude / scale_pow;
        let fraction_part = magnitude % scale_pow;
        let mut digits_int = 1usize;
        let mut probe = integer_part;
        while probe >= 10 {
            probe /= 10;
            digits_int += 1;
        }
        let words_int = digits_to_words(digits_int as i32) as usize;
        let words_frac = digits_to_words(storage_scale as i32) as usize;
        if words_int + words_frac > MAX_WORD_BUF_LEN {
            return None;
        }
        let mut result = MyDecimal {
            digits_int: digits_int as i8,
            digits_frac: storage_scale as i8,
            result_frac: result_frac as i8,
            negative: value < 0,
            word_buf: [0; MAX_WORD_BUF_LEN],
        };
        for word_idx in (0..words_int).rev() {
            result.word_buf[word_idx] = (integer_part % u128::from(WORD_BASE)) as i32;
            integer_part /= u128::from(WORD_BASE);
        }
        let padding = words_frac * DIGITS_PER_WORD as usize - storage_scale as usize;
        let mut fraction_padded = fraction_part.checked_mul(10u128.checked_pow(padding as u32)?)?;
        for word_idx in (words_int..words_int + words_frac).rev() {
            result.word_buf[word_idx] = (fraction_padded % u128::from(WORD_BASE)) as i32;
            fraction_padded /= u128::from(WORD_BASE);
        }
        Some(result)
    }

    fn from_scaled_u64(
        magnitude: u64,
        negative: bool,
        storage_scale: u32,
        result_frac: u32,
    ) -> Option<MyDecimal> {
        debug_assert!(storage_scale <= 18);
        let scale_pow = 10u64.pow(storage_scale);
        let mut integer_part = magnitude / scale_pow;
        let fraction_part = magnitude % scale_pow;
        let digits_int = integer_part
            .checked_ilog10()
            .map_or(1, |log| log as usize + 1);
        let words_int = digits_to_words(digits_int as i32) as usize;
        let words_frac = digits_to_words(storage_scale as i32) as usize;
        if words_int + words_frac > MAX_WORD_BUF_LEN {
            return None;
        }
        let mut result = MyDecimal {
            digits_int: digits_int as i8,
            digits_frac: storage_scale as i8,
            result_frac: result_frac as i8,
            negative,
            word_buf: [0; MAX_WORD_BUF_LEN],
        };
        let word_base = WORD_BASE;
        for word_idx in (0..words_int).rev() {
            result.word_buf[word_idx] = (integer_part % word_base) as i32;
            integer_part /= word_base;
        }
        let padding = words_frac * DIGITS_PER_WORD as usize - storage_scale as usize;
        let mut fraction_padded = fraction_part * 10u64.pow(padding as u32);
        for word_idx in (words_int..words_int + words_frac).rev() {
            result.word_buf[word_idx] = (fraction_padded % word_base) as i32;
            fraction_padded /= word_base;
        }
        Some(result)
    }

    /// Original private helper exposed only for the native facade's tests.
    pub fn digit_bounds(&self) -> (i32, i32) {
        let buf_len = digits_to_words(i32::from(self.digits_int))
            + digits_to_words(i32::from(self.digits_frac));
        let mut buf_beg = 0i32;
        let mut buf_end = buf_len - 1;
        while buf_beg < buf_len && self.word_buf[buf_beg as usize] == 0 {
            buf_beg += 1;
        }
        if buf_beg >= buf_len {
            return (0, 0);
        }
        let mut i;
        let mut start;
        if buf_beg == 0 && self.digits_int > 0 {
            i = (i32::from(self.digits_int) - 1) % DIGITS_PER_WORD;
            start = DIGITS_PER_WORD - i - 1;
        } else {
            i = DIGITS_PER_WORD - 1;
            start = buf_beg * DIGITS_PER_WORD;
        }
        if buf_beg < buf_len {
            start += count_leading_zeroes(i, self.word_buf[buf_beg as usize]);
        }
        while buf_end > buf_beg && self.word_buf[buf_end as usize] == 0 {
            buf_end -= 1;
        }
        let mut end;
        if buf_end == buf_len - 1 && self.digits_frac > 0 {
            i = (i32::from(self.digits_frac) - 1) % DIGITS_PER_WORD + 1;
            end = buf_end * DIGITS_PER_WORD + i;
            i = DIGITS_PER_WORD - i + 1;
        } else {
            end = (buf_end + 1) * DIGITS_PER_WORD;
            i = 1;
        }
        end -= count_trailing_zeroes(i, self.word_buf[buf_end as usize]);
        (start, end)
    }

    fn do_mini_left_shift(&mut self, shift: i32, beg: i32, end: i32) {
        let mut buf_from = (beg / DIGITS_PER_WORD) as usize;
        let buf_end = ((end - 1) / DIGITS_PER_WORD) as usize;
        let c_shift = (DIGITS_PER_WORD - shift) as usize;
        if beg % DIGITS_PER_WORD < shift {
            self.word_buf[buf_from - 1] = self.word_buf[buf_from] / POWERS10[c_shift];
        }
        while buf_from < buf_end {
            self.word_buf[buf_from] = (self.word_buf[buf_from] % POWERS10[c_shift])
                * POWERS10[shift as usize]
                + self.word_buf[buf_from + 1] / POWERS10[c_shift];
            buf_from += 1;
        }
        self.word_buf[buf_from] =
            (self.word_buf[buf_from] % POWERS10[c_shift]) * POWERS10[shift as usize];
    }

    fn do_mini_right_shift(&mut self, shift: i32, beg: i32, end: i32) {
        let mut buf_from = ((end - 1) / DIGITS_PER_WORD) as usize;
        let buf_end = (beg / DIGITS_PER_WORD) as usize;
        let c_shift = (DIGITS_PER_WORD - shift) as usize;
        if DIGITS_PER_WORD - ((end - 1) % DIGITS_PER_WORD + 1) < shift {
            self.word_buf[buf_from + 1] =
                (self.word_buf[buf_from] % POWERS10[shift as usize]) * POWERS10[c_shift];
        }
        while buf_from > buf_end {
            self.word_buf[buf_from] = self.word_buf[buf_from] / POWERS10[shift as usize]
                + (self.word_buf[buf_from - 1] % POWERS10[shift as usize]) * POWERS10[c_shift];
            buf_from -= 1;
        }
        self.word_buf[buf_from] /= POWERS10[shift as usize];
    }

    pub fn round(
        &self,
        to: &mut MyDecimal,
        frac: i32,
        round_mode: RoundMode,
    ) -> Option<DecimalError> {
        let same = std::ptr::eq(self, to);
        debug_assert!(!same, "use round_in_place for the aliasing case");
        to.round_from(self, frac, round_mode)
    }

    pub fn round_in_place(&mut self, frac: i32, round_mode: RoundMode) -> Option<DecimalError> {
        let source = *self;
        self.round_from_aliased(&source, frac, round_mode, true)
    }

    fn round_from(
        &mut self,
        d: &MyDecimal,
        frac: i32,
        round_mode: RoundMode,
    ) -> Option<DecimalError> {
        self.round_from_aliased(d, frac, round_mode, false)
    }

    fn round_from_aliased(
        &mut self,
        d: &MyDecimal,
        frac: i32,
        round_mode: RoundMode,
        aliased: bool,
    ) -> Option<DecimalError> {
        let buf_len = MAX_WORD_BUF_LEN as i32;
        let mut frac = frac;
        let mut err = None;
        let mut words_frac_to = (frac + 1) / DIGITS_PER_WORD;
        if frac > 0 {
            words_frac_to = digits_to_words(frac);
        }
        let words_frac = digits_to_words(i32::from(d.digits_frac));
        let words_int = digits_to_words(i32::from(d.digits_int));
        let round_digit = round_mode as i32;
        if words_int + words_frac_to > buf_len {
            words_frac_to = buf_len - words_int;
            frac = words_frac_to * DIGITS_PER_WORD;
            err = Some(DecimalError::Truncated);
        }
        if i32::from(d.digits_int) + frac < 0 {
            *self = MyDecimal::default();
            return None;
        }
        if !aliased {
            self.word_buf = d.word_buf;
            self.negative = d.negative;
            self.digits_int = (words_int.min(buf_len) * DIGITS_PER_WORD) as i8;
        }
        if words_frac_to > words_frac {
            let mut idx = (words_int + words_frac) as usize;
            while words_frac_to > words_frac {
                words_frac_to -= 1;
                self.word_buf[idx] = 0;
                idx += 1;
            }
            self.digits_frac = frac as i8;
            self.result_frac = self.digits_frac;
            return err;
        }
        if frac >= i32::from(d.digits_frac) {
            self.digits_frac = frac as i8;
            self.result_frac = self.digits_frac;
            return err;
        }
        let mut to_idx = words_int + words_frac_to - 1;
        if frac == words_frac_to * DIGITS_PER_WORD {
            let do_inc = match round_mode {
                RoundMode::Ceiling => {
                    let mut idx = to_idx + (words_frac - words_frac_to);
                    let mut inc = false;
                    while idx > to_idx {
                        if d.word_buf[idx as usize] != 0 {
                            inc = true;
                            break;
                        }
                        idx -= 1;
                    }
                    inc
                }
                RoundMode::HalfUp => d.word_buf[(to_idx + 1) as usize] / DIG_MASK >= 5,
                RoundMode::Truncate => false,
            };
            if do_inc {
                if to_idx >= 0 {
                    self.word_buf[to_idx as usize] += 1;
                } else {
                    to_idx += 1;
                    self.word_buf[to_idx as usize] = WORD_BASE as i32;
                }
            } else if words_int + words_frac_to == 0 {
                *self = MyDecimal::default();
                return None;
            }
        } else {
            let pos = (words_frac_to * DIGITS_PER_WORD - frac - 1) as usize;
            let word = self.word_buf[to_idx as usize];
            let scale = POWERS10[pos];
            let mut shifted_number = word / scale;
            let dig_after_scale = shifted_number % 10;
            let discarded_nonzero = word % (scale * 10) != 0;
            let do_inc = match round_mode {
                RoundMode::Ceiling => discarded_nonzero,
                RoundMode::HalfUp => {
                    dig_after_scale > round_digit || (round_digit == 5 && dig_after_scale == 5)
                }
                RoundMode::Truncate => false,
            };
            if do_inc {
                shifted_number += 10;
            }
            self.word_buf[to_idx as usize] = scale * (shifted_number - dig_after_scale);
        }
        if words_frac_to < words_frac {
            let mut idx = words_int + words_frac_to;
            if frac == 0 && words_int == 0 {
                idx = 1;
            }
            while idx < buf_len {
                self.word_buf[idx as usize] = 0;
                idx += 1;
            }
        }
        if self.word_buf[to_idx as usize] >= WORD_BASE as i32 {
            let mut carry = 1;
            self.word_buf[to_idx as usize] -= WORD_BASE as i32;
            while carry == 1 && to_idx > 0 {
                to_idx -= 1;
                let (sum, new_carry) = add_word(self.word_buf[to_idx as usize], 0, carry);
                self.word_buf[to_idx as usize] = sum;
                carry = new_carry;
            }
            if carry > 0 {
                if words_int + words_frac_to >= buf_len {
                    words_frac_to -= 1;
                    frac = words_frac_to * DIGITS_PER_WORD;
                    err = Some(DecimalError::Truncated);
                }
                to_idx = words_int + words_frac_to.max(0);
                while to_idx > 0 {
                    if to_idx < buf_len {
                        self.word_buf[to_idx as usize] = self.word_buf[(to_idx - 1) as usize];
                    } else {
                        err = Some(DecimalError::Overflow);
                    }
                    to_idx -= 1;
                }
                self.word_buf[to_idx as usize] = 1;
                if i32::from(self.digits_int) < DIGITS_PER_WORD * buf_len {
                    self.digits_int += 1;
                } else {
                    err = Some(DecimalError::Overflow);
                }
            }
        } else {
            loop {
                if self.word_buf[to_idx as usize] != 0 {
                    break;
                }
                if to_idx == 0 {
                    let idx = words_frac_to + 1;
                    self.digits_int = 1;
                    self.digits_frac = frac.max(0) as i8;
                    self.negative = false;
                    while to_idx < idx {
                        self.word_buf[to_idx as usize] = 0;
                        to_idx += 1;
                    }
                    self.result_frac = self.digits_frac;
                    return None;
                }
                to_idx -= 1;
            }
        }
        let first_dig = i32::from(self.digits_int) % DIGITS_PER_WORD;
        if first_dig > 0 && self.word_buf[to_idx as usize] >= POWERS10[first_dig as usize] {
            self.digits_int += 1;
        }
        if frac < 0 {
            frac = 0;
        }
        self.digits_frac = frac as i8;
        self.result_frac = self.digits_frac;
        err
    }

    pub fn shift(&mut self, shift: i32) -> Option<DecimalError> {
        let buf_len = MAX_WORD_BUF_LEN as i32;
        let mut err = None;
        if shift == 0 {
            return None;
        }
        let point = digits_to_words(i32::from(self.digits_int)) * DIGITS_PER_WORD;
        let mut new_point = point + shift;
        let (mut digit_begin, mut digit_end) = self.digit_bounds();
        if digit_begin == digit_end {
            *self = MyDecimal::default();
            return None;
        }
        let digits_int = (new_point - digit_begin).max(0);
        let mut digits_frac = (digit_end - new_point).max(0);
        let words_int = digits_to_words(digits_int);
        let mut words_frac = digits_to_words(digits_frac);
        let new_len = words_int + words_frac;
        if new_len > buf_len {
            let lack = new_len - buf_len;
            if words_frac < lack {
                return Some(DecimalError::Overflow);
            }
            err = Some(DecimalError::Truncated);
            words_frac -= lack;
            let diff = digits_frac - words_frac * DIGITS_PER_WORD;
            if let Some(err1) = self.round_in_place(digit_end - point - diff, RoundMode::HalfUp) {
                return Some(err1);
            }
            digit_end -= diff;
            digits_frac = words_frac * DIGITS_PER_WORD;
            if digit_end <= digit_begin {
                *self = MyDecimal::default();
                return Some(DecimalError::Truncated);
            }
        }
        if shift % DIGITS_PER_WORD != 0 {
            let l_mini_shift;
            let r_mini_shift;
            let do_left;
            if shift > 0 {
                l_mini_shift = shift % DIGITS_PER_WORD;
                r_mini_shift = DIGITS_PER_WORD - l_mini_shift;
                do_left = l_mini_shift <= digit_begin;
            } else {
                r_mini_shift = (-shift) % DIGITS_PER_WORD;
                l_mini_shift = DIGITS_PER_WORD - r_mini_shift;
                do_left = (DIGITS_PER_WORD * buf_len - digit_end) < r_mini_shift;
            }
            let mini_shift = if do_left {
                self.do_mini_left_shift(l_mini_shift, digit_begin, digit_end);
                -l_mini_shift
            } else {
                self.do_mini_right_shift(r_mini_shift, digit_begin, digit_end);
                r_mini_shift
            };
            new_point += mini_shift;
            if shift + mini_shift == 0 && (new_point - digits_int) < DIGITS_PER_WORD {
                self.digits_int = digits_int as i8;
                self.digits_frac = digits_frac as i8;
                return err;
            }
            digit_begin += mini_shift;
            digit_end += mini_shift;
        }
        let new_front = new_point - digits_int;
        if !(0..DIGITS_PER_WORD).contains(&new_front) {
            let mut word_shift;
            if new_front > 0 {
                word_shift = new_front / DIGITS_PER_WORD;
                let mut to = digit_begin / DIGITS_PER_WORD - word_shift;
                let mut barier = (digit_end - 1) / DIGITS_PER_WORD - word_shift;
                while to <= barier {
                    self.word_buf[to as usize] = self.word_buf[(to + word_shift) as usize];
                    to += 1;
                }
                barier += word_shift;
                while to <= barier {
                    self.word_buf[to as usize] = 0;
                    to += 1;
                }
                word_shift = -word_shift;
            } else {
                word_shift = (1 - new_front) / DIGITS_PER_WORD;
                let mut to = (digit_end - 1) / DIGITS_PER_WORD + word_shift;
                let mut barier = digit_begin / DIGITS_PER_WORD + word_shift;
                while to >= barier {
                    self.word_buf[to as usize] = self.word_buf[(to - word_shift) as usize];
                    to -= 1;
                }
                barier -= word_shift;
                while to >= barier {
                    self.word_buf[to as usize] = 0;
                    to -= 1;
                }
            }
            let digit_shift = word_shift * DIGITS_PER_WORD;
            digit_begin += digit_shift;
            digit_end += digit_shift;
            new_point += digit_shift;
        }
        let word_idx_begin = digit_begin / DIGITS_PER_WORD;
        let word_idx_end = (digit_end - 1) / DIGITS_PER_WORD;
        let mut word_idx_new_point = 0;
        if new_point != 0 {
            word_idx_new_point = (new_point - 1) / DIGITS_PER_WORD;
        }
        if word_idx_new_point > word_idx_end {
            while word_idx_new_point > word_idx_end {
                self.word_buf[word_idx_new_point as usize] = 0;
                word_idx_new_point -= 1;
            }
        } else {
            while word_idx_new_point < word_idx_begin {
                self.word_buf[word_idx_new_point as usize] = 0;
                word_idx_new_point += 1;
            }
        }
        self.digits_int = digits_int as i8;
        self.digits_frac = digits_frac as i8;
        err
    }

    pub fn from_string(str: &[u8]) -> (MyDecimal, Option<DecimalError>) {
        let mut d = MyDecimal::default();
        let err = d.set_from_string(str);
        (d, err)
    }

    fn set_from_string(&mut self, str: &[u8]) -> Option<DecimalError> {
        let buf_len = MAX_WORD_BUF_LEN as i32;
        let mut str = str;
        for i in 0..str.len() {
            if !is_space(str[i]) {
                str = &str[i..];
                break;
            }
        }
        if str.is_empty() {
            *self = MyDecimal::default();
            return Some(DecimalError::TruncatedWrongValue);
        }
        match str[0] {
            b'-' => {
                self.negative = true;
                str = &str[1..];
            }
            b'+' => str = &str[1..],
            _ => {}
        }
        let mut str_idx = 0usize;
        while str_idx < str.len() && str[str_idx].is_ascii_digit() {
            str_idx += 1;
        }
        let mut digits_int = str_idx as i32;
        let mut digits_frac;
        let end_idx;
        if str_idx < str.len() && str[str_idx] == b'.' {
            let mut e = str_idx + 1;
            while e < str.len() && str[e].is_ascii_digit() {
                e += 1;
            }
            digits_frac = (e - str_idx - 1) as i32;
            end_idx = e;
        } else {
            digits_frac = 0;
            end_idx = str_idx;
        }
        if digits_int + digits_frac == 0 {
            *self = MyDecimal::default();
            return Some(DecimalError::TruncatedWrongValue);
        }
        let words_int_raw = digits_to_words(digits_int);
        let words_frac_raw = digits_to_words(digits_frac);
        let (words_int, words_frac, mut err) = fix_word_cnt_error(words_int_raw, words_frac_raw);
        if err.is_some() {
            digits_frac = words_frac * DIGITS_PER_WORD;
            if err == Some(DecimalError::Overflow) {
                digits_int = words_int * DIGITS_PER_WORD;
            }
        }
        self.digits_int = digits_int as i8;
        self.digits_frac = digits_frac as i8;
        let mut word_idx = words_int;
        let str_idx_tmp = str_idx;
        let mut word: i32 = 0;
        let mut inner_idx = 0i32;
        while digits_int > 0 {
            digits_int -= 1;
            str_idx -= 1;
            word += i32::from(str[str_idx] - b'0') * POWERS10[inner_idx as usize];
            inner_idx += 1;
            if inner_idx == DIGITS_PER_WORD {
                word_idx -= 1;
                self.word_buf[word_idx as usize] = word;
                word = 0;
                inner_idx = 0;
            }
        }
        if inner_idx != 0 {
            word_idx -= 1;
            self.word_buf[word_idx as usize] = word;
        }
        word_idx = words_int;
        str_idx = str_idx_tmp;
        word = 0;
        inner_idx = 0;
        while digits_frac > 0 {
            digits_frac -= 1;
            str_idx += 1;
            word = i32::from(str[str_idx] - b'0') + word * 10;
            inner_idx += 1;
            if inner_idx == DIGITS_PER_WORD {
                self.word_buf[word_idx as usize] = word;
                word_idx += 1;
                word = 0;
                inner_idx = 0;
            }
        }
        if inner_idx != 0 {
            self.word_buf[word_idx as usize] =
                word * POWERS10[(DIGITS_PER_WORD - inner_idx) as usize];
        }
        if end_idx < str.len() {
            if str[end_idx] == b'e' || str[end_idx] == b'E' {
                let (exponent, err1) = str_to_int(&str[end_idx + 1..]);
                if let Some(cause) = err1 {
                    err = Some(cause);
                    if cause != DecimalError::Truncated {
                        *self = MyDecimal::default();
                    }
                }
                if exponent > i64::from(i32::MAX / 2) {
                    let negative = self.negative;
                    max_decimal(buf_len * DIGITS_PER_WORD, 0, self);
                    self.negative = negative;
                    err = Some(DecimalError::Overflow);
                }
                if exponent < i64::from(i32::MIN / 2) && err != Some(DecimalError::Overflow) {
                    *self = MyDecimal::default();
                    err = Some(DecimalError::Truncated);
                }
                if err != Some(DecimalError::Overflow) {
                    if let Some(shift_err) = self.shift(exponent as i32) {
                        if shift_err == DecimalError::Overflow {
                            let negative = self.negative;
                            max_decimal(buf_len * DIGITS_PER_WORD, 0, self);
                            self.negative = negative;
                        }
                        err = Some(shift_err);
                    }
                }
            } else if !trim_go_space(&str[end_idx..]).is_empty() {
                err = Some(DecimalError::Truncated);
            }
        }
        if self.word_buf.iter().all(|word| *word == 0) {
            self.negative = false;
        }
        self.result_frac = self.digits_frac;
        err
    }

    #[must_use]
    pub fn from_int(val: i64) -> MyDecimal {
        let mut d = MyDecimal::default();
        let u_val = if val < 0 {
            d.negative = true;
            val.unsigned_abs()
        } else {
            val as u64
        };
        d.set_from_uint(u_val);
        d
    }

    #[must_use]
    pub fn from_uint(val: u64) -> MyDecimal {
        let mut d = MyDecimal::default();
        d.set_from_uint(val);
        d
    }

    fn set_from_uint(&mut self, val: u64) {
        let mut x = val;
        let mut word_idx = 1usize;
        while x >= WORD_BASE {
            word_idx += 1;
            x /= WORD_BASE;
        }
        self.digits_frac = 0;
        self.digits_int = (word_idx as i32 * DIGITS_PER_WORD) as i8;
        x = val;
        while word_idx > 0 {
            word_idx -= 1;
            let y = x / WORD_BASE;
            self.word_buf[word_idx] = (x - y * WORD_BASE) as i32;
            x = y;
        }
    }

    #[must_use]
    pub fn is_negative(&self) -> bool {
        self.negative
    }

    #[must_use]
    pub fn to_int(&self) -> (i64, Option<DecimalError>) {
        let mut x: i64 = 0;
        let mut word_idx = 0usize;
        let mut i = i32::from(self.digits_int);
        while i > 0 {
            let y = x;
            x = x
                .wrapping_mul(WORD_BASE as i64)
                .wrapping_sub(i64::from(self.word_buf[word_idx]));
            word_idx += 1;
            if y < i64::MIN / (WORD_BASE as i64) || x > y {
                return if self.negative {
                    (i64::MIN, Some(DecimalError::Overflow))
                } else {
                    (i64::MAX, Some(DecimalError::Overflow))
                };
            }
            i -= DIGITS_PER_WORD;
        }
        if !self.negative && x == i64::MIN {
            return (i64::MAX, Some(DecimalError::Overflow));
        }
        if !self.negative {
            x = -x;
        }
        let mut i = i32::from(self.digits_frac);
        while i > 0 {
            if self.word_buf[word_idx] != 0 {
                return (x, Some(DecimalError::Truncated));
            }
            word_idx += 1;
            i -= DIGITS_PER_WORD;
        }
        (x, None)
    }

    #[must_use]
    pub fn to_uint(&self) -> (u64, Option<DecimalError>) {
        if self.negative {
            return (0, Some(DecimalError::Overflow));
        }
        let mut x: u64 = 0;
        let mut word_idx = 0usize;
        let mut i = i32::from(self.digits_int);
        while i > 0 {
            let y = x;
            x = x
                .wrapping_mul(WORD_BASE)
                .wrapping_add(self.word_buf[word_idx] as u64);
            word_idx += 1;
            if y > u64::MAX / WORD_BASE || x < y {
                return (u64::MAX, Some(DecimalError::Overflow));
            }
            i -= DIGITS_PER_WORD;
        }
        let mut i = i32::from(self.digits_frac);
        while i > 0 {
            if self.word_buf[word_idx] != 0 {
                return (x, Some(DecimalError::Truncated));
            }
            word_idx += 1;
            i -= DIGITS_PER_WORD;
        }
        (x, None)
    }

    pub fn from_float64(f: f64) -> (MyDecimal, Option<DecimalError>) {
        MyDecimal::from_string(format_float_g_shortest(f).as_bytes())
    }

    #[must_use]
    pub fn to_float64(&self) -> (f64, Option<DecimalError>) {
        let digits_int = i32::from(self.digits_int);
        let digits_frac = i32::from(self.digits_frac);
        if digits_int + digits_frac > 12 {
            let text = String::from_utf8(self.to_string_bytes())
                .expect("MyDecimal::to_string_bytes is ASCII");
            return match text.parse::<f64>() {
                Ok(value) if value.is_finite() => (value, None),
                _ => (0.0, Some(DecimalError::Overflow)),
            };
        }
        let words_int = (digits_int - 1) / DIGITS_PER_WORD + 1;
        let mut word_idx = 0i32;
        let mut f = 0.0f64;
        let mut i = 0;
        while i < digits_int {
            let x = self.word_buf[word_idx as usize];
            word_idx += 1;
            f += f64::from(x) * pow10((words_int - word_idx) * DIGITS_PER_WORD);
            i += DIGITS_PER_WORD;
        }
        let frac_start = word_idx;
        let mut i = 0;
        while i < digits_frac {
            let x = self.word_buf[word_idx as usize];
            word_idx += 1;
            f += f64::from(x) * pow10(-DIGITS_PER_WORD * (word_idx - frac_start));
            i += DIGITS_PER_WORD;
        }
        let unit = pow10(i32::from(self.result_frac));
        f = (f * unit).round() / unit;
        if self.negative {
            f = -f;
        }
        (f, None)
    }

    #[must_use]
    pub fn compare(&self, other: &MyDecimal) -> std::cmp::Ordering {
        if self.negative == other.negative {
            return match self.compare_magnitude(other) {
                0 => std::cmp::Ordering::Equal,
                order if order < 0 => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Greater,
            };
        }
        if self.negative {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    }

    fn compare_magnitude(&self, other: &MyDecimal) -> i32 {
        let from1 = self;
        let from2 = other;
        let mut words_int1 = digits_to_words(i32::from(from1.digits_int));
        let mut words_frac1 = digits_to_words(i32::from(from1.digits_frac));
        let mut words_int2 = digits_to_words(i32::from(from2.digits_int));
        let mut words_frac2 = digits_to_words(i32::from(from2.digits_frac));
        let stop1 = words_int1;
        let mut idx1 = 0;
        let stop2 = words_int2;
        let mut idx2 = 0;
        if from1.word_buf[idx1 as usize] == 0 {
            while idx1 < stop1 && from1.word_buf[idx1 as usize] == 0 {
                idx1 += 1;
            }
            words_int1 = stop1 - idx1;
        }
        if from2.word_buf[idx2 as usize] == 0 {
            while idx2 < stop2 && from2.word_buf[idx2 as usize] == 0 {
                idx2 += 1;
            }
            words_int2 = stop2 - idx2;
        }
        let mut carry = 0;
        if words_int2 > words_int1 {
            carry = 1;
        } else if words_int2 == words_int1 {
            let mut end1 = stop1 + words_frac1 - 1;
            let mut end2 = stop2 + words_frac2 - 1;
            while idx1 <= end1 && from1.word_buf[end1 as usize] == 0 {
                end1 -= 1;
            }
            while idx2 <= end2 && from2.word_buf[end2 as usize] == 0 {
                end2 -= 1;
            }
            words_frac1 = end1 - stop1 + 1;
            words_frac2 = end2 - stop2 + 1;
            let _ = (words_frac1, words_frac2);
            while idx1 <= end1
                && idx2 <= end2
                && from1.word_buf[idx1 as usize] == from2.word_buf[idx2 as usize]
            {
                idx1 += 1;
                idx2 += 1;
            }
            if idx1 <= end1 {
                carry = i32::from(
                    idx2 <= end2 && from2.word_buf[idx2 as usize] > from1.word_buf[idx1 as usize],
                );
            } else {
                if idx2 > end2 {
                    return 0;
                }
                carry = 1;
            }
        }
        if (carry > 0) == from1.negative { 1 } else { -1 }
    }

    #[must_use]
    pub fn digits_frac(&self) -> i8 {
        self.digits_frac
    }

    #[must_use]
    pub fn coefficient_i128(&self) -> Option<(i128, u32)> {
        const I128_MAX_DIGITS: usize = 38;
        let (negative, digits, storage_scale, _) = self.to_decimal_parts();
        if digits.is_empty() || digits.len() > I128_MAX_DIGITS {
            return None;
        }
        let mut coefficient: i128 = 0;
        for &digit in digits.iter() {
            let value = i128::from(digit.wrapping_sub(b'0'));
            coefficient = coefficient.checked_mul(10)?.checked_add(value)?;
        }
        let value = if negative {
            coefficient.checked_neg()?
        } else {
            coefficient
        };
        Some((value, storage_scale))
    }

    #[must_use]
    pub fn result_frac(&self) -> i8 {
        self.result_frac
    }

    pub fn set_result_frac(&mut self, result_frac: i8) {
        debug_assert!(result_frac >= 0 && result_frac <= self.digits_frac);
        self.result_frac = result_frac;
    }

    fn remove_leading_zeros(&self) -> (usize, i32) {
        let mut word_idx = 0usize;
        let mut digits_int = i32::from(self.digits_int);
        let mut i = ((digits_int - 1) % DIGITS_PER_WORD) + 1;
        while digits_int > 0 && self.word_buf[word_idx] == 0 {
            digits_int -= i;
            i = DIGITS_PER_WORD;
            word_idx += 1;
        }
        if digits_int > 0 {
            digits_int -=
                count_leading_zeroes((digits_int - 1) % DIGITS_PER_WORD, self.word_buf[word_idx]);
        } else {
            digits_int = 0;
        }
        (word_idx, digits_int)
    }

    fn remove_trailing_zeros(&self) -> (usize, i32) {
        let mut digits_frac = i32::from(self.digits_frac);
        let mut i = ((digits_frac - 1) % DIGITS_PER_WORD) + 1;
        let mut last_word_idx =
            (digits_to_words(i32::from(self.digits_int)) + digits_to_words(digits_frac)) as usize;
        while digits_frac > 0 && self.word_buf[last_word_idx - 1] == 0 {
            digits_frac -= i;
            i = DIGITS_PER_WORD;
            last_word_idx -= 1;
        }
        if digits_frac > 0 {
            digits_frac -= count_trailing_zeroes(
                9 - ((digits_frac - 1) % DIGITS_PER_WORD),
                self.word_buf[last_word_idx - 1],
            );
        } else {
            digits_frac = 0;
        }
        (last_word_idx, digits_frac)
    }

    pub fn to_hash_key(&self) -> Result<SmallVec<[u8; 64]>, DecimalCodecError> {
        let (_, digits_int) = self.remove_leading_zeros();
        let (_, digits_frac) = self.remove_trailing_zeros();
        let mut prec = digits_int + digits_frac;
        if prec == 0 {
            prec = 1;
        }
        let size = checked_bin_size(prec, digits_frac)?;
        let mut key = SmallVec::from_elem(0u8, size + 1);
        // The old hash caller ignores the encoder's soft truncation status.
        write_bin(
            self.negative,
            i32::from(self.digits_int),
            i32::from(self.digits_frac),
            &self.word_buf,
            prec,
            digits_frac,
            &mut key[..size],
        )?;
        key[size] = digits_frac as u8;
        Ok(key)
    }

    #[must_use]
    pub fn to_string_bytes(&self) -> Vec<u8> {
        let mut output = Vec::new();
        self.append_to_string_bytes(&mut output);
        output
    }

    pub fn append_to_string_bytes(&self, output: &mut Vec<u8>) {
        let digits_frac_total = i32::from(self.digits_frac);
        let mut digits_frac = digits_frac_total;
        let (word_start_idx, mut digits_int) = self.remove_leading_zeros();
        let mut word_start_idx = word_start_idx;
        if digits_int + digits_frac == 0 {
            digits_int = 1;
            word_start_idx = 0;
        }
        let digits_int_len = if digits_int == 0 { 1 } else { digits_int };
        let digits_frac_len = digits_frac;
        let mut length = digits_int_len + digits_frac_len;
        if self.negative {
            length += 1;
        }
        if digits_frac > 0 {
            length += 1;
        }
        let start = output.len();
        output.resize(start + length as usize, 0);
        let str = &mut output[start..];
        let mut str_idx = 0usize;
        if self.negative {
            str[str_idx] = b'-';
            str_idx += 1;
        }
        let mut fill;
        if digits_frac > 0 {
            let mut frac_idx = str_idx + digits_int_len as usize;
            fill = digits_frac_len - digits_frac;
            let mut word_idx = word_start_idx + digits_to_words(digits_int) as usize;
            str[frac_idx] = b'.';
            frac_idx += 1;
            while digits_frac > 0 {
                let mut x = self.word_buf[word_idx];
                word_idx += 1;
                let mut i = digits_frac.min(DIGITS_PER_WORD);
                while i > 0 {
                    let y = x / DIG_MASK;
                    str[frac_idx] = y as u8 + b'0';
                    frac_idx += 1;
                    x -= y * DIG_MASK;
                    x *= 10;
                    i -= 1;
                }
                digits_frac -= DIGITS_PER_WORD;
            }
            while fill > 0 {
                str[frac_idx] = b'0';
                frac_idx += 1;
                fill -= 1;
            }
        }
        fill = digits_int_len - digits_int;
        if digits_int == 0 {
            fill -= 1;
        }
        while fill > 0 {
            str[str_idx] = b'0';
            str_idx += 1;
            fill -= 1;
        }
        if digits_int > 0 {
            str_idx += digits_int as usize;
            let mut word_idx = word_start_idx + digits_to_words(digits_int) as usize;
            while digits_int > 0 {
                word_idx -= 1;
                let mut x = self.word_buf[word_idx];
                let mut i = digits_int.min(DIGITS_PER_WORD);
                while i > 0 {
                    let y = x / 10;
                    str_idx -= 1;
                    str[str_idx] = b'0' + (x - y * 10) as u8;
                    x = y;
                    i -= 1;
                }
                digits_int -= DIGITS_PER_WORD;
            }
        } else {
            str[str_idx] = b'0';
        }
    }

    #[must_use]
    pub fn to_result_string_bytes(&self) -> Vec<u8> {
        let mut output = Vec::new();
        self.append_result_string_bytes(&mut output);
        output
    }

    pub fn append_result_string_bytes(&self, output: &mut Vec<u8>) {
        let mut rounded = *self;
        let _ = rounded.round_in_place(i32::from(self.result_frac), RoundMode::HalfUp);
        rounded.append_to_string_bytes(output);
    }

    pub fn to_decimal_parts(self) -> (bool, SmallVec<[u8; 24]>, u32, u32) {
        let (word_start_idx, digits_int) = self.remove_leading_zeros();
        let integer_len = digits_int.max(1) as usize;
        let fraction_len = i32::from(self.digits_frac).max(0) as usize;
        let mut coefficient = SmallVec::<[u8; 24]>::new();
        coefficient.resize(integer_len + fraction_len, b'0');
        if digits_int > 0 {
            let mut pos = integer_len;
            let mut word_idx = word_start_idx + digits_to_words(digits_int) as usize;
            let mut remaining = digits_int;
            while remaining > 0 {
                word_idx -= 1;
                let mut word = self.word_buf[word_idx];
                let take = remaining.min(DIGITS_PER_WORD);
                for _ in 0..take {
                    let next = word / 10;
                    pos -= 1;
                    coefficient[pos] = b'0' + (word - next * 10) as u8;
                    word = next;
                }
                remaining -= DIGITS_PER_WORD;
            }
        }
        if fraction_len > 0 {
            let mut offset = integer_len;
            let mut word_idx = word_start_idx + digits_to_words(digits_int) as usize;
            let mut remaining = fraction_len as i32;
            while remaining > 0 {
                let mut word = self.word_buf[word_idx];
                word_idx += 1;
                let take = remaining.min(DIGITS_PER_WORD);
                for _ in 0..take {
                    let next = word / DIG_MASK;
                    coefficient[offset] = b'0' + next as u8;
                    offset += 1;
                    word = (word - next * DIG_MASK) * 10;
                }
                remaining -= DIGITS_PER_WORD;
            }
        }
        (
            self.negative,
            coefficient,
            fraction_len as u32,
            i32::from(self.result_frac).max(0) as u32,
        )
    }

    pub fn to_i128_scaled(&self) -> Option<(i128, u32)> {
        let integer_words = digits_to_words(i32::from(self.digits_int)) as usize;
        let fraction_words = digits_to_words(i32::from(self.digits_frac)) as usize;
        let fraction_padding =
            fraction_words * DIGITS_PER_WORD as usize - usize::try_from(self.digits_frac).ok()?;
        let used_words = integer_words + fraction_words;
        let mut magnitude = 0_i128;
        for (index, word) in self.word_buf[..used_words].iter().enumerate() {
            if fraction_padding > 0 && index + 1 == used_words {
                let kept = POWERS10[DIGITS_PER_WORD as usize - fraction_padding];
                magnitude = magnitude
                    .checked_mul(i128::from(kept))?
                    .checked_add(i128::from(*word / POWERS10[fraction_padding]))?;
            } else {
                magnitude = magnitude
                    .checked_mul(i128::from(WORD_BASE))?
                    .checked_add(i128::from(*word))?;
            }
        }
        let signed = if self.negative {
            magnitude.checked_neg()?
        } else {
            magnitude
        };
        Some((signed, self.digits_frac as u32))
    }

    pub fn i128_scaled_from_raw_bytes(bytes: &[u8]) -> Option<(i128, u32)> {
        if bytes.len() != MYDECIMAL_STRUCT_SIZE || bytes[3] > 1 {
            return None;
        }
        let digits_int = bytes[0] as i8;
        let digits_frac = bytes[1] as i8;
        let result_frac = bytes[2] as i8;
        if digits_int < 0 || digits_frac < 0 || result_frac < 0 {
            return None;
        }
        let integer_words = digits_to_words(i32::from(digits_int)) as usize;
        let fraction_words = digits_to_words(i32::from(digits_frac)) as usize;
        let used_words = integer_words.checked_add(fraction_words)?;
        if used_words > MAX_WORD_BUF_LEN {
            return None;
        }
        let fraction_padding =
            fraction_words * DIGITS_PER_WORD as usize - usize::try_from(digits_frac).ok()?;
        let mut magnitude = 0_i128;
        let words = bytes[4..4 + used_words * 4].as_chunks::<4>().0;
        for (index, chunk) in words.iter().enumerate() {
            let word = i32::from_ne_bytes(*chunk);
            if !(0..WORD_BASE as i32).contains(&word) {
                return None;
            }
            if fraction_padding > 0 && index + 1 == used_words {
                let kept = POWERS10[DIGITS_PER_WORD as usize - fraction_padding];
                magnitude = magnitude
                    .checked_mul(i128::from(kept))?
                    .checked_add(i128::from(word / POWERS10[fraction_padding]))?;
            } else {
                magnitude = magnitude
                    .checked_mul(i128::from(WORD_BASE))?
                    .checked_add(i128::from(word))?;
            }
        }
        if bytes[3] == 1 {
            magnitude = magnitude.checked_neg()?;
        }
        Some((magnitude, digits_frac as u32))
    }

    #[must_use]
    pub fn to_raw_bytes(&self) -> [u8; MYDECIMAL_STRUCT_SIZE] {
        let mut bytes = [0u8; MYDECIMAL_STRUCT_SIZE];
        bytes[0] = self.digits_int as u8;
        bytes[1] = self.digits_frac as u8;
        bytes[2] = self.result_frac as u8;
        bytes[3] = u8::from(self.negative);
        for (chunk, w) in bytes[4..]
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(&self.word_buf)
        {
            *chunk = w.to_ne_bytes();
        }
        bytes
    }

    pub fn from_raw_bytes(bytes: [u8; MYDECIMAL_STRUCT_SIZE]) -> Result<MyDecimal, &'static str> {
        if bytes[3] > 1 {
            return Err("invalid MyDecimal negative flag byte");
        }
        let digits_int = bytes[0] as i8;
        let digits_frac = bytes[1] as i8;
        let result_frac = bytes[2] as i8;
        if digits_int < 0 || digits_frac < 0 || result_frac < 0 {
            return Err("invalid negative MyDecimal digit count");
        }
        let used_words =
            digits_to_words(i32::from(digits_int)) + digits_to_words(i32::from(digits_frac));
        if used_words > MAX_WORD_BUF_LEN as i32 {
            return Err("MyDecimal digit counts exceed the word buffer");
        }
        let mut d = MyDecimal {
            digits_int,
            digits_frac,
            result_frac,
            negative: bytes[3] == 1,
            word_buf: [0; MAX_WORD_BUF_LEN],
        };
        for (w, chunk) in d.word_buf.iter_mut().zip(bytes[4..].as_chunks::<4>().0) {
            *w = i32::from_ne_bytes(*chunk);
        }
        if d.word_buf[..used_words as usize]
            .iter()
            .any(|word| !(0..WORD_BASE as i32).contains(word))
        {
            return Err("MyDecimal word is outside base-1e9 storage");
        }
        Ok(d)
    }

    #[must_use]
    pub fn from_raw_bytes_like_go(bytes: [u8; MYDECIMAL_STRUCT_SIZE]) -> MyDecimal {
        let mut word_buf = [0; MAX_WORD_BUF_LEN];
        for (index, word) in word_buf.iter_mut().enumerate() {
            let start = 4 + index * 4;
            *word = i32::from_ne_bytes(bytes[start..start + 4].try_into().expect("4-byte word"));
        }
        MyDecimal {
            digits_int: bytes[0] as i8,
            digits_frac: bytes[1] as i8,
            result_frac: bytes[2] as i8,
            negative: bytes[3] & 1 != 0,
            word_buf,
        }
    }
}

fn parse_decimal_group(group: &[u8]) -> i32 {
    group
        .iter()
        .fold(0i32, |value, byte| value * 10 + i32::from(byte - b'0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_mydecimal_keeps_status_raw_storage_and_round_render_paths() {
        assert_eq!(std::mem::size_of::<MyDecimal>(), 40);
        let (value, error) = MyDecimal::from_string(b"1.25junk");
        assert_eq!(error, Some(DecimalError::Truncated));
        assert_eq!(value.to_string_bytes(), b"1.25");
        assert_eq!(
            MyDecimal::from_string(b" \t+.").1,
            Some(DecimalError::TruncatedWrongValue)
        );
        let mut parts = value.raw_parts();
        parts.2 = 1;
        parts.4[8] = -12345;
        let stored = MyDecimal::from_raw_parts(parts);
        assert_eq!(
            MyDecimal::from_raw_bytes(stored.to_raw_bytes())
                .unwrap()
                .raw_parts(),
            parts
        );
        assert_eq!(stored.to_string_bytes(), b"1.25");
        assert_eq!(stored.to_result_string_bytes(), b"1.3");
        assert!(format!("{stored:?}").starts_with("MyDecimal {"));
        let mut destination = MyDecimal::from_int(98765);
        assert_eq!(stored.round(&mut destination, 1, RoundMode::HalfUp), None);
        assert_eq!(destination.to_string_bytes(), b"1.3");
        let mut in_place = stored;
        assert_eq!(in_place.round_in_place(1, RoundMode::HalfUp), None);
        assert_eq!(in_place.to_string_bytes(), b"1.3");
        // Source encoding: precision 3 / fraction 2; signed integer 1,
        // fractional byte 25, then the significant fraction-count suffix.
        assert_eq!(
            MyDecimal::from_string(b"1.2500")
                .0
                .to_hash_key()
                .unwrap()
                .as_slice(),
            &[0x81, 0x19, 0x02]
        );
        let (maximum, error) = MyDecimal::from_string(format!("{}.1", "9".repeat(81)).as_bytes());
        assert_eq!(error, Some(DecimalError::Truncated));
        assert_eq!(maximum.to_string_bytes(), "9".repeat(81).as_bytes());
        let mut shifted = MyDecimal::from_string(b"12.3").0;
        assert_eq!(shifted.shift(1), None);
        assert_eq!(shifted.to_string_bytes(), b"123");
    }
}
