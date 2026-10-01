// Copyright 2016 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    cmp,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    mem,
    ops::{Add, Deref, DerefMut, Div, Mul, Neg, Rem, Sub},
    str::{self, FromStr},
};

use codec::prelude::*;
use smallvec::SmallVec;
use tikv_util::escape;

use crate::{
    codec::{
        Error, Result, TEN_POW,
        convert::{ConvertTo, ToStringValue},
        data_type::*,
        mysql::DEFAULT_DIV_FRAC_INCR,
    },
    expr::EvalContext,
};

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Res<T> {
    Ok(T),
    Truncated(T),
    Overflow(T),
}

impl<T> Res<T> {
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> Res<U> {
        match self {
            Res::Ok(t) => Res::Ok(f(t)),
            Res::Truncated(t) => Res::Truncated(f(t)),
            Res::Overflow(t) => Res::Overflow(f(t)),
        }
    }

    pub fn unwrap(self) -> T {
        match self {
            Res::Ok(t) | Res::Truncated(t) | Res::Overflow(t) => t,
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(*self, Res::Ok(_))
    }

    pub fn is_overflow(&self) -> bool {
        matches!(*self, Res::Overflow(_))
    }

    pub fn is_truncated(&self) -> bool {
        matches!(*self, Res::Truncated(_))
    }

    /// Convert `Res` into `Result` with an `EvalContext` that handling the
    /// errors If `truncated_err` is None, `ctx` will try to handle the
    /// default truncated error: `Error::truncated()`, otherwise handle the
    /// specified error inside `truncated_err`. Same does `overflow_err`
    /// means.
    fn into_result_impl(
        self,
        ctx: &mut EvalContext,
        truncated_err: Option<Error>,
        overflow_err: Option<Error>,
    ) -> Result<T> {
        self.into_result_with_error_factory(ctx, truncated_err, || {
            overflow_err.unwrap_or_else(|| Error::overflow("DECIMAL", ""))
        })
    }

    fn into_result_with_error_factory<F: FnOnce() -> Error>(
        self,
        ctx: &mut EvalContext,
        truncated_err: Option<Error>,
        overflow_err: F,
    ) -> Result<T> {
        match self {
            Res::Ok(t) => Ok(t),
            Res::Truncated(t) => if let Some(error) = truncated_err {
                ctx.handle_truncate_err(error)
            } else {
                ctx.handle_truncate(true)
            }
            .map(|()| t),
            Res::Overflow(t) => ctx.handle_overflow_err(overflow_err()).map(|()| t),
        }
    }

    /// Construct the supplied overflow error only for `Res::Overflow`.
    /// Ok/Truncated retain the same payload/context handling without invoking
    /// this factory, even when truncation itself becomes an error.
    pub fn into_result_with_overflow_err_lazy<F: FnOnce() -> Error>(
        self,
        ctx: &mut EvalContext,
        overflow_err: F,
    ) -> Result<T> {
        self.into_result_with_error_factory(ctx, None, overflow_err)
    }

    pub fn into_result_with_overflow_err(
        self,
        ctx: &mut EvalContext,
        overflow_err: Error,
    ) -> Result<T> {
        self.into_result_impl(ctx, None, Some(overflow_err))
    }

    pub fn into_result(self, ctx: &mut EvalContext) -> Result<T> {
        self.into_result_impl(ctx, None, None)
    }
}

impl<T> From<Res<T>> for Result<T> {
    fn from(r: Res<T>) -> Result<T> {
        match r {
            Res::Ok(t) => Ok(t),
            Res::Truncated(_) => Err(Error::truncated()),
            Res::Overflow(_) => Err(Error::overflow("", "")),
        }
    }
}

impl<T> Deref for Res<T> {
    type Target = T;

    fn deref(&self) -> &T {
        match *self {
            Res::Ok(ref t) | Res::Overflow(ref t) | Res::Truncated(ref t) => t,
        }
    }
}

impl<T> DerefMut for Res<T> {
    fn deref_mut(&mut self) -> &mut T {
        match *self {
            Res::Ok(ref mut t) | Res::Overflow(ref mut t) | Res::Truncated(ref mut t) => t,
        }
    }
}

// The existing arithmetic policy and physical cell retain a nine-word limit.
// This is not the capacity of the owning logical representation.
const WORD_BUF_LEN: usize = 9;
// A word holds 9 digits.
const DIGITS_PER_WORD: usize = 9;
// A word is 4 bytes i32.
const WORD_SIZE: usize = 4;
const DIG_MASK: u32 = TEN_POW[8];
const WORD_BASE: u32 = TEN_POW[9];
const WORD_MAX: u32 = WORD_BASE - 1;
const MAX_FRACTION: usize = 30;
const DIG_2_BYTES: &[usize] = &[0, 1, 1, 2, 2, 3, 3, 4, 4, 4];
const FRAC_MAX: &[u32] = &[
    900000000, 990000000, 999000000, 999900000, 999990000, 999999000, 999999900, 999999990,
];
const NOT_FIXED_DEC: usize = 31;

/// SQL parsing disposition, separate from resource/layout/count failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecimalParseStatus {
    Ok,
    Truncated,
    Overflow,
    BadNumber,
    TruncatedWrongValue,
}

#[derive(Clone, Debug)]
pub struct DecimalParseOutcome {
    pub value: Decimal,
    pub status: DecimalParseStatus,
}

/// Lexical and ordered-disposition contracts over one scanner/word packer.
/// Canonical admits only a complete decimal lexeme, never an exponent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecimalParsePolicy {
    Legacy(usize),
    Mysql(usize),
    Canonical,
}

impl DecimalParsePolicy {
    fn limit(self) -> WordLimit {
        match self {
            Self::Legacy(words) | Self::Mysql(words) => WordLimit::Fixed(words),
            Self::Canonical => WordLimit::Grow,
        }
    }

    fn legacy(self) -> bool {
        matches!(self, Self::Legacy(_))
    }
}

/// Only the reset/rounding/disposition choices differ; shifts use the same
/// word-alignment and move loops. This is not stored on a Decimal value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShiftDisposition {
    Legacy,
    Mysql,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShiftStatusOrigin {
    Direct,
    Rounding,
}

struct DecimalShiftOutcome {
    result: Res<Decimal>,
    origin: ShiftStatusOrigin,
}

impl DecimalShiftOutcome {
    fn direct(result: Res<Decimal>) -> Self {
        Self {
            result,
            origin: ShiftStatusOrigin::Direct,
        }
    }
}

fn first_utf8_char(bytes: &[u8]) -> Option<(char, usize)> {
    let width = match *bytes.first()? {
        0..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    let prefix = str::from_utf8(bytes.get(..width)?).ok()?;
    Some((prefix.chars().next()?, width))
}

/// Go strings.TrimSpace semantics at the edges without requiring a complete
/// UTF-8 input. An invalid byte is junk, not a codec decoding failure.
fn trim_unicode_space(mut bytes: &[u8]) -> &[u8] {
    while let Some((character, width)) = first_utf8_char(bytes) {
        if !character.is_whitespace() {
            break;
        }
        bytes = &bytes[width..];
    }
    while !bytes.is_empty() {
        let mut start = bytes.len() - 1;
        while start > 0 && bytes[start] & 0xc0 == 0x80 && bytes.len() - start < 4 {
            start -= 1;
        }
        let Some((character, width)) = first_utf8_char(&bytes[start..]) else {
            break;
        };
        if width != bytes.len() - start || !character.is_whitespace() {
            break;
        }
        bytes = &bytes[..start];
    }
    bytes
}

struct DecimalExponent {
    value: i64,
    status: DecimalParseStatus,
}

/// One exponent digit scan. Source range validation also runs after junk;
/// legacy suppresses junk warnings but retains its outer BIGINT range error.
fn scan_decimal_exponent(bytes: &[u8], legacy: bool) -> Result<DecimalExponent> {
    let bytes = if legacy {
        let start = bytes
            .iter()
            .position(|byte| *byte != b' ' && *byte != b'\t')
            .unwrap_or(bytes.len());
        &bytes[start..]
    } else {
        trim_unicode_space(bytes)
    };
    let (negative, start) = match bytes.first() {
        Some(b'-') => (true, 1),
        Some(b'+') => (false, 1),
        _ => (false, 0),
    };
    let mut magnitude = 0_u64;
    let mut has_digit = false;
    let mut status = DecimalParseStatus::Ok;
    for byte in &bytes[start..] {
        if !byte.is_ascii_digit() {
            status = DecimalParseStatus::Truncated;
            break;
        }
        has_digit = true;
        let Some(next) = magnitude
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(byte - b'0')))
        else {
            if legacy {
                return Err(Error::overflow("BIGINT", ""));
            }
            magnitude = 0;
            status = DecimalParseStatus::BadNumber;
            break;
        };
        magnitude = next;
        if legacy && magnitude > (i64::MAX as u64 + u64::from(negative)) {
            return Err(Error::overflow("BIGINT", ""));
        }
    }
    if !has_digit {
        status = DecimalParseStatus::Truncated;
    }
    let value = if !negative && magnitude > i64::MAX as u64 {
        status = DecimalParseStatus::BadNumber;
        i64::MAX
    } else if negative && magnitude > i64::MAX as u64 + 1 {
        status = DecimalParseStatus::BadNumber;
        i64::MIN
    } else if negative && magnitude == i64::MAX as u64 + 1 {
        i64::MIN
    } else if negative {
        -(magnitude as i64)
    } else {
        magnitude as i64
    };
    if legacy {
        status = DecimalParseStatus::Ok;
    }
    Ok(DecimalExponent { value, status })
}

/// Capacity policy for the one set of word workers. This is independent of
/// SQL declaration limits and of a result's visible fraction count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WordLimit {
    Grow,
    Fixed(usize),
}

impl WordLimit {
    fn apply(self, int_words: usize, frac_words: usize) -> Result<Res<(usize, usize)>> {
        match self {
            Self::Fixed(words) => Ok(fix_word_cnt_err(int_words, frac_words, words)),
            Self::Grow => {
                checked_word_extent(int_words, frac_words)?;
                Ok(Res::Ok((int_words, frac_words)))
            }
        }
    }
}

/// A bounded stack buffer shared by storage and result formatting. A common
/// inline decimal is emitted in one write; arbitrarily long zero tails use
/// this same fixed space and stop as soon as the destination reports failure.
struct DecimalTextWriter<'a> {
    out: &'a mut dyn fmt::Write,
    bytes: [u8; 128],
    len: usize,
}

impl<'a> DecimalTextWriter<'a> {
    fn new(out: &'a mut dyn fmt::Write) -> Self {
        Self {
            out,
            bytes: [0; 128],
            len: 0,
        }
    }

    fn flush(&mut self) -> fmt::Result {
        if self.len != 0 {
            let text = str::from_utf8(&self.bytes[..self.len]).map_err(|_| fmt::Error)?;
            self.out.write_str(text)?;
            self.len = 0;
        }
        Ok(())
    }

    fn byte(&mut self, byte: u8) -> fmt::Result {
        if self.len == self.bytes.len() {
            self.flush()?;
        }
        self.bytes[self.len] = byte;
        self.len += 1;
        Ok(())
    }

    fn word(&mut self, word: u32, digits: usize, skip_low: usize) -> fmt::Result {
        let mut value = word / TEN_POW[skip_low];
        let mut bytes = [b'0'; DIGITS_PER_WORD];
        for byte in bytes[..digits].iter_mut().rev() {
            *byte += (value % 10) as u8;
            value /= 10;
        }
        for byte in &bytes[..digits] {
            self.byte(*byte)?;
        }
        Ok(())
    }

    fn zeroes(&mut self, mut count: usize) -> fmt::Result {
        while count > 0 {
            if self.len == self.bytes.len() {
                self.flush()?;
            }
            let written = count.min(self.bytes.len() - self.len);
            self.bytes[self.len..self.len + written].fill(b'0');
            self.len += written;
            count -= written;
        }
        Ok(())
    }

    fn finish(mut self) -> fmt::Result {
        self.flush()
    }
}

#[derive(Default)]
struct DecimalTextCounter {
    bytes: usize,
}

impl fmt::Write for DecimalTextCounter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.bytes = self.bytes.checked_add(text.len()).ok_or(fmt::Error)?;
        Ok(())
    }
}

fn decimal_resource_error(detail: &str) -> Error {
    Error::InvalidDataType(format!("decimal resource/count failure: {detail}"))
}

fn checked_word_extent(int_words: usize, frac_words: usize) -> Result<usize> {
    let words = int_words
        .checked_add(frac_words)
        .ok_or_else(|| decimal_resource_error("active word count overflow"))?;
    words
        .max(WORD_BUF_LEN)
        .checked_mul(mem::size_of::<u32>())
        .filter(|bytes| *bytes <= isize::MAX as usize)
        .ok_or_else(|| decimal_resource_error("word allocation size overflow"))?;
    Ok(words)
}

fn checked_word_digits(words: usize) -> Result<usize> {
    words
        .checked_mul(DIGITS_PER_WORD)
        .ok_or_else(|| decimal_resource_error("word-aligned digit count overflow"))
}

fn checked_decimal_position(position: i128) -> Result<usize> {
    usize::try_from(position)
        .map_err(|_| decimal_resource_error("digit/word position exceeds indexing domain"))
}

fn checked_fraction(count: usize) -> Result<usize> {
    u32::try_from(count)
        .map(|_| count)
        .map_err(|_| decimal_resource_error("fraction count exceeds u32"))
}

macro_rules! word_cnt {
    ($len:expr) => {
        word_cnt!($len, usize)
    };
    ($len:expr, $t:ty) => {{
        if $len > 0 && $len as usize > (DIGITS_PER_WORD * WORD_BUF_LEN) as usize {
            // process overflow
            (WORD_BUF_LEN + 1) as $t
        } else if $len <= 0 && ($len as $t) > 0 {
            // when $len is negative and $t is unsigned
            0 as $t
        } else {
            // feature `int_roundings` is not stable.
            #[allow(clippy::manual_div_ceil)]
            {
                ($len as $t + DIGITS_PER_WORD as $t - 1) / (DIGITS_PER_WORD as $t)
            }
        }
    }};
}

/// Return the first encoded decimal's length.
pub fn dec_encoded_len(encoded: &[u8]) -> Result<usize> {
    if encoded.len() < 2 {
        return Err(box_err!("decimal too short: {} < 2", encoded.len()));
    }

    let precision = usize::from(encoded[0]);
    let frac_cnt = usize::from(encoded[1]);
    if precision < frac_cnt {
        return Err(box_err!(
            "invalid decimal, precision {} < frac_cnt {}",
            precision,
            frac_cnt
        ));
    }
    let int_cnt = precision - frac_cnt;
    let int_word_cnt = int_cnt / DIGITS_PER_WORD;
    let frac_word_cnt = frac_cnt / DIGITS_PER_WORD;
    let int_left = (int_cnt - int_word_cnt * DIGITS_PER_WORD) as usize;
    let frac_left = (frac_cnt - frac_word_cnt * DIGITS_PER_WORD) as usize;
    let int_len = (int_word_cnt * WORD_SIZE + DIG_2_BYTES[int_left]) as usize;
    let frac_len = (frac_word_cnt * WORD_SIZE + DIG_2_BYTES[frac_left]) as usize;
    Ok(int_len + frac_len + 2)
}

/// `count_leading_zeroes` returns the number of leading zeroes that can be
/// removed from int.
fn count_leading_zeroes(i: usize, word: u32) -> usize {
    let (mut c, mut i) = (0, i as usize);
    while TEN_POW[i] > word {
        i -= 1;
        c += 1;
    }
    c
}

/// `count_trailing_zeroes` returns the number of trailing zeroes that can be
/// removed from fraction.
fn count_trailing_zeroes(i: usize, word: u32) -> usize {
    let (mut c, mut i) = (0, i as usize);
    while word.is_multiple_of(TEN_POW[i]) {
        i += 1;
        c += 1;
    }
    c
}

/// `add` adds a and b and carry, stores the sum and new carry.
fn add(a: u32, b: u32, carry: &mut u32, res: &mut u32) {
    let sum = a + b + *carry;
    if sum >= WORD_BASE {
        *res = sum - WORD_BASE;
        *carry = 1;
    } else {
        *res = sum;
        *carry = 0;
    }
}

/// `fix_word_cnt_err` limits word count in `word_buf_len`.
fn fix_word_cnt_err(
    int_word_cnt: usize,
    frac_word_cnt: usize,
    word_buf_len: usize,
) -> Res<(usize, usize)> {
    if int_word_cnt > word_buf_len {
        return Res::Overflow((word_buf_len, 0));
    }
    if frac_word_cnt > word_buf_len - int_word_cnt {
        return Res::Truncated((int_word_cnt, word_buf_len - int_word_cnt));
    }
    Res::Ok((int_word_cnt, frac_word_cnt))
}

/// `sub` subtracts rhs and carry from lhs, store the diff and new carry.
fn sub(lhs: u32, rhs: u32, carry: &mut i32, res: &mut u32) {
    let diff = lhs as i32 - rhs as i32 - *carry;
    if diff < 0 {
        *carry = 1;
        *res = (diff + WORD_BASE as i32) as u32;
    } else {
        *carry = 0;
        *res = diff as u32;
    }
}

/// `sub2` subtracts rhs and carry from lhs, stores the diff and new carry.
/// the new carry may be 2.
fn sub2(lhs: u32, rhs: u32, carry: &mut i32, res: &mut u32) {
    let mut diff = lhs as i32 - rhs as i32 - *carry;
    if diff < -(WORD_BASE as i32) {
        *carry = 2;
        diff += WORD_BASE as i32 + WORD_BASE as i32;
    } else if diff < 0 {
        *carry = 1;
        diff += WORD_BASE as i32;
    } else {
        *carry = 0;
    }
    *res = diff as u32;
}

type SubTmp = (usize, usize, usize);

/// calculate the carry for lhs - rhs, returns the carry and needed temporary
/// results for beginning a subtraction.
///
/// The new carry can be:
///     1. None if lhs is equals to rhs.
///     2. Some(0) if abs(lhs) > abs(rhs),
///     3. Some(1) if abs(lhs) < abs(rhs).
/// l_frac_word_cnt and r_frac_word_cnt do not contain the suffix 0 when
/// r_int_word_cnt == l_int_word_cnt.
#[inline]
fn calc_sub_carry(lhs: &Decimal, rhs: &Decimal) -> (Option<i32>, usize, SubTmp, SubTmp) {
    let (l_int_word_cnt, mut l_frac_word_cnt) = (lhs.int_words(), lhs.frac_words());
    let (r_int_word_cnt, mut r_frac_word_cnt) = (rhs.int_words(), rhs.frac_words());
    let frac_word_to = cmp::max(l_frac_word_cnt, r_frac_word_cnt);

    let (l_stop, mut l_idx) = (l_int_word_cnt as usize, 0usize);
    while l_idx < l_stop && lhs.word_buf[l_idx] == 0 {
        l_idx += 1;
    }
    let l_start = l_idx;
    let l_int_word_cnt = l_stop - l_idx;

    let (r_stop, mut r_idx) = (r_int_word_cnt as usize, 0usize);
    while r_idx < r_stop && rhs.word_buf[r_idx] == 0 {
        r_idx += 1;
    }
    let r_start = r_idx;
    let r_int_word_cnt = r_stop - r_idx;

    let carry = match r_int_word_cnt.cmp(&l_int_word_cnt) {
        Ordering::Greater => Some(1),
        Ordering::Equal => {
            let mut l_end = (l_stop + l_frac_word_cnt) as isize - 1;
            let mut r_end = (r_stop + r_frac_word_cnt) as isize - 1;
            // trims suffix 0(also trims the suffix 0 before the point
            // when there is no digit after point).
            while l_idx as isize <= l_end && lhs.word_buf[l_end as usize] == 0 {
                l_end -= 1;
            }

            // trims suffix 0(also trims the suffix 0 before the point
            // when there is no digit after point).
            while r_idx as isize <= r_end && rhs.word_buf[r_end as usize] == 0 {
                r_end -= 1;
            }
            // here l_end is the last nonzero index in l.word_buf, attention:it may in the
            // range of (0,l_int_word_cnt)
            l_frac_word_cnt = cmp::max(0, l_end + 1 - l_stop as isize) as usize;
            // here r_end is the last nonzero index in r.word_buf, attention:it may in the
            // range of (0,r_int_word_cnt)
            r_frac_word_cnt = cmp::max(0, r_end + 1 - r_stop as isize) as usize;
            while l_idx as isize <= l_end
                && r_idx as isize <= r_end
                && lhs.word_buf[l_idx] == rhs.word_buf[r_idx]
            {
                l_idx += 1;
                r_idx += 1;
            }
            if l_idx as isize <= l_end {
                if r_idx as isize <= r_end && rhs.word_buf[r_idx] > lhs.word_buf[l_idx] {
                    Some(1)
                } else {
                    Some(0)
                }
            } else if r_idx as isize <= r_end {
                Some(1)
            } else {
                None
            }
        }
        Ordering::Less => Some(0),
    };
    let l_res = (l_start, l_int_word_cnt, l_frac_word_cnt);
    let r_res = (r_start, r_int_word_cnt, r_frac_word_cnt);
    (carry, frac_word_to, l_res, r_res)
}

/// Subtract rhs from lhs when lhs.negative=rhs.negative.
fn do_sub(lhs: &Decimal, rhs: &Decimal) -> Res<Decimal> {
    do_sub_with_limit(lhs, rhs, WordLimit::Fixed(WORD_BUF_LEN))
        .expect("bounded Decimal subtraction allocation failed")
}

fn do_sub_with_limit<'a>(
    mut lhs: &'a Decimal,
    mut rhs: &'a Decimal,
    limit: WordLimit,
) -> Result<Res<Decimal>> {
    let (carry, mut frac_word_to, l_res, r_res) = calc_sub_carry(lhs, rhs);
    if carry.is_none() {
        let value = if limit == WordLimit::Grow {
            let frac = cmp::max(lhs.frac_cnt, rhs.frac_cnt);
            Decimal::try_new(usize::from(frac == 0), frac, false)?
        } else {
            Decimal::zero()
        };
        return Ok(Res::Ok(value));
    }
    let (mut l_start, mut l_int_word_cnt, mut l_frac_word_cnt) = l_res;
    let (mut r_start, mut r_int_word_cnt, mut r_frac_word_cnt) = r_res;

    // determine the res.negative and make the abs(lhs) > abs(rhs).
    let negative = if carry.unwrap() > 0 {
        mem::swap(&mut lhs, &mut rhs);
        mem::swap(&mut l_start, &mut r_start);
        mem::swap(&mut l_int_word_cnt, &mut r_int_word_cnt);
        mem::swap(&mut l_frac_word_cnt, &mut r_frac_word_cnt);
        !rhs.negative
    } else {
        lhs.negative
    };

    let res = limit.apply(l_int_word_cnt, frac_word_to)?;
    l_int_word_cnt = res.0;
    frac_word_to = res.1;
    let mut idx_to = checked_word_extent(l_int_word_cnt, frac_word_to)?;
    let mut frac_cnt = cmp::max(lhs.frac_cnt, rhs.frac_cnt);
    let int_cnt = checked_word_digits(l_int_word_cnt)?;
    if !res.is_ok() {
        frac_cnt = cmp::min(frac_cnt, frac_word_to * DIGITS_PER_WORD);
        l_frac_word_cnt = cmp::min(l_frac_word_cnt, frac_word_to);
        r_frac_word_cnt = cmp::min(r_frac_word_cnt, frac_word_to);
        r_int_word_cnt = cmp::min(r_int_word_cnt, l_int_word_cnt);
    }
    let mut carry = 0;
    let value = Decimal::try_new(int_cnt, frac_cnt, negative)?;
    let mut res = res.map(|_| value);
    let mut l_idx = l_start + l_int_word_cnt + l_frac_word_cnt as usize;
    let mut r_idx = r_start + r_int_word_cnt + r_frac_word_cnt as usize;
    // adjust `l_idx` and `r_idx` to the same position of digits after the point.
    if l_frac_word_cnt > r_frac_word_cnt {
        let l_stop = l_start + l_int_word_cnt + r_frac_word_cnt as usize;
        if l_frac_word_cnt < frac_word_to {
            // It happens only when suffix 0 exist(3.10000000000-2.00).
            idx_to -= (frac_word_to - l_frac_word_cnt) as usize;
        }
        while l_idx > l_stop {
            idx_to -= 1;
            l_idx -= 1;
            res.word_buf[idx_to] = lhs.word_buf[l_idx];
        }
    } else {
        let r_stop = r_start + r_int_word_cnt + l_frac_word_cnt as usize;
        if frac_word_to > r_frac_word_cnt {
            // It happens only when suffix 0 exist(3.00-2.00000000000).
            idx_to -= (frac_word_to - r_frac_word_cnt) as usize;
        }
        while r_idx > r_stop {
            idx_to -= 1;
            r_idx -= 1;
            sub(
                0,
                rhs.word_buf[r_idx],
                &mut carry,
                &mut res.word_buf[idx_to],
            );
        }
    }

    while r_idx > r_start {
        idx_to -= 1;
        l_idx -= 1;
        r_idx -= 1;
        sub(
            lhs.word_buf[l_idx],
            rhs.word_buf[r_idx],
            &mut carry,
            &mut res.word_buf[idx_to],
        );
    }

    while carry > 0 && l_idx > l_start {
        idx_to -= 1;
        l_idx -= 1;
        sub(
            lhs.word_buf[l_idx],
            0,
            &mut carry,
            &mut res.word_buf[idx_to],
        );
    }
    while l_idx > l_start {
        idx_to -= 1;
        l_idx -= 1;
        res.word_buf[idx_to] = lhs.word_buf[l_idx];
    }
    res.try_ensure_storage()?;
    Ok(res)
}

fn checked_fixed_decimal_target(prec: u8, frac: u8) -> Result<(usize, usize)> {
    if prec < frac {
        return Err(Error::m_bigger_than_d(""));
    }
    let int_digits = usize::from(prec - frac);
    let frac_digits = usize::from(frac);
    let words = checked_word_extent(
        int_digits.div_ceil(DIGITS_PER_WORD),
        frac_digits.div_ceil(DIGITS_PER_WORD),
    )?;
    if words > WORD_BUF_LEN {
        return Err(Error::InvalidDataType(format!(
            "decimal target ({prec},{frac}) exceeds the nine-word layout"
        )));
    }
    Ok((int_digits, frac_digits))
}

/// Get the maximum decimal for the given bounded precision/fraction shape.
///
/// # Panics
///
/// Panics unless `prec >= frac_cnt` AND the separately rounded-up integer and
/// fraction word counts sum to at most nine. This is not a SQL65/30 validator.
pub fn max_decimal(prec: u8, frac_cnt: u8) -> Decimal {
    try_max_decimal(prec, frac_cnt)
        .expect("max_decimal requires ordered counts and the existing nine-word layout")
}

/// Checked callers and legacy wrappers share this one fill loop.
fn try_max_decimal(prec: u8, frac_cnt: u8) -> Result<Decimal> {
    let (int_cnt, frac_cnt) = checked_fixed_decimal_target(prec, frac_cnt)?;
    let mut res = Decimal::try_new(int_cnt, frac_cnt, false)?;
    let mut idx = 0;
    if int_cnt > 0 {
        let first_word_cnt = int_cnt % DIGITS_PER_WORD;
        if first_word_cnt > 0 {
            res.word_buf[idx] = TEN_POW[first_word_cnt as usize] - 1;
            idx += 1;
        }
        for _ in 0..int_cnt / DIGITS_PER_WORD {
            res.word_buf[idx] = WORD_MAX;
            idx += 1;
        }
    }
    if frac_cnt > 0 {
        let last_digits = frac_cnt % DIGITS_PER_WORD;
        for _ in 0..frac_cnt / DIGITS_PER_WORD {
            res.word_buf[idx] = WORD_MAX;
            idx += 1;
        }
        if last_digits > 0 {
            res.word_buf[idx] = FRAC_MAX[last_digits as usize - 1];
        }
    }
    Ok(res)
}

/// `max_or_min_dec`(`NewMaxOrMinDec` in tidb) returns the max or min
/// value decimal for given precision and fraction.
/// The precision/fraction pair must fit the bounded layout.
///
/// # Panics
///
/// Panics unless `prec >= frac` AND the separately rounded-up integer and
/// fraction word counts sum to at most nine, as for `max_decimal`.
pub fn max_or_min_dec(negative: bool, prec: u8, frac: u8) -> Decimal {
    let mut ret = max_decimal(prec, frac);
    ret.negative = negative;
    ret
}

/// Add lhs to rhs.
fn do_add(lhs: &Decimal, rhs: &Decimal) -> Res<Decimal> {
    do_add_with_limit(lhs, rhs, WordLimit::Fixed(WORD_BUF_LEN))
        .expect("bounded Decimal addition allocation failed")
}

fn addition_int_word_count(lhs: &Decimal, rhs: &Decimal) -> Result<usize> {
    let left_words = lhs.int_words();
    let right_words = rhs.int_words();
    let head = |value: &Decimal| {
        if value.int_words() + value.frac_words() == 0 {
            0
        } else {
            value.word_buf[0]
        }
    };
    let leading = match left_words.cmp(&right_words) {
        Ordering::Greater => head(lhs),
        Ordering::Less => head(rhs),
        Ordering::Equal => head(lhs) + head(rhs),
    };
    left_words
        .max(right_words)
        .checked_add(usize::from(leading > WORD_MAX - 1))
        .ok_or_else(|| decimal_resource_error("addition carry extent overflow"))
}

fn do_add_with_limit<'a>(
    mut lhs: &'a Decimal,
    mut rhs: &'a Decimal,
    limit: WordLimit,
) -> Result<Res<Decimal>> {
    let (mut l_int_word_cnt, mut l_frac_word_cnt) = (lhs.int_words(), lhs.frac_words());
    let (mut r_int_word_cnt, mut r_frac_word_cnt) = (rhs.int_words(), rhs.frac_words());
    let (int_word_to, frac_word_to) = (
        addition_int_word_count(lhs, rhs)?,
        cmp::max(l_frac_word_cnt, r_frac_word_cnt),
    );
    let res = limit.apply(int_word_to, frac_word_to)?;
    if res.is_overflow() {
        let mut max = Decimal::try_new(checked_word_digits(res.0)?, 0, false)?;
        max.word_buf[..res.0].fill(WORD_MAX);
        return Ok(Res::Overflow(max));
    }
    let (int_word_to, frac_word_to) = res.unwrap();
    let mut idx_to = checked_word_extent(int_word_to, frac_word_to)?;
    let source_frac = cmp::max(lhs.frac_cnt, rhs.frac_cnt);
    let stored_frac = if res.is_ok() {
        source_frac
    } else {
        cmp::min(checked_word_digits(frac_word_to)?, source_frac)
    };
    // Fixed preselection precedes allocation, rather than constructing a wide
    // exact result and clipping it afterward.
    let mut value = Decimal::try_new(checked_word_digits(int_word_to)?, stored_frac, lhs.negative)?;
    value.result_frac_cnt = source_frac;
    let mut res = res.map(|_| value);
    res.word_buf[0] = 0;
    if !res.is_ok() {
        res.frac_cnt = cmp::min(frac_word_to * DIGITS_PER_WORD, res.frac_cnt);
        l_frac_word_cnt = cmp::min(frac_word_to, l_frac_word_cnt);
        r_frac_word_cnt = cmp::min(r_frac_word_cnt, frac_word_to);
        l_int_word_cnt = cmp::min(l_int_word_cnt, int_word_to);
        r_int_word_cnt = cmp::min(r_int_word_cnt, int_word_to);
    }
    let (mut l_idx, mut r_idx, l_stop, r_stop, exchanged);
    if l_frac_word_cnt > r_frac_word_cnt {
        l_idx = (l_int_word_cnt + l_frac_word_cnt) as usize;
        l_stop = (l_int_word_cnt + r_frac_word_cnt) as usize;
        r_idx = (r_int_word_cnt + r_frac_word_cnt) as usize;
        r_stop = l_int_word_cnt.saturating_sub(r_int_word_cnt) as usize;
        exchanged = false;
    } else {
        l_idx = (r_int_word_cnt + r_frac_word_cnt) as usize;
        l_stop = (r_int_word_cnt + l_frac_word_cnt) as usize;
        r_idx = (l_int_word_cnt + l_frac_word_cnt) as usize;
        r_stop = r_int_word_cnt.saturating_sub(l_int_word_cnt) as usize;
        mem::swap(&mut lhs, &mut rhs);
        exchanged = true;
    }
    while l_idx > l_stop {
        idx_to -= 1;
        l_idx -= 1;
        res.word_buf[idx_to] = lhs.word_buf[l_idx];
    }
    let mut carry = 0;
    while l_idx > r_stop {
        l_idx -= 1;
        r_idx -= 1;
        idx_to -= 1;
        add(
            lhs.word_buf[l_idx],
            rhs.word_buf[r_idx],
            &mut carry,
            &mut res.word_buf[idx_to],
        );
    }
    let l_stop = 0;
    if l_int_word_cnt > r_int_word_cnt {
        l_idx = (l_int_word_cnt - r_int_word_cnt) as usize;
        if exchanged {
            mem::swap(&mut lhs, &mut rhs);
        }
    } else {
        l_idx = (r_int_word_cnt - l_int_word_cnt) as usize;
        if !exchanged {
            mem::swap(&mut lhs, &mut rhs);
        }
    }
    while l_idx > l_stop {
        idx_to -= 1;
        l_idx -= 1;
        add(
            lhs.word_buf[l_idx],
            0,
            &mut carry,
            &mut res.word_buf[idx_to],
        );
    }
    if carry > 0 {
        idx_to -= 1;
        res.word_buf[idx_to] = 1;
    }
    res.try_ensure_storage()?;
    Ok(res)
}

/// Precision requests on the same base-1e9 long-division loop. An integer
/// pair captures both guesses and the final scratch remainder in one pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DivisionRequest {
    MysqlQuotient { frac_incr: usize },
    RetainedQuotient { frac_words: usize },
    Remainder,
    IntegerPair,
}

impl DivisionRequest {
    fn quotient(self) -> bool {
        self != Self::Remainder
    }

    fn remainder(self) -> bool {
        matches!(self, Self::Remainder | Self::IntegerPair)
    }
}

struct DivisionOutput {
    quotient: Option<Res<Decimal>>,
    remainder: Option<Res<Decimal>>,
}

/// Legacy counter selection is not a numerical truth about an exact quotient.
/// In particular Fixed MOD historically stops at the n+1-word sentinel even
/// though it does not store the quotient. Grow must not inherit that cap.
fn division_word_count(digits: usize, limit: WordLimit) -> usize {
    let words = digits.div_ceil(DIGITS_PER_WORD);
    match limit {
        WordLimit::Grow => words,
        WordLimit::Fixed(words_limit) => words.min(words_limit.saturating_add(1)),
    }
}

fn try_zeroed_words(words: usize) -> Result<Vec<u32>> {
    checked_word_extent(words, 0)?;
    let mut result = Vec::new();
    result
        .try_reserve_exact(words)
        .map_err(|_| decimal_resource_error("division scratch allocation failed"))?;
    result.resize(words, 0);
    Ok(result)
}

fn do_div_mod_impl(
    lhs: &Decimal,
    rhs: &Decimal,
    frac_incr: usize,
    do_mod: bool,
    result_frac_cnt: Option<usize>,
) -> Option<Res<Decimal>> {
    let request = if do_mod {
        DivisionRequest::Remainder
    } else {
        DivisionRequest::MysqlQuotient { frac_incr }
    };
    divide_with_limit(
        lhs,
        rhs,
        request,
        WordLimit::Fixed(WORD_BUF_LEN),
        result_frac_cnt,
    )
    .expect("bounded Decimal division count or allocation failed")
    .map(|output| {
        if do_mod {
            output.remainder.expect("requested division remainder")
        } else {
            output.quotient.expect("requested division quotient")
        }
    })
}

fn divide_with_limit(
    lhs: &Decimal,
    rhs: &Decimal,
    request: DivisionRequest,
    limit: WordLimit,
    result_frac_cnt: Option<usize>,
) -> Result<Option<DivisionOutput>> {
    if request == DivisionRequest::IntegerPair && limit != WordLimit::Grow {
        return Err(Error::InvalidDataType(
            "full integer pair requires Grow capacity".to_owned(),
        ));
    }
    // Input extents are real initialized extents, not the legacy quotient
    // sentinel. Padded digit counts are intermediate counts, not u32 headers.
    let l_frac_words = lhs.frac_words();
    let r_frac_words = rhs.frac_words();
    let l_frac_cnt = checked_word_digits(l_frac_words)?;
    let r_frac_cnt = checked_word_digits(r_frac_words)?;
    let r_full = rhs
        .int_cnt
        .checked_add(r_frac_cnt)
        .ok_or_else(|| decimal_resource_error("divisor precision count overflow"))?;
    let (r_idx, r_prec) = rhs.remove_leading_zeroes(r_full);
    if r_prec == 0 {
        return Ok(None);
    }
    let l_full = lhs
        .int_cnt
        .checked_add(l_frac_cnt)
        .ok_or_else(|| decimal_resource_error("dividend precision count overflow"))?;
    let (l_start, l_prec) = lhs.remove_leading_zeroes(l_full);
    let remainder_scale = lhs.frac_cnt.max(rhs.frac_cnt);
    let remainder_visible = result_frac_cnt.unwrap_or(lhs.result_frac_cnt.max(rhs.result_frac_cnt));
    let requested_frac_words = match request {
        DivisionRequest::MysqlQuotient { frac_incr } => {
            let padding = (l_frac_cnt - lhs.frac_cnt) + (r_frac_cnt - rhs.frac_cnt);
            let increment = frac_incr.saturating_sub(padding);
            let digits = match limit {
                WordLimit::Fixed(_) => l_frac_cnt
                    .saturating_add(r_frac_cnt)
                    .saturating_add(increment),
                WordLimit::Grow => l_frac_cnt
                    .checked_add(r_frac_cnt)
                    .and_then(|digits| digits.checked_add(increment))
                    .ok_or_else(|| decimal_resource_error("division retained scale overflow"))?,
            };
            division_word_count(digits, limit)
        }
        DivisionRequest::RetainedQuotient { frac_words } => frac_words,
        DivisionRequest::Remainder | DivisionRequest::IntegerPair => 0,
    };
    if l_prec == 0 {
        // Legacy zero uses the requested visible scale; AVG and exact
        // remainder instead retain their independently planned storage scale.
        let quotient = if request.quotient() {
            let mut zero = match request {
                DivisionRequest::RetainedQuotient { .. } => {
                    Decimal::try_new(0, checked_word_digits(requested_frac_words)?, false)?
                }
                DivisionRequest::MysqlQuotient { .. } => {
                    if let Some(scale) = result_frac_cnt {
                        Decimal::try_new(0, scale, false)?
                    } else {
                        Decimal::zero()
                    }
                }
                DivisionRequest::IntegerPair => Decimal::zero(),
                DivisionRequest::Remainder => unreachable!(),
            };
            if let Some(scale) = result_frac_cnt {
                zero.result_frac_cnt = checked_fraction(scale)?;
            }
            Some(Res::Ok(zero))
        } else {
            None
        };
        let remainder = if request.remainder() {
            // Keep the entire legacy result-byte domain unchanged. A newly
            // wide visible scale is presentation metadata, not permission to
            // allocate that many stored zero digits in a Fixed operation.
            let retain_input_storage =
                limit == WordLimit::Grow || remainder_visible > usize::from(u8::MAX);
            let mut zero = if retain_input_storage {
                Decimal::try_new(0, remainder_scale, false)?
            } else if let Some(scale) = result_frac_cnt {
                Decimal::try_new(0, scale, false)?
            } else {
                Decimal::zero()
            };
            if retain_input_storage {
                zero.result_frac_cnt = checked_fraction(remainder_visible)?;
            }
            Some(Res::Ok(zero))
        } else {
            None
        };
        return Ok(Some(DivisionOutput {
            quotient,
            remainder,
        }));
    }

    // Signed digit displacement is not a base-1e9 carry and must not narrow
    // to i32. Fractional gaps use truncation toward zero, not ceil(abs/9).
    let displacement = (l_prec as i128 - l_frac_cnt as i128)
        - (r_prec as i128 - r_frac_cnt as i128)
        + i128::from(lhs.word_buf[l_start] >= rhs.word_buf[r_idx]);
    let int_digits = if displacement > 0 {
        usize::try_from(displacement)
            .map_err(|_| decimal_resource_error("quotient integer precision overflow"))?
    } else {
        0
    };
    let mut int_word_to = division_word_count(int_digits, limit);
    let mut frac_word_to = requested_frac_words;
    let mut quotient = if request.quotient() {
        let status = limit.apply(int_word_to, frac_word_to)?;
        (int_word_to, frac_word_to) = (status.0, status.1);
        let mut value = Decimal::try_new(
            checked_word_digits(int_word_to)?,
            checked_word_digits(frac_word_to)?,
            lhs.negative != rhs.negative,
        )?;
        if let Some(scale) = result_frac_cnt {
            value.result_frac_cnt = checked_fraction(scale)?;
        }
        Some(status.map(|_| value))
    } else {
        None
    };
    let end = checked_word_extent(int_word_to, frac_word_to)?;
    let start = if request.quotient() && displacement < 0 {
        let gap = usize::try_from(-displacement / DIGITS_PER_WORD as i128)
            .map_err(|_| decimal_resource_error("quotient fractional gap overflow"))?;
        match limit {
            WordLimit::Fixed(words) => gap.min(words),
            WordLimit::Grow => gap.min(end),
        }
    } else {
        0
    };

    let lhs_words = l_prec.div_ceil(DIGITS_PER_WORD);
    let rhs_words = r_prec.div_ceil(DIGITS_PER_WORD);
    let (r_start, mut r_stop) = (
        r_idx,
        r_idx
            .checked_add(rhs_words - 1)
            .ok_or_else(|| decimal_resource_error("divisor active extent overflow"))?,
    );
    while r_stop > r_start && rhs.word_buf[r_stop] == 0 {
        r_stop -= 1;
    }
    let r_len = r_stop - r_start;
    r_stop += 1;
    let head_skip = usize::from(lhs.word_buf[l_start] < rhs.word_buf[r_start]);
    let iterations = end.saturating_sub(start);
    let loop_words = if iterations == 0 {
        0
    } else {
        head_skip
            .checked_add(iterations)
            .and_then(|words| words.checked_add(r_len.max(1)))
            .ok_or_else(|| decimal_resource_error("division loop extent overflow"))?
    };
    let remainder_words = remainder_scale.div_ceil(DIGITS_PER_WORD);
    let remainder_stop = if request.remainder() {
        lhs_words
            .checked_add(remainder_words - l_frac_words)
            .ok_or_else(|| decimal_resource_error("remainder scratch extent overflow"))?
    } else {
        0
    };
    let scratch_words = 3.max(lhs_words).max(loop_words).max(remainder_stop);
    let mut buf = try_zeroed_words(scratch_words)?;
    let l_stop = l_start
        .checked_add(lhs_words)
        .ok_or_else(|| decimal_resource_error("dividend active extent overflow"))?;
    buf[..lhs_words].copy_from_slice(&lhs.word_buf[l_start..l_stop]);
    let mut l_idx = 0;

    let norm_factor = i64::from(WORD_BASE / (rhs.word_buf[r_start] + 1));
    let mut r_norm = norm_factor * i64::from(rhs.word_buf[r_start]);
    if r_len > 0 {
        r_norm += norm_factor * i64::from(rhs.word_buf[r_start + 1]) / i64::from(WORD_BASE);
    }
    let mut dcarry = 0;
    if buf[l_idx] < rhs.word_buf[r_start] {
        dcarry = buf[l_idx] as i32;
        l_idx += 1;
    }
    let mut guess;
    for idx_to in start..end {
        if dcarry == 0 && buf[l_idx] < rhs.word_buf[r_start] {
            guess = 0;
        } else {
            let x = i64::from(buf[l_idx]) + i64::from(dcarry) * i64::from(WORD_BASE);
            let y = i64::from(buf[l_idx + 1]);
            guess = (norm_factor * x + norm_factor * y / i64::from(WORD_BASE)) / r_norm;
            if guess >= i64::from(WORD_BASE) {
                guess = i64::from(WORD_BASE) - 1;
            }
            if r_len > 0 {
                if i64::from(rhs.word_buf[r_start + 1]) * guess
                    > (x - guess * i64::from(rhs.word_buf[r_start])) * i64::from(WORD_BASE) + y
                {
                    guess -= 1;
                }
                if i64::from(rhs.word_buf[r_start + 1]) * guess
                    > (x - guess * i64::from(rhs.word_buf[r_start])) * i64::from(WORD_BASE) + y
                {
                    guess -= 1;
                }
            }
            let mut carry = 0;
            for (r_idx, l_idx) in (r_start..r_stop).rev().zip((0..=l_idx + r_len).rev()) {
                let x = guess * i64::from(rhs.word_buf[r_idx]);
                let hi = x / i64::from(WORD_BASE);
                let lo = x - hi * i64::from(WORD_BASE);
                sub2(buf[l_idx], lo as u32, &mut carry, &mut buf[l_idx]);
                carry += hi as i32;
            }
            if dcarry < carry {
                carry = 1;
            } else {
                carry = 0;
            }
            if carry > 0 {
                guess -= 1;
                let mut carry = 0;
                for (r_idx, l_idx) in (r_start..r_stop).rev().zip((0..=l_idx + r_len).rev()) {
                    add(buf[l_idx], rhs.word_buf[r_idx], &mut carry, &mut buf[l_idx]);
                }
            }
        }
        if let Some(ref mut value) = quotient {
            value.word_buf[idx_to] = guess as u32;
        }
        dcarry = buf[l_idx] as i32;
        l_idx += 1;
    }
    let remainder = if request.remainder() {
        if dcarry != 0 {
            l_idx = l_idx
                .checked_sub(1)
                .ok_or_else(|| decimal_resource_error("remainder carry position underflow"))?;
            buf[l_idx] = dcarry as u32;
        }
        let plan = RemainderPlan {
            // ceil((l_prec - l_frac_cnt - 9*l_idx)/9), with signed
            // truncation toward zero for its negative branch, simplifies to
            // this word displacement. No narrowing or unsigned wrap cast.
            int_words: lhs_words as i128 - l_frac_words as i128 - l_idx as i128,
            storage_frac: remainder_scale,
            result_frac: if limit == WordLimit::Grow {
                remainder_visible
            } else {
                result_frac_cnt.unwrap_or(remainder_scale)
            },
            integer_bound: rhs.int_cnt,
            negative: lhs.negative,
            start: l_idx,
            stop: remainder_stop,
        };
        Some(finish_division_remainder(&buf, plan, limit)?)
    } else {
        None
    };
    if let Some(ref mut value) = quotient {
        value.try_ensure_storage()?;
        if value.is_zero() {
            value.negative = false;
        }
    }
    Ok(Some(DivisionOutput {
        quotient,
        remainder,
    }))
}

/// Shape/copy planning over the completed shared division scratch. This does
/// not recalculate a remainder from a quotient or run another numeric loop.
struct RemainderPlan {
    int_words: i128,
    storage_frac: usize,
    result_frac: usize,
    integer_bound: usize,
    negative: bool,
    start: usize,
    stop: usize,
}

fn finish_division_remainder(
    buf: &[u32],
    plan: RemainderPlan,
    limit: WordLimit,
) -> Result<Res<Decimal>> {
    let int_words = usize::try_from(plan.int_words.max(0))
        .map_err(|_| decimal_resource_error("remainder integer extent overflow"))?;
    let gap = usize::try_from((-plan.int_words).max(0))
        .map_err(|_| decimal_resource_error("remainder fractional gap overflow"))?;
    let frac_words = plan.storage_frac.div_ceil(DIGITS_PER_WORD);
    let mut storage_frac = plan.storage_frac;
    let mut stop = plan.stop;
    let mut status = Res::Ok(());
    if let WordLimit::Fixed(words_limit) = limit {
        // Preserve legacy early-return payloads, including their distinct
        // zero-scale/sign dispositions. Grow does not take these branches.
        if plan.int_words == 0 && frac_words == 0 {
            return Ok(Res::Ok(Decimal::zero()));
        }
        if plan.int_words <= 0 && gap >= words_limit {
            return Ok(Res::Truncated(Decimal::zero()));
        }
        if int_words > words_limit {
            let mut value = Decimal::try_new(checked_word_digits(words_limit)?, 0, plan.negative)?;
            value.result_frac_cnt = checked_fraction(plan.result_frac)?;
            return Ok(Res::Overflow(value));
        }
        if gap > frac_words {
            return Err(decimal_resource_error(
                "legacy remainder gap exceeds storage",
            ));
        }
        // Approved bounded output plan: leading fractional zero words occupy
        // cells too. Select the FULL integer+fraction extent before copying,
        // not merely the non-gap source tail. Canonical <=n inputs retain
        // their old path by the recorded extent bound.
        let output_words = checked_word_extent(int_words, frac_words)?;
        if output_words > words_limit {
            stop = stop
                .checked_sub(output_words - words_limit)
                .ok_or_else(|| {
                    decimal_resource_error("remainder selected copy extent underflow")
                })?;
            let selected_frac_words = words_limit - int_words;
            storage_frac = checked_word_digits(selected_frac_words)?;
            status = Res::Truncated(());
        }
    }
    let copy_len = stop
        .checked_sub(plan.start)
        .ok_or_else(|| decimal_resource_error("remainder source extent underflow"))?;
    let copy_end = gap
        .checked_add(copy_len)
        .ok_or_else(|| decimal_resource_error("remainder destination extent overflow"))?;
    if let WordLimit::Fixed(words_limit) = limit {
        if copy_end > words_limit {
            return Err(decimal_resource_error(
                "legacy Fixed remainder projection exceeds capacity",
            ));
        }
    }
    let int_digits = checked_word_digits(int_words)?.min(plan.integer_bound);
    // Allocate/initialize the complete destination BEFORE copying. Growing
    // an initially fraction-only destination after the copy is too late.
    let mut value = Decimal::try_new(int_digits, storage_frac, plan.negative)?;
    value.result_frac_cnt = checked_fraction(plan.result_frac)?;
    value.try_reserve_words(copy_end)?;
    let source = buf
        .get(plan.start..stop)
        .ok_or_else(|| decimal_resource_error("remainder source exceeds initialized scratch"))?;
    value.word_buf[gap..copy_end].copy_from_slice(source);
    value.try_ensure_storage()?;
    if value.is_zero() {
        value.negative = false;
    }
    Ok(status.map(|_| value))
}

#[allow(dead_code)]
fn do_div_mod(lhs: &Decimal, rhs: &Decimal, frac_incr: u8, do_mod: bool) -> Option<Res<Decimal>> {
    do_div_mod_impl(lhs, rhs, usize::from(frac_incr), do_mod, None)
}

/// `do_mul` multiplies two decimals.
fn do_mul(lhs: &Decimal, rhs: &Decimal) -> Res<Decimal> {
    do_mul_with_limit(lhs, rhs, WordLimit::Fixed(WORD_BUF_LEN))
        .expect("bounded Decimal multiplication allocation failed")
}

fn do_mul_with_limit(lhs: &Decimal, rhs: &Decimal, limit: WordLimit) -> Result<Res<Decimal>> {
    do_mul_with_policy(lhs, rhs, limit, false)
}

// The native MySQL wrapper uses the same multiplication loop but retains its
// original projected storage fraction and performs its own status finishing.
fn do_mul_with_policy(
    lhs: &Decimal,
    rhs: &Decimal,
    limit: WordLimit,
    native_mysql: bool,
) -> Result<Res<Decimal>> {
    let (l_int_word_cnt, mut l_frac_word_cnt) =
        (lhs.int_words() as isize, lhs.frac_words() as isize);
    let (mut r_int_word_cnt, mut r_frac_word_cnt) =
        (rhs.int_words() as isize, rhs.frac_words() as isize);
    let old_r_int_word_cnt = r_int_word_cnt;
    let int_digits = lhs
        .int_cnt
        .checked_add(rhs.int_cnt)
        .ok_or_else(|| decimal_resource_error("product integer digit count overflow"))?;
    let (int_word_to, frac_word_to) = (
        int_digits.div_ceil(DIGITS_PER_WORD),
        l_frac_word_cnt + r_frac_word_cnt,
    );
    let (mut old_int_word_to, mut old_frac_word_to) = (int_word_to as isize, frac_word_to);
    let res = limit.apply(int_word_to, frac_word_to as usize)?;
    let (int_word_to, frac_word_to) = (res.0, res.1);
    let negative = lhs.negative != rhs.negative;
    let (frac_cnt, result_frac_cnt) = match limit {
        WordLimit::Fixed(_) if native_mysql => (
            (lhs.frac_cnt + rhs.frac_cnt).min(frac_word_to * DIGITS_PER_WORD),
            (lhs.result_frac_cnt + rhs.result_frac_cnt).min(MAX_FRACTION),
        ),
        WordLimit::Fixed(_) => (
            (lhs.frac_cnt.min(NOT_FIXED_DEC) + rhs.frac_cnt.min(NOT_FIXED_DEC)).min(NOT_FIXED_DEC),
            (lhs.result_frac_cnt.min(MAX_FRACTION) + rhs.result_frac_cnt.min(MAX_FRACTION))
                .min(MAX_FRACTION),
        ),
        WordLimit::Grow => (
            checked_fraction(
                lhs.frac_cnt
                    .checked_add(rhs.frac_cnt)
                    .ok_or_else(|| decimal_resource_error("product storage scale overflow"))?,
            )?,
            checked_fraction(
                lhs.result_frac_cnt
                    .checked_add(rhs.result_frac_cnt)
                    .ok_or_else(|| decimal_resource_error("product result scale overflow"))?,
            )?,
        ),
    };
    let int_cnt = checked_word_digits(int_word_to)?;
    let mut dec = Decimal::try_new(int_cnt, frac_cnt, negative)?;
    dec.result_frac_cnt = result_frac_cnt;
    if res.is_overflow() {
        return Ok(Res::Overflow(dec));
    }
    // Separately aligned input fractions can need one scratch word beyond
    // the logical product's active extent. It is initialized before the loop.
    dec.try_reserve_words(checked_word_extent(int_word_to, frac_word_to)?)?;

    if !res.is_ok() {
        dec.frac_cnt = cmp::min(dec.frac_cnt, frac_word_to as usize * DIGITS_PER_WORD);
        if old_int_word_to > int_word_to as isize {
            old_int_word_to -= int_word_to as isize;
            old_frac_word_to = old_int_word_to / 2;
            r_int_word_cnt = old_int_word_to - old_frac_word_to;
            l_frac_word_cnt = 0;
            r_frac_word_cnt = 0;
        } else {
            old_frac_word_to -= frac_word_to as isize;
            old_int_word_to = old_frac_word_to / 2;
            if l_frac_word_cnt <= r_frac_word_cnt {
                l_frac_word_cnt -= old_int_word_to;
                r_frac_word_cnt -= old_frac_word_to - old_int_word_to;
            } else {
                r_frac_word_cnt -= old_int_word_to;
                l_frac_word_cnt -= old_frac_word_to - old_int_word_to;
            }
        }
    }

    let mut start_to = (int_word_to + frac_word_to) as isize - 1;
    let r_start = old_r_int_word_cnt + r_frac_word_cnt - 1;
    let r_stop = old_r_int_word_cnt - r_int_word_cnt;
    let mut l_idx = l_int_word_cnt + l_frac_word_cnt - 1;

    while l_idx >= 0 {
        let (mut carry, mut idx_to) = (0, start_to);
        let mut r_idx = r_start;
        while r_idx >= r_stop {
            let p =
                u64::from(lhs.word_buf[l_idx as usize]) * u64::from(rhs.word_buf[r_idx as usize]);
            let hi = p / u64::from(WORD_BASE);
            let lo = p - hi * u64::from(WORD_BASE);
            add(
                dec.word_buf[idx_to as usize],
                lo as u32,
                &mut carry,
                &mut dec.word_buf[idx_to as usize],
            );
            // A product column includes a full previous high-word carry, so
            // native/exact arithmetic can need two base reductions. The wire
            // Fixed path deliberately retains its original one-carry leaf.
            if (native_mysql || matches!(limit, WordLimit::Grow))
                && dec.word_buf[idx_to as usize] >= WORD_BASE
            {
                dec.word_buf[idx_to as usize] -= WORD_BASE;
                carry += 1;
            }
            carry += hi as u32;
            r_idx -= 1;
            idx_to -= 1;
        }
        while carry > 0 {
            if idx_to < 0 {
                dec.try_ensure_storage()?;
                return Ok(Res::Overflow(dec));
            }
            add(
                dec.word_buf[idx_to as usize],
                0,
                &mut carry,
                &mut dec.word_buf[idx_to as usize],
            );
            idx_to -= 1;
        }
        l_idx -= 1;
        start_to -= 1;
    }

    // Now we have to check for -0.000, including an empty physical zero.
    if dec.negative
        && dec.word_buf[..int_word_to + frac_word_to]
            .iter()
            .all(|word| *word == 0)
    {
        // A successful zero product keeps its storage/result scale.
        // Preserve the existing truncated payload convention; overflow
        // has already returned above and may intentionally contain -0.
        if res.is_ok() || native_mysql {
            dec.negative = false;
        } else {
            dec = Decimal::zero();
        }
    }

    let (mut idx_to, mut d_to_move) = (0, int_word_to + dec.frac_words());
    while dec.word_buf[idx_to] == 0 && dec.int_cnt > DIGITS_PER_WORD {
        idx_to += 1;
        dec.int_cnt -= DIGITS_PER_WORD;
        d_to_move -= 1;
    }
    if idx_to > 0 {
        for cur_idx in 0..d_to_move {
            dec.word_buf[cur_idx] = dec.word_buf[idx_to];
            idx_to += 1;
        }
    }
    dec.try_ensure_storage()?;
    Ok(res.map(|_| dec))
}

/// Size of the legacy physical Decimal chunk cell, not the owning Rust value.
pub const DECIMAL_STRUCT_SIZE: usize = 40;

/// Physical bytes only. No pointer or owning value is copied across this
/// layout.
struct DecimalCell([u8; DECIMAL_STRUCT_SIZE]);

const_assert_eq!(DECIMAL_STRUCT_SIZE, mem::size_of::<DecimalCell>());
const_assert_eq!(DECIMAL_STRUCT_SIZE, 4 + WORD_BUF_LEN * WORD_SIZE);

/// An owning logical decimal, independent of the physical forty-byte cell.
///
/// All counters use the indexing width. Constructors enforce the public u32
/// fraction-count domain; the current public arithmetic remains Fixed(9).
#[derive(Clone, Debug)]
pub struct Decimal {
    /// The number of stored decimal digits before the point.
    int_cnt: usize,
    /// Stored fraction digits, independent of the visible result scale.
    frac_cnt: usize,
    /// Calculated or printed result fraction digits.
    result_frac_cnt: usize,
    negative: bool,
    /// Initialized base-1e9 words, with nine inline cells. Status payloads may
    /// need additional cells even when arithmetic itself has a nine-word limit.
    word_buf: SmallVec<[u32; 9]>,
}

/// Borrowed exact logical fields, not a wire or FFI representation.
///
/// `words` includes initialized inactive cells, not uninitialized capacity.
/// General wide construction is not yet admitted by the fixed-worker APIs.
#[derive(Clone, Copy, Debug)]
pub struct DecimalWordsRef<'a> {
    pub int_digits: usize,
    pub storage_frac: u32,
    pub result_frac: u32,
    pub negative: bool,
    pub words: &'a [u32],
}

/// Logical, exact components of the bounded decimal core.
///
/// This is not a wire format or a raw-memory/FFI layout. Storage fraction and
/// displayed result fraction are independent; neither is a SQL column's
/// declared precision/scale. Use `Decimal::try_from_parts` before importing
/// components supplied by another representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecimalParts {
    /// Number of stored decimal digits before the decimal point.
    pub int_digits: u8,
    /// Number of stored decimal digits after the decimal point.
    pub frac_digits: u8,
    /// Number of calculated or printed result fraction digits.
    pub result_frac_digits: u8,
    /// Sign bit, including signed zero in an arithmetic status payload.
    pub negative: bool,
    /// Big-endian base-1e9 words; the counts determine the active prefix.
    /// Inactive capacity is preserved verbatim, not treated as extra digits.
    pub words: [u32; 9],
}

/// The exact native decimal math policies admitted by the closed value bridge.
/// These are not the wire signatures or a public arithmetic-capacity policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeDecimalOp {
    Abs,
    Negate,
    Ceil,
    Floor,
    Round(i32),
    Truncate(i32),
}

/// Binary operations shared by exact native values and native MySQL status
/// APIs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeDecimalBinaryOp {
    Add,
    Subtract,
    Multiply,
}

/// Native policies are separate from the original wire Fixed(9) signatures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeDecimalBinaryPolicy {
    Exact,
    MySql,
}

/// Signed coefficient shape used by the native column-wise fast outcome.
/// This is not the unsigned-coefficient eligibility policy of MySql multiply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeDecimalFastValue {
    pub coefficient: i128,
    pub storage_scale: u32,
    pub scale: u32,
}

/// Preserve the column fast path's checked-i128 domain. None means Unsupported,
/// not SQL NULL; subtraction first checks negation of the right coefficient.
pub fn native_decimal_fast_binary(
    left: NativeDecimalFastValue,
    mut right: NativeDecimalFastValue,
    operation: NativeDecimalBinaryOp,
) -> Option<NativeDecimalFastValue> {
    if operation == NativeDecimalBinaryOp::Multiply {
        let scale = left.scale.checked_add(right.scale)?;
        if scale > MAX_FRACTION as u32 {
            return None;
        }
        return Some(NativeDecimalFastValue {
            coefficient: left.coefficient.checked_mul(right.coefficient)?,
            storage_scale: left.storage_scale.checked_add(right.storage_scale)?,
            scale,
        });
    }
    if operation == NativeDecimalBinaryOp::Subtract {
        right.coefficient = right.coefficient.checked_neg()?;
    }
    let storage_scale = left.storage_scale.max(right.storage_scale);
    let aligned = |value: NativeDecimalFastValue| {
        if storage_scale == value.storage_scale {
            Some(value.coefficient)
        } else {
            value
                .coefficient
                .checked_mul(10i128.checked_pow(storage_scale - value.storage_scale)?)
        }
    };
    Some(NativeDecimalFastValue {
        coefficient: aligned(left)?.checked_add(aligned(right)?)?,
        storage_scale,
        scale: left.scale.max(right.scale),
    })
}

/// A bridge/resource refusal is not a SQL numeric overflow. Core failures keep
/// their original owned cause; callers must not classify them by message text.
#[derive(Debug)]
pub enum NativeDecimalError {
    InvalidInput(&'static str),
    Resource(&'static str),
    Core(Error),
}

impl fmt::Display for NativeDecimalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(detail) => write!(formatter, "invalid native decimal: {detail}"),
            Self::Resource(detail) => {
                write!(formatter, "native decimal resource refusal: {detail}")
            }
            Self::Core(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for NativeDecimalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Core(error) => Some(error),
            Self::InvalidInput(_) | Self::Resource(_) => None,
        }
    }
}

type NativeDecimalResult<T> = std::result::Result<T, NativeDecimalError>;

// A limit applies to ONE materialized word value/buffer, including its nine
// initialized inline cells, not allocator capacity, headers or a combined peak.
// Round/shift preflights may reserve a conservative carry/alignment bound.
fn native_decimal_word_budget(words: usize, limit: usize) -> NativeDecimalResult<()> {
    let bytes = words
        .max(WORD_BUF_LEN)
        .checked_mul(mem::size_of::<u32>())
        .filter(|bytes| *bytes <= isize::MAX as usize)
        .ok_or(NativeDecimalError::Resource(
            "word-buffer byte count overflow",
        ))?;
    if bytes > limit {
        return Err(NativeDecimalError::Resource("word buffer exceeds limit"));
    }
    Ok(())
}

fn native_digit_reserve(
    digits: &mut Vec<u8>,
    length: usize,
    limit: usize,
) -> NativeDecimalResult<()> {
    if length > limit || length > isize::MAX as usize {
        return Err(NativeDecimalError::Resource(
            "coefficient buffer exceeds limit",
        ));
    }
    digits
        .try_reserve_exact(length.saturating_sub(digits.len()))
        .map_err(|_| NativeDecimalError::Resource("coefficient allocation failed"))
}

fn native_pad_coefficient(
    digits: &mut Vec<u8>,
    width: usize,
    limit: usize,
) -> NativeDecimalResult<()> {
    if digits.len() < width {
        native_digit_reserve(digits, width, limit)?;
        let length = digits.len();
        digits.resize(width, b'0');
        digits.copy_within(..length, width - length);
        digits[..width - length].fill(b'0');
    }
    Ok(())
}

/// Unsigned coefficient arithmetic for the remaining native parser/round/divide
/// consumers. Addition/subtraction keep their original zero-padded width;
/// multiplication returns canonical digits. Subtraction requires lhs >= rhs.
pub fn native_decimal_coefficient_binary(
    lhs: &[u8],
    rhs: &[u8],
    operation: NativeDecimalBinaryOp,
    limit: usize,
) -> NativeDecimalResult<Vec<u8>> {
    if lhs.is_empty() && rhs.is_empty() && operation != NativeDecimalBinaryOp::Multiply {
        return Ok(Vec::new());
    }
    let left = Decimal::try_from_native_digits(
        false,
        if lhs.is_empty() { b"0" } else { lhs },
        0,
        0,
        limit,
    )?;
    let right = Decimal::try_from_native_digits(
        false,
        if rhs.is_empty() { b"0" } else { rhs },
        0,
        0,
        limit,
    )?;
    let value = left
        .try_native_binary(&right, operation, NativeDecimalBinaryPolicy::Exact, limit)?
        .unwrap();
    if value.negative {
        return Err(NativeDecimalError::InvalidInput(
            "coefficient subtraction requires lhs >= rhs",
        ));
    }
    let mut digits = value.native_coefficient_digits(limit)?;
    if operation != NativeDecimalBinaryOp::Multiply {
        native_pad_coefficient(&mut digits, lhs.len().max(rhs.len()), limit)?;
    }
    Ok(digits)
}

#[derive(Debug, Clone)]
pub enum RoundMode {
    // HalfEven rounds normally.
    HalfEven,
    // Truncate just truncates the decimal.
    Truncate,
    // Ceiling is not supported now.
    Ceiling,
}

impl Default for Decimal {
    fn default() -> Self {
        // Valid in-memory NULL backing; physical NULL bytes follow the bitmap.
        Self::zero()
    }
}

impl Decimal {
    /// Imports a native unsigned coefficient without text formatting/parsing or
    /// a nine-word projection. Only the native logical domain is admitted:
    /// nonempty ASCII digits, enough digits for storage, and result <= storage.
    /// `limit` bounds each materialized word buffer's logical data bytes, never
    /// precision. The borrowed coefficient and simultaneous owners are not a
    /// combined physical-memory accounting claim.
    pub fn try_from_native_digits(
        negative: bool,
        digits: &[u8],
        storage: u32,
        result: u32,
        limit: usize,
    ) -> std::result::Result<Self, NativeDecimalError> {
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return Err(NativeDecimalError::InvalidInput(
                "coefficient must contain ASCII digits",
            ));
        }
        if result > storage {
            return Err(NativeDecimalError::InvalidInput(
                "result scale exceeds storage scale",
            ));
        }
        let fraction = usize::try_from(storage)
            .map_err(|_| NativeDecimalError::Resource("storage scale exceeds indexing width"))?;
        let integer =
            digits
                .len()
                .checked_sub(fraction)
                .ok_or(NativeDecimalError::InvalidInput(
                    "coefficient omits stored fraction digits",
                ))?;
        let int_words = integer.div_ceil(DIGITS_PER_WORD);
        let active = int_words
            .checked_add(fraction.div_ceil(DIGITS_PER_WORD))
            .ok_or(NativeDecimalError::Resource("active word count overflow"))?;
        native_decimal_word_budget(active, limit)?;
        let mut words = Vec::new();
        words
            .try_reserve_exact(active)
            .map_err(|_| NativeDecimalError::Resource("coefficient word allocation failed"))?;
        words.resize(active, 0);
        let word = |chunk: &[u8]| {
            chunk
                .iter()
                .fold(0_u32, |value, digit| value * 10 + u32::from(*digit - b'0'))
        };
        let mut offset = 0;
        for (index, destination) in words[..int_words].iter_mut().enumerate() {
            let width = if index == 0 {
                (integer - 1) % DIGITS_PER_WORD + 1
            } else {
                DIGITS_PER_WORD
            };
            *destination = word(&digits[offset..offset + width]);
            offset += width;
        }
        for (destination, chunk) in words[int_words..]
            .iter_mut()
            .zip(digits[integer..].chunks(DIGITS_PER_WORD))
        {
            *destination = word(chunk) * TEN_POW[DIGITS_PER_WORD - chunk.len()];
        }
        Self::try_from_words(DecimalWordsRef {
            int_digits: integer,
            storage_frac: storage,
            result_frac: result,
            negative,
            words: &words,
        })
        .map_err(NativeDecimalError::Core)
    }

    /// Fallible logical-value extraction for the native math result carrier.
    /// This does not admit raw physical cells, visible-only padding, or
    /// inactive heap payloads as native coefficients. The sign of zero is
    /// transported; value-producing ABS/round operations normalize it
    /// separately.
    pub fn try_clone_native_math(
        &self,
        limit: usize,
    ) -> std::result::Result<Self, NativeDecimalError> {
        self.check_native_math_value(limit)?;
        self.try_clone_for_worker()
            .map_err(NativeDecimalError::Core)
    }

    /// Evaluates one admitted native policy using the existing exact workers.
    /// SQL argument coercion, NULL, result-type selection and scale caps belong
    /// to the closed expression caller. `limit` is a per-value/buffer logical
    /// byte allowance, not a precision cap or a total allocation-peak bound.
    pub fn try_native_math(
        &self,
        operation: NativeDecimalOp,
        limit: usize,
    ) -> std::result::Result<Self, NativeDecimalError> {
        self.check_native_math_value(limit)?;
        match operation {
            NativeDecimalOp::Abs => {
                let value = self
                    .try_clone_for_worker()
                    .map_err(NativeDecimalError::Core)?;
                Self::try_finish_exact(value.abs(), self.result_frac_cnt)
                    .map_err(NativeDecimalError::Core)
            }
            NativeDecimalOp::Negate => {
                let value = self
                    .try_clone_for_worker()
                    .map_err(NativeDecimalError::Core)?;
                // Reuse the wire Neg leaf; only the native finish canonicalizes
                // signed zero, preserving the wire/status-payload sign policy.
                Self::try_finish_exact(Res::Ok(-value), self.result_frac_cnt)
                    .map_err(NativeDecimalError::Core)
            }
            NativeDecimalOp::Ceil | NativeDecimalOp::Floor => {
                let ceiling = operation == NativeDecimalOp::Ceil;
                let mode = if ceiling != self.negative {
                    RoundMode::Ceiling
                } else {
                    RoundMode::Truncate
                };
                self.native_math_round(0, mode, limit)
            }
            NativeDecimalOp::Round(scale) => {
                self.native_math_round_compat(scale, true, scale.max(0) as u32, limit)
            }
            NativeDecimalOp::Truncate(scale) => {
                self.native_math_round_compat(scale, false, scale.max(0) as u32, limit)
            }
        }
    }

    /// Materialize the signed fast shape without parsing or visible-scale
    /// rounding. All digit and word allocations honor the bridge buffer limit.
    pub fn try_from_native_fast(
        value: NativeDecimalFastValue,
        limit: usize,
    ) -> NativeDecimalResult<Self> {
        if value.scale > value.storage_scale {
            return Err(NativeDecimalError::InvalidInput(
                "result scale exceeds storage scale",
            ));
        }
        let mut buffer = [b'0'; 39];
        let mut index = buffer.len();
        let mut magnitude = value.coefficient.unsigned_abs();
        loop {
            index -= 1;
            buffer[index] += (magnitude % 10) as u8;
            magnitude /= 10;
            if magnitude == 0 {
                break;
            }
        }
        let mut digits = Vec::new();
        native_digit_reserve(
            &mut digits,
            (buffer.len() - index).max(value.storage_scale as usize),
            limit,
        )?;
        digits.extend_from_slice(&buffer[index..]);
        native_pad_coefficient(&mut digits, value.storage_scale as usize, limit)?;
        Self::try_from_native_digits(
            value.coefficient < 0,
            &digits,
            value.storage_scale,
            value.scale,
            limit,
        )
    }

    /// Lossless signed-i128 coefficient projection, including MIN. None is an
    /// unsupported coefficient, while invalid shape/resource refusal is Err.
    pub fn try_native_fast_value(
        &self,
        limit: usize,
    ) -> NativeDecimalResult<Option<NativeDecimalFastValue>> {
        self.check_native_math_value(limit)?;
        let Some(magnitude) = self.native_coefficient_u128() else {
            return Ok(None);
        };
        let coefficient = if self.negative && magnitude == (1u128 << 127) {
            i128::MIN
        } else {
            let Ok(value) = i128::try_from(magnitude) else {
                return Ok(None);
            };
            if self.negative { -value } else { value }
        };
        Ok(Some(NativeDecimalFastValue {
            coefficient,
            storage_scale: self.frac_cnt as u32,
            scale: self.result_frac_cnt as u32,
        }))
    }

    /// Apply an exact/native-MySQL binary policy through the existing word
    /// workers. Resource refusals are not SQL arithmetic statuses.
    pub fn try_native_binary(
        &self,
        rhs: &Self,
        operation: NativeDecimalBinaryOp,
        policy: NativeDecimalBinaryPolicy,
        limit: usize,
    ) -> NativeDecimalResult<Res<Self>> {
        self.check_native_math_value(limit)?;
        rhs.check_native_math_value(limit)?;
        if operation == NativeDecimalBinaryOp::Multiply {
            return if policy == NativeDecimalBinaryPolicy::MySql {
                self.native_mysql_multiply(rhs, limit)
            } else {
                self.native_exact_multiply(rhs, limit).map(Res::Ok)
            };
        }
        let bound = self
            .int_words()
            .max(rhs.int_words())
            .checked_add(1)
            .and_then(|integer| integer.checked_add(self.frac_words().max(rhs.frac_words())))
            .ok_or(NativeDecimalError::Resource("binary word count overflow"))?;
        native_decimal_word_budget(bound, limit)?;
        let value = match operation {
            NativeDecimalBinaryOp::Add => self.try_add_exact(rhs),
            NativeDecimalBinaryOp::Subtract => self.try_sub_exact(rhs),
            NativeDecimalBinaryOp::Multiply => unreachable!(),
        }
        .map_err(NativeDecimalError::Core)?;
        if policy == NativeDecimalBinaryPolicy::Exact {
            return Ok(Res::Ok(value));
        }
        let adds_magnitudes = match operation {
            NativeDecimalBinaryOp::Add => self.negative == rhs.negative,
            NativeDecimalBinaryOp::Subtract => self.negative != rhs.negative,
            NativeDecimalBinaryOp::Multiply => unreachable!(),
        };
        if adds_magnitudes {
            let left = self.native_projected_value(limit)?;
            let right = rhs.native_projected_value(limit)?;
            if addition_int_word_count(&left, &right).map_err(NativeDecimalError::Core)?
                > WORD_BUF_LEN
            {
                return Self::native_positive_maximum(limit).map(Res::Overflow);
            }
        }
        let integer_words = value
            .remove_leading_zeroes(value.int_cnt)
            .1
            .div_ceil(DIGITS_PER_WORD);
        if integer_words > WORD_BUF_LEN {
            return Self::native_positive_maximum(limit).map(Res::Overflow);
        }
        if integer_words + value.frac_words() <= WORD_BUF_LEN {
            return Ok(Res::Ok(value));
        }
        let kept = ((WORD_BUF_LEN - integer_words) * DIGITS_PER_WORD) as i32;
        value
            .try_native_math(NativeDecimalOp::Truncate(kept), limit)
            .map(Res::Truncated)
    }

    fn native_positive_maximum(limit: usize) -> NativeDecimalResult<Self> {
        native_decimal_word_budget(WORD_BUF_LEN, limit)?;
        let mut value = Self::try_new(WORD_BUF_LEN * DIGITS_PER_WORD, 0, false)
            .map_err(NativeDecimalError::Core)?;
        value.word_buf.fill(WORD_MAX);
        Ok(value)
    }

    fn native_zero(negative: bool, scale: u32, limit: usize) -> NativeDecimalResult<Self> {
        let fraction = scale as usize;
        native_decimal_word_budget(fraction.div_ceil(DIGITS_PER_WORD).max(1), limit)?;
        Self::try_new(usize::from(scale == 0), fraction, negative).map_err(NativeDecimalError::Core)
    }

    fn native_exact_multiply(&self, rhs: &Self, limit: usize) -> NativeDecimalResult<Self> {
        // Keep the native public API's plain-u32 additions and thus this build's
        // original overflow-check policy, rather than turning it into SQL overflow.
        let result_scale = (self.result_frac_cnt as u32) + (rhs.result_frac_cnt as u32);
        let storage_scale = (self.frac_cnt as u32) + (rhs.frac_cnt as u32);
        // Explicit bridge-domain exception for unchecked-overflow builds: a
        // wrapped visible scale beyond storage is not an admitted value. Keep
        // this an infrastructure refusal, never a SQL Overflow disposition.
        if result_scale > storage_scale {
            return Err(NativeDecimalError::InvalidInput(
                "wrapped result scale exceeds storage scale",
            ));
        }
        let bound = self
            .int_cnt
            .checked_add(rhs.int_cnt)
            .map(|digits| digits.div_ceil(DIGITS_PER_WORD))
            .and_then(|words| words.checked_add(self.frac_words()))
            .and_then(|words| words.checked_add(rhs.frac_words()))
            .ok_or(NativeDecimalError::Resource("product word count overflow"))?;
        native_decimal_word_budget(bound, limit)?;
        if self.frac_cnt.checked_add(rhs.frac_cnt) == Some(storage_scale as usize)
            && self.result_frac_cnt.checked_add(rhs.result_frac_cnt) == Some(result_scale as usize)
        {
            return self.try_mul_exact(rhs).map_err(NativeDecimalError::Core);
        }
        // In an unchecked-overflow build, preserve representable wrapped scale
        // headers by multiplying the exact coefficients, not reinterpreting the
        // wrapped scales as an instruction to round the mathematical product.
        let left = self.native_coefficient_digits(limit)?;
        let right = rhs.native_coefficient_digits(limit)?;
        let digits = native_decimal_coefficient_binary(
            &left,
            &right,
            NativeDecimalBinaryOp::Multiply,
            limit,
        )?;
        let mut padded = digits;
        native_pad_coefficient(&mut padded, (storage_scale as usize).max(1), limit)?;
        Self::try_from_native_digits(
            self.negative != rhs.negative && padded.iter().any(|digit| *digit != b'0'),
            &padded,
            storage_scale,
            result_scale,
            limit,
        )
    }

    // The old i128 fast path tests unsigned coefficient magnitude, including
    // all hidden fractional digits. It is eligibility, not a second arithmetic
    // implementation: successful multiplication still uses the Grow worker.
    fn native_coefficient_i128(&self) -> Option<i128> {
        i128::try_from(self.native_coefficient_u128()?).ok()
    }

    fn native_coefficient_u128(&self) -> Option<u128> {
        let active = self.int_words().checked_add(self.frac_words())?;
        let mut coefficient = 0u128;
        for (index, word) in self.word_buf[..active].iter().copied().enumerate() {
            let width = if index + 1 == active && self.frac_cnt % DIGITS_PER_WORD != 0 {
                self.frac_cnt % DIGITS_PER_WORD
            } else {
                DIGITS_PER_WORD
            };
            let digits = word / TEN_POW[DIGITS_PER_WORD - width];
            coefficient = coefficient
                .checked_mul(u128::from(TEN_POW[width]))?
                .checked_add(u128::from(digits))?;
        }
        Some(coefficient)
    }

    fn native_mysql_multiply(&self, rhs: &Self, limit: usize) -> NativeDecimalResult<Res<Self>> {
        let fast = (self.result_frac_cnt as u32)
            .checked_add(rhs.result_frac_cnt as u32)
            .filter(|scale| *scale <= MAX_FRACTION as u32)
            .and_then(|_| {
                self.native_coefficient_i128()?
                    .checked_mul(rhs.native_coefficient_i128()?)
            })
            .and_then(|_| (self.frac_cnt as u32).checked_add(rhs.frac_cnt as u32));
        if fast.is_some() {
            return self.native_exact_multiply(rhs, limit).map(Res::Ok);
        }
        let left = self.native_projected_value(limit)?;
        let right = rhs.native_projected_value(limit)?;
        let result_scale =
            ((self.result_frac_cnt as u32) + (rhs.result_frac_cnt as u32)).min(MAX_FRACTION as u32);
        // Native MyDecimal uses projected fractions without the wire's 31-digit
        // storage cap. Both policies execute the same word multiplication loop.
        let output = do_mul_with_policy(&left, &right, WordLimit::Fixed(WORD_BUF_LEN), true)
            .map_err(NativeDecimalError::Core)?;
        if output.is_overflow() {
            return Self::native_zero(self.negative != rhs.negative, result_scale, limit)
                .map(Res::Overflow);
        }
        let truncated = output.is_truncated();
        let output = output.unwrap();
        let value = if output.is_zero() {
            Self::native_zero(false, result_scale, limit)?
        } else {
            let storage = output.frac_cnt as u32;
            let output = Self::try_finish_exact(Res::Ok(output), storage as usize)
                .map_err(NativeDecimalError::Core)?;
            output.try_native_round_with_storage(
                result_scale as i32,
                true,
                storage.max(result_scale),
                limit,
            )?
        };
        Ok(if truncated {
            Res::Truncated(value)
        } else {
            Res::Ok(value)
        })
    }

    /// Original MyDecimalWords projection: low integer words on overflow,
    /// leading fractional words on truncation, with no normalization of the
    /// projected shape or zero sign. Also used by the native codec facade.
    pub fn try_native_word_projection(&self, limit: usize) -> NativeDecimalResult<DecimalParts> {
        self.check_native_math_value(limit)?;
        let original_int = self.int_words();
        let original_frac = self.frac_words();
        let status = fix_word_cnt_err(original_int, original_frac, WORD_BUF_LEN);
        let (integer, fraction) = *status;
        let int_digits = if status.is_overflow() {
            integer * DIGITS_PER_WORD
        } else {
            self.int_cnt
        };
        let frac_digits = if status.is_ok() {
            self.frac_cnt
        } else {
            fraction * DIGITS_PER_WORD
        };
        let mut words = [0; WORD_BUF_LEN];
        words[..integer].copy_from_slice(&self.word_buf[original_int - integer..original_int]);
        words[integer..integer + fraction]
            .copy_from_slice(&self.word_buf[original_int..original_int + fraction]);
        Ok(DecimalParts {
            int_digits: int_digits as u8,
            frac_digits: frac_digits as u8,
            result_frac_digits: frac_digits as u8,
            negative: self.negative,
            words,
        })
    }

    fn native_projected_value(&self, limit: usize) -> NativeDecimalResult<Self> {
        let parts = self.try_native_word_projection(limit)?;
        let mut value = Self::try_new(
            parts.int_digits as usize,
            parts.frac_digits as usize,
            parts.negative,
        )
        .map_err(NativeDecimalError::Core)?;
        value.word_buf.copy_from_slice(&parts.words);
        Ok(value)
    }

    // Exact coefficient layout conversion, without sign, SQL formatting, or
    // result-scale rounding. This also supports the surviving coefficient-only
    // parsing/division consumers of the native arithmetic helpers.
    fn native_coefficient_digits(&self, limit: usize) -> NativeDecimalResult<Vec<u8>> {
        let length =
            self.int_cnt
                .checked_add(self.frac_cnt)
                .ok_or(NativeDecimalError::Resource(
                    "coefficient digit count overflow",
                ))?;
        let mut digits = Vec::new();
        native_digit_reserve(&mut digits, length.max(1), limit)?;
        let integer = self.int_words();
        let mut emit = |word: u32, width: usize| {
            let mut buffer = [b'0'; DIGITS_PER_WORD];
            let mut value = word;
            for digit in buffer[..width].iter_mut().rev() {
                *digit += (value % 10) as u8;
                value /= 10;
            }
            digits.extend_from_slice(&buffer[..width]);
        };
        for index in 0..integer {
            let width = if index == 0 {
                (self.int_cnt - 1) % DIGITS_PER_WORD + 1
            } else {
                DIGITS_PER_WORD
            };
            emit(self.word_buf[index], width);
        }
        let mut remaining = self.frac_cnt;
        for index in integer..integer + self.frac_words() {
            let width = remaining.min(DIGITS_PER_WORD);
            emit(
                self.word_buf[index] / TEN_POW[DIGITS_PER_WORD - width],
                width,
            );
            remaining -= width;
        }
        if digits.is_empty() {
            digits.push(b'0');
        }
        Ok(digits)
    }

    /// The same native round policy with explicitly retained storage scale.
    /// This narrow bridge serves existing value consumers such as
    /// multiplication; visible scale is max(scale, 0), and storage is at
    /// least that visible scale. It is not a new precision limit or a
    /// general rounding-mode interface.
    pub fn try_native_round_with_storage(
        &self,
        scale: i32,
        round: bool,
        storage: u32,
        limit: usize,
    ) -> std::result::Result<Self, NativeDecimalError> {
        self.check_native_math_value(limit)?;
        self.native_math_round_compat(scale, round, storage, limit)
    }

    fn check_native_math_value(&self, limit: usize) -> NativeDecimalResult<()> {
        if self.result_frac_cnt > self.frac_cnt {
            return Err(NativeDecimalError::InvalidInput(
                "result scale exceeds storage scale",
            ));
        }
        self.int_cnt
            .checked_add(self.frac_cnt)
            .ok_or(NativeDecimalError::Resource(
                "coefficient digit count overflow",
            ))?;
        let active = self
            .int_words()
            .checked_add(self.frac_words())
            .ok_or(NativeDecimalError::Resource("active word count overflow"))?;
        if active == 0 || active > self.word_buf.len() {
            return Err(NativeDecimalError::InvalidInput(
                "native coefficient requires initialized active words",
            ));
        }
        native_decimal_word_budget(active, limit)?;
        if self.word_buf[..active]
            .iter()
            .any(|word| *word >= WORD_BASE)
        {
            return Err(NativeDecimalError::InvalidInput(
                "active word is not base-1e9",
            ));
        }
        let head = self.int_cnt % DIGITS_PER_WORD;
        if head != 0 && self.word_buf[0] >= TEN_POW[head] {
            return Err(NativeDecimalError::InvalidInput(
                "leading word exceeds its digit count",
            ));
        }
        let tail = self.frac_cnt % DIGITS_PER_WORD;
        if tail != 0 && self.word_buf[active - 1] % TEN_POW[DIGITS_PER_WORD - tail] != 0 {
            return Err(NativeDecimalError::InvalidInput(
                "fractional word has nonzero padding",
            ));
        }
        Ok(())
    }

    fn native_math_round(
        &self,
        scale: i64,
        mode: RoundMode,
        limit: usize,
    ) -> NativeDecimalResult<Self> {
        let target = u32::try_from(scale.max(0))
            .map_err(|_| NativeDecimalError::Resource("round scale exceeds u32"))?;
        let target_words = usize::try_from(target)
            .map_err(|_| NativeDecimalError::Resource("round scale exceeds indexing width"))?
            .div_ceil(DIGITS_PER_WORD);
        let mut bound = self
            .int_words()
            .checked_add(self.frac_words().max(target_words))
            .ok_or(NativeDecimalError::Resource("round word count overflow"))?;
        if scale < i64::from(self.storage_scale()) && !matches!(mode, RoundMode::Truncate) {
            bound = bound
                .checked_add(1)
                .ok_or(NativeDecimalError::Resource("round carry count overflow"))?;
        }
        native_decimal_word_budget(bound, limit)?;
        self.try_round_exact(scale, mode)
            .map_err(NativeDecimalError::Core)
    }

    fn native_math_round_compat(
        &self,
        target: i32,
        round: bool,
        storage: u32,
        limit: usize,
    ) -> NativeDecimalResult<Self> {
        let result_scale = target.max(0) as u32;
        let storage_scale = storage.max(result_scale);
        // These are deliberately the source unchecked expressions. The caller
        // workspace's overflow-check setting, not debug_assertions, decides
        // whether they panic or wrap. A wrapped branch is NOT mathematical zero.
        let shift = self.storage_scale() as i32 - target;
        if shift <= 0 {
            let padding = storage_scale - self.storage_scale();
            let displacement =
                i64::from(self.storage_scale()) + i64::from(padding) - i64::from(storage_scale);
            if displacement == 0 {
                return self
                    .try_clone_native_math(limit)?
                    .native_math_output_storage(storage_scale, result_scale, limit);
            }
            // A wrapped pad adds 2^32 to the numeric exponent. Zero needs no
            // padding allocation; a nonzero expanded value is preflighted below.
            return self
                .try_clone_native_math(limit)?
                .native_math_shift_and_scale(displacement, storage_scale, result_scale, limit);
        }
        // Keep the remaining source unary-minus overflow point as well. A
        // wrapped negative usize padding count is a resource refusal, not SQL
        // overflow and not a reason to attempt a gigantic allocation.
        let trailing = if target < 0 { (-target) as usize } else { 0 };
        if trailing > isize::MAX as usize {
            return Err(NativeDecimalError::Resource(
                "native trailing padding exceeds indexing width",
            ));
        }
        let effective_scale = i64::from(self.storage_scale()) - i64::from(shift);
        let mode = if round {
            RoundMode::HalfEven
        } else {
            RoundMode::Truncate
        };
        let value = self.native_math_round(effective_scale, mode, limit)?;
        let displacement =
            i128::from(effective_scale) + trailing as i128 - i128::from(result_scale);
        let displacement = i64::try_from(displacement)
            .map_err(|_| NativeDecimalError::Resource("native scale displacement exceeds i64"))?;
        if displacement == 0 {
            return value.native_math_output_storage(storage_scale, result_scale, limit);
        }
        value.native_math_shift_and_scale(displacement, storage_scale, result_scale, limit)
    }

    fn native_math_output_storage(
        self,
        storage_scale: u32,
        result_scale: u32,
        limit: usize,
    ) -> NativeDecimalResult<Self> {
        let storage = usize::try_from(storage_scale).map_err(|_| {
            NativeDecimalError::Resource("output storage scale exceeds indexing width")
        })?;
        let result = usize::try_from(result_scale).map_err(|_| {
            NativeDecimalError::Resource("output result scale exceeds indexing width")
        })?;
        let value = if self.frac_cnt == storage {
            self
        } else {
            self.native_math_round(i64::from(storage_scale), RoundMode::Truncate, limit)?
        };
        Self::try_finish_exact(Res::Ok(value), result).map_err(NativeDecimalError::Core)
    }

    fn native_math_shift_and_scale(
        self,
        displacement: i64,
        storage_scale: u32,
        result_scale: u32,
        limit: usize,
    ) -> NativeDecimalResult<Self> {
        if self.is_zero() {
            return Self::zero().native_math_output_storage(storage_scale, result_scale, limit);
        }
        // Native wrapped-scale branches only increase the numeric exponent.
        // This is a resource bound; the existing shift worker owns all digits.
        let displacement_usize = usize::try_from(displacement).map_err(|_| {
            NativeDecimalError::Resource("native scale displacement exceeds indexing width")
        })?;
        let integer =
            self.int_cnt
                .checked_add(displacement_usize)
                .ok_or(NativeDecimalError::Resource(
                    "native shifted integer count overflow",
                ))?;
        let bound = integer
            .div_ceil(DIGITS_PER_WORD)
            .checked_add(self.frac_words())
            .ok_or(NativeDecimalError::Resource(
                "native shifted word count overflow",
            ))?;
        native_decimal_word_budget(bound, limit)?;
        let shifted = self
            .shift_with_limit(
                i128::from(displacement),
                WordLimit::Grow,
                ShiftDisposition::Legacy,
            )
            .map_err(NativeDecimalError::Core)?;
        let storage = shifted.result.frac_cnt;
        let shifted =
            Self::try_finish_exact(shifted.result, storage).map_err(NativeDecimalError::Core)?;
        shifted.native_math_output_storage(storage_scale, result_scale, limit)
    }

    /// Imports exact bounded components without parsing, rounding or SQL
    /// policy.
    ///
    /// Checks active word capacity/ranges, partial-word padding and a result
    /// fraction count at most 81. It does not impose SQL's 65/30 declaration
    /// limits or require stored and result fraction counts to agree. Inactive
    /// words and the sign of zero are retained. A zero-length active prefix is
    /// rejected by this initial logical admission contract (canonical zero has
    /// one integer digit). Physical cells have a separate checked adapter.
    ///
    /// Over-capacity status payloads are observable through `words`, not a
    /// silently truncated nine-word export.
    pub fn try_from_parts(parts: DecimalParts) -> Result<Self> {
        Self::from_fixed_parts(parts, false)
    }

    /// Exact logical import, private until every reachable wide consumer is
    /// closed. This extends strict parts validation, not physical admission.
    /// Supplied inactive cells, count padding and raw zero sign are preserved.
    fn try_from_words(parts: DecimalWordsRef<'_>) -> Result<Self> {
        let storage_frac = usize::try_from(parts.storage_frac)
            .map_err(|_| decimal_resource_error("storage scale exceeds indexing width"))?;
        let result_frac = usize::try_from(parts.result_frac)
            .map_err(|_| decimal_resource_error("result scale exceeds indexing width"))?;
        parts
            .int_digits
            .checked_add(storage_frac)
            .ok_or_else(|| decimal_resource_error("import storage precision count overflow"))?;
        let int_words = parts.int_digits.div_ceil(DIGITS_PER_WORD);
        let frac_words = storage_frac.div_ceil(DIGITS_PER_WORD);
        let active = checked_word_extent(int_words, frac_words)?;
        checked_word_digits(active)?;
        checked_word_extent(parts.words.len(), 0)?;
        if active == 0 {
            return Err(Error::InvalidDataType(
                "logical decimal words require a nonempty active prefix".to_owned(),
            ));
        }
        if parts.words.len() < active {
            return Err(Error::InvalidDataType(
                "decimal words omit active initialized cells".to_owned(),
            ));
        }
        for (index, word) in parts.words[..active].iter().enumerate() {
            if *word >= WORD_BASE {
                return Err(Error::InvalidDataType(format!(
                    "decimal active word {index}={word} is not base-1e9"
                )));
            }
        }
        let head_digits = parts.int_digits % DIGITS_PER_WORD;
        if head_digits != 0 && parts.words[0] >= TEN_POW[head_digits] {
            return Err(Error::InvalidDataType(
                "decimal leading integer word exceeds its digit count".to_owned(),
            ));
        }
        let tail_digits = storage_frac % DIGITS_PER_WORD;
        if tail_digits != 0 && parts.words[active - 1] % TEN_POW[DIGITS_PER_WORD - tail_digits] != 0
        {
            return Err(Error::InvalidDataType(
                "decimal trailing fractional word has nonzero padding".to_owned(),
            ));
        }
        // Validate before indexing/allocation, then reserve the supplied
        // INITIALIZED extent, not just numerical active words or Vec capacity.
        let mut value = Self {
            int_cnt: parts.int_digits,
            frac_cnt: storage_frac,
            result_frac_cnt: result_frac,
            negative: parts.negative,
            word_buf: SmallVec::from_buf([0; WORD_BUF_LEN]),
        };
        value.try_reserve_words(parts.words.len())?;
        value.word_buf[..parts.words.len()].copy_from_slice(parts.words);
        // No arithmetic normalization or ensure_storage here: that would
        // strip headers/sign or silently drop supplied inactive heap cells.
        Ok(value)
    }

    // Physical transport preserves all byte-sized result headers and does not
    // impose logical partial-word normalization. Both domains require valid
    // active base-1e9 words within the supplied nine cells. Raw-like-Go cells
    // with invalid counts/words remain outside this checked owning admission.
    fn from_fixed_parts(parts: DecimalParts, physical: bool) -> Result<Self> {
        let digits_per_word = DIGITS_PER_WORD;
        // Widen before ceiling/count arithmetic so malformed u8 counts cannot
        // wrap before the capacity check or reach an indexing operation.
        let int_words = usize::from(parts.int_digits).div_ceil(digits_per_word);
        let frac_words = usize::from(parts.frac_digits).div_ceil(digits_per_word);
        let used_words = int_words + frac_words;
        if used_words > usize::from(WORD_BUF_LEN) {
            return Err(Error::InvalidDataType(format!(
                "decimal parts need {used_words} words for int_digits={} frac_digits={}",
                parts.int_digits, parts.frac_digits
            )));
        }
        if !physical && usize::from(parts.result_frac_digits) > DIGITS_PER_WORD * WORD_BUF_LEN {
            return Err(Error::InvalidDataType(format!(
                "decimal parts result_frac_digits={} exceeds 81",
                parts.result_frac_digits
            )));
        }
        for (index, word) in parts.words[..used_words].iter().enumerate() {
            if *word >= WORD_BASE {
                return Err(Error::InvalidDataType(format!(
                    "decimal parts active word {index}={word} is not base-1e9"
                )));
            }
        }
        if used_words == 0 && !physical {
            return Err(Error::InvalidDataType(
                "decimal parts with no active words are not an admitted zero representation"
                    .to_owned(),
            ));
        }
        let leading_digits = usize::from(parts.int_digits) % DIGITS_PER_WORD;
        if !physical
            && leading_digits != 0
            && parts.words[0] >= TEN_POW[usize::from(leading_digits)]
        {
            return Err(Error::InvalidDataType(
                "decimal parts leading integer word exceeds its digit count".to_owned(),
            ));
        }
        let trailing_digits = usize::from(parts.frac_digits) % DIGITS_PER_WORD;
        if !physical
            && trailing_digits != 0
            && parts.words[used_words - 1] % TEN_POW[usize::from(DIGITS_PER_WORD - trailing_digits)]
                != 0
        {
            return Err(Error::InvalidDataType(
                "decimal parts trailing fractional word has nonzero padding".to_owned(),
            ));
        }
        Ok(Self {
            int_cnt: usize::from(parts.int_digits),
            frac_cnt: usize::from(parts.frac_digits),
            result_frac_cnt: usize::from(parts.result_frac_digits),
            negative: parts.negative,
            word_buf: SmallVec::from_buf(parts.words),
        })
    }

    /// Borrows exact logical fields, including safe over-capacity status data.
    pub fn words(&self) -> DecimalWordsRef<'_> {
        DecimalWordsRef {
            int_digits: self.int_cnt,
            storage_frac: self.storage_scale(),
            result_frac: self.result_scale(),
            negative: self.negative,
            words: &self.word_buf,
        }
    }

    /// Checked bounded transport, never a lossy projection or diagnostic clip.
    pub fn try_to_parts(&self) -> Result<DecimalParts> {
        self.fixed_parts(false)
    }

    fn fixed_parts(&self, physical: bool) -> Result<DecimalParts> {
        let narrow = |name: &str, count: usize| {
            u8::try_from(count).map_err(|_| {
                Error::InvalidDataType(format!(
                    "decimal {name}={count} does not fit a physical count"
                ))
            })
        };
        if self.word_buf.len() > WORD_BUF_LEN {
            return Err(Error::InvalidDataType(
                "decimal backing does not fit the bounded nine-word record".to_owned(),
            ));
        }
        let mut words = [0; WORD_BUF_LEN];
        words[..self.word_buf.len()].copy_from_slice(&self.word_buf);
        let parts = DecimalParts {
            int_digits: narrow("int_digits", self.int_cnt)?,
            frac_digits: narrow("storage_frac", self.frac_cnt)?,
            result_frac_digits: narrow("result_frac", self.result_frac_cnt)?,
            negative: self.negative,
            words,
        };
        Self::from_fixed_parts(parts, physical)?;
        Ok(parts)
    }

    pub fn storage_scale(&self) -> u32 {
        u32::try_from(self.frac_cnt).expect("constructed Decimal storage scale is u32")
    }

    pub fn result_scale(&self) -> u32 {
        u32::try_from(self.result_frac_cnt).expect("constructed Decimal result scale is u32")
    }

    pub fn integer_digits(&self) -> usize {
        self.remove_leading_zeroes(self.int_cnt).1
    }

    pub fn natural_storage_shape(&self) -> (usize, u32) {
        self.prec_and_frac()
    }

    /// Heap allocation in addition to the inline owning value, not wire bytes.
    pub fn spill_capacity_bytes(&self) -> usize {
        if self.word_buf.spilled() {
            self.word_buf.capacity() * mem::size_of::<u32>()
        } else {
            0
        }
    }

    /// abs the Decimal into a new Decimal.
    #[inline]
    pub fn abs(mut self) -> Res<Decimal> {
        self.negative = false;
        Res::Ok(self)
    }

    /// ceil the Decimal into a new Decimal.
    pub fn ceil(&self) -> Res<Decimal> {
        if !self.negative {
            self.clone().round(0, RoundMode::Ceiling)
        } else {
            self.clone().round(0, RoundMode::Truncate)
        }
    }

    /// floor the Decimal into a new Decimal.
    pub fn floor(&self) -> Res<Decimal> {
        if !self.negative {
            self.clone().round(0, RoundMode::Truncate)
        } else {
            self.clone().round(0, RoundMode::Ceiling)
        }
    }

    /// Existing bounded constructors retain their infallible interface. The
    /// Grow workers use try_new and propagate structural/resource failures.
    fn new(int_cnt: usize, frac_cnt: usize, negative: bool) -> Decimal {
        Self::try_new(int_cnt, frac_cnt, negative)
            .expect("bounded Decimal construction or allocation failed")
    }

    fn try_new(int_cnt: usize, frac_cnt: usize, negative: bool) -> Result<Decimal> {
        checked_fraction(frac_cnt)?;
        let mut value = Decimal {
            int_cnt,
            frac_cnt,
            result_frac_cnt: frac_cnt,
            negative,
            word_buf: SmallVec::from_buf([0; WORD_BUF_LEN]),
        };
        value.try_ensure_storage()?;
        Ok(value)
    }

    fn int_words(&self) -> usize {
        self.int_cnt.div_ceil(DIGITS_PER_WORD)
    }

    fn frac_words(&self) -> usize {
        self.frac_cnt.div_ceil(DIGITS_PER_WORD)
    }

    /// Fallible equivalent of Clone: preserve every raw field and ALL
    /// initialized cells, including inactive heap cells and physical shapes
    /// outside strict logical admission. No normalization or re-import.
    fn try_clone_all(&self) -> Result<Self> {
        checked_word_extent(self.word_buf.len(), 0)?;
        let mut words: SmallVec<[u32; 9]> = SmallVec::new();
        words
            .try_reserve_exact(self.word_buf.len())
            .map_err(|_| decimal_resource_error("decimal clone reservation failed"))?;
        words.resize(self.word_buf.len(), 0);
        words.copy_from_slice(&self.word_buf);
        Ok(Self {
            int_cnt: self.int_cnt,
            frac_cnt: self.frac_cnt,
            result_frac_cnt: self.result_frac_cnt,
            negative: self.negative,
            word_buf: words,
        })
    }

    /// Fallible scratch copy for a value-producing worker. Inactive physical
    /// cells within the inline prefix are retained; unused heap capacity is
    /// not an operand or a reason to allocate more scratch.
    fn try_clone_for_worker(&self) -> Result<Self> {
        let mut value = Self::try_new(self.int_cnt, self.frac_cnt, self.negative)?;
        value.result_frac_cnt = checked_fraction(self.result_frac_cnt)?;
        let extent = checked_word_extent(self.int_words(), self.frac_words())?.max(WORD_BUF_LEN);
        value.word_buf[..extent].copy_from_slice(&self.word_buf[..extent]);
        Ok(value)
    }

    fn try_reserve_words(&mut self, words: usize) -> Result<()> {
        let extent = checked_word_extent(words, 0)?.max(WORD_BUF_LEN);
        if extent > self.word_buf.len() {
            self.word_buf
                .try_reserve_exact(extent - self.word_buf.len())
                .map_err(|_| decimal_resource_error("word reservation failed"))?;
            self.word_buf.resize(extent, 0);
        }
        Ok(())
    }

    /// Backing extent is independent of arithmetic capacity, including every
    /// non-Ok payload. Scratch words may be reserved separately by a worker.
    fn try_ensure_storage(&mut self) -> Result<()> {
        checked_fraction(self.frac_cnt)?;
        checked_fraction(self.result_frac_cnt)?;
        self.int_cnt
            .checked_add(self.frac_cnt)
            .ok_or_else(|| decimal_resource_error("storage precision count overflow"))?;
        let extent = checked_word_extent(self.int_words(), self.frac_words())?.max(WORD_BUF_LEN);
        self.try_reserve_words(extent)?;
        self.word_buf.truncate(extent);
        if extent == WORD_BUF_LEN && self.word_buf.spilled() {
            // Moving the nine initialized words back inline does not allocate.
            self.word_buf.shrink_to_fit();
        }
        Ok(())
    }

    // Grow-producing entrypoints stay private until the complete wide-value
    // consumer closure (including division and formatting) has been checked.
    fn try_add_exact(&self, rhs: &Self) -> Result<Self> {
        let outcome = if self.negative == rhs.negative {
            do_add_with_limit(self, rhs, WordLimit::Grow)?
        } else {
            do_sub_with_limit(self, rhs, WordLimit::Grow)?
        };
        Self::try_finish_exact(outcome, self.result_frac_cnt.max(rhs.result_frac_cnt))
    }

    fn try_sub_exact(&self, rhs: &Self) -> Result<Self> {
        let outcome = if self.negative == rhs.negative {
            do_sub_with_limit(self, rhs, WordLimit::Grow)?
        } else {
            do_add_with_limit(self, rhs, WordLimit::Grow)?
        };
        Self::try_finish_exact(outcome, self.result_frac_cnt.max(rhs.result_frac_cnt))
    }

    fn try_mul_exact(&self, rhs: &Self) -> Result<Self> {
        let outcome = do_mul_with_limit(self, rhs, WordLimit::Grow)?;
        let result_frac = outcome.result_frac_cnt;
        Self::try_finish_exact(outcome, result_frac)
    }

    fn try_div_rem_exact(&self, rhs: &Self) -> Result<Option<(Self, Self)>> {
        let Some(output) = divide_with_limit(
            self,
            rhs,
            DivisionRequest::IntegerPair,
            WordLimit::Grow,
            None,
        )?
        else {
            return Ok(None);
        };
        let quotient = output
            .quotient
            .ok_or_else(|| decimal_resource_error("missing requested integer quotient"))?;
        let remainder = output
            .remainder
            .ok_or_else(|| decimal_resource_error("missing requested exact remainder"))?;
        Ok(Some((
            Self::try_finish_exact(quotient, 0)?,
            Self::try_finish_exact(remainder, self.result_frac_cnt.max(rhs.result_frac_cnt))?,
        )))
    }

    fn try_div_round_exact(&self, count: i64, result_scale: u32) -> Result<Option<Self>> {
        if count < 0 {
            return Err(Error::InvalidDataType(
                "decimal AVG count must be nonnegative".to_owned(),
            ));
        }
        if count == 0 {
            return Ok(None);
        }
        let target = usize::try_from(result_scale)
            .map_err(|_| decimal_resource_error("AVG result scale exceeds indexing width"))?;
        let increment = target.checked_sub(self.result_frac_cnt).ok_or_else(|| {
            Error::InvalidDataType(
                "decimal AVG result scale is below the input visible scale".to_owned(),
            )
        })?;
        let retained = self
            .frac_cnt
            .checked_add(increment)
            .ok_or_else(|| decimal_resource_error("AVG retained scale overflow"))?;
        let frac_words = retained.div_ceil(DIGITS_PER_WORD);
        checked_fraction(checked_word_digits(frac_words)?)?;
        // Retain whole storage words, then record the visible target. There
        // is no extra guard digit and no rounding mutation of this payload.
        let Some(output) = divide_with_limit(
            self,
            &Self::from(count),
            DivisionRequest::RetainedQuotient { frac_words },
            WordLimit::Grow,
            Some(target),
        )?
        else {
            return Ok(None);
        };
        let quotient = output
            .quotient
            .ok_or_else(|| decimal_resource_error("missing requested AVG quotient"))?;
        Ok(Some(Self::try_finish_exact(quotient, target)?))
    }

    fn try_round_exact(&self, scale: i64, mode: RoundMode) -> Result<Self> {
        let stored_scale = if scale > 0 {
            checked_fraction(
                usize::try_from(scale)
                    .map_err(|_| decimal_resource_error("round scale exceeds indexing width"))?,
            )?
        } else {
            0
        };
        let result = self.try_clone_for_worker()?.round_with_limit(
            i128::from(scale),
            WordLimit::Grow,
            mode,
        )?;
        // Host exact-value construction normalizes successful zero, including
        // no-op/growth. Fixed wrappers keep their original raw-zero policy.
        Self::try_finish_exact(result, stored_scale)
    }

    fn try_finish_exact(outcome: Res<Self>, result_frac: usize) -> Result<Self> {
        let mut value = match outcome {
            Res::Ok(value) => value,
            Res::Truncated(_) | Res::Overflow(_) => {
                return Err(Error::InvalidDataType(
                    "Grow arithmetic produced a bounded numeric status".to_owned(),
                ));
            }
        };
        value.result_frac_cnt = checked_fraction(result_frac)?;
        // Canonicalize only a fresh successful Grow result. Transport and
        // legacy status payloads retain their original header/sign/bytes.
        let active = checked_word_extent(value.int_words(), value.frac_words())?;
        let (skip, int_digits) = value.remove_leading_zeroes(value.int_cnt);
        if skip > 0 {
            value.word_buf.copy_within(skip..active, 0);
            value.word_buf[active - skip..].fill(0);
        }
        value.int_cnt = if int_digits == 0 && value.frac_cnt == 0 {
            1
        } else {
            int_digits
        };
        if value.is_zero() {
            value.negative = false;
        }
        value.try_ensure_storage()?;
        let active = checked_word_extent(value.int_words(), value.frac_words())?;
        value.word_buf[active..].fill(0);
        Ok(value)
    }

    pub fn is_negative(&self) -> bool {
        self.negative
    }

    /// Creates a new decimal which is zero.
    pub fn zero() -> Decimal {
        Decimal::new(1, 0, false)
    }

    /// Given a precision count 'prec', get:
    ///  1. the index of first non-zero word in self.word_buf to hold the
    ///     leading 'prec' number of     digits
    ///  2. the number of remained digits if we remove all leading zeros for the
    ///     leading 'prec'     number of digits
    fn remove_leading_zeroes(&self, prec: usize) -> (usize, usize) {
        let mut cnt = prec;
        let mut i = if cnt == 0 {
            DIGITS_PER_WORD
        } else {
            (cnt - 1) % DIGITS_PER_WORD + 1
        };
        let mut word_idx = 0;
        while cnt > 0 && self.word_buf[word_idx] == 0 {
            cnt -= i;
            i = DIGITS_PER_WORD;
            word_idx += 1;
        }
        if cnt > 0 {
            cnt -= count_leading_zeroes((cnt - 1) % DIGITS_PER_WORD, self.word_buf[word_idx])
        }
        (word_idx, cnt)
    }

    /// Prepare a buf for string output.
    fn write_storage_into(&self, text: &mut DecimalTextWriter<'_>) -> fmt::Result {
        let (mut word_start, mut int_digits) = self.remove_leading_zeroes(self.int_cnt);
        if int_digits == 0 && self.frac_cnt == 0 {
            // Keep the legacy virtual integer cell for empty/zero storage.
            int_digits = 1;
            word_start = 0;
        }
        if self.negative {
            text.byte(b'-')?;
        }
        let int_words = int_digits.div_ceil(DIGITS_PER_WORD);
        if int_digits > 0 {
            let first_digits = (int_digits - 1) % DIGITS_PER_WORD + 1;
            text.word(self.word_buf[word_start], first_digits, 0)?;
            for index in word_start + 1..word_start + int_words {
                text.word(self.word_buf[index], DIGITS_PER_WORD, 0)?;
            }
        } else {
            text.byte(b'0')?;
        }
        if self.frac_cnt > 0 {
            text.byte(b'.')?;
            let mut index = word_start + int_words;
            let mut remaining = self.frac_cnt;
            while remaining > 0 {
                let digits = remaining.min(DIGITS_PER_WORD);
                text.word(self.word_buf[index], digits, DIGITS_PER_WORD - digits)?;
                remaining -= digits;
                index += 1;
            }
        }
        Ok(())
    }

    fn write_storage(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        let mut text = DecimalTextWriter::new(out);
        self.write_storage_into(&mut text)?;
        text.finish()
    }

    /// Materialize the actual STORAGE emitter fallibly, including allowed raw
    /// shapes. Counting and writing share the emitter, not a guessed header
    /// length, a second formatter, or visible-scale padding.
    fn try_storage_text(&self) -> Result<String> {
        let mut count = DecimalTextCounter::default();
        self.write_storage(&mut count)
            .map_err(|_| decimal_resource_error("storage text byte count overflow"))?;
        let mut text = String::new();
        text.try_reserve_exact(count.bytes)
            .map_err(|_| decimal_resource_error("storage text reservation failed"))?;
        self.write_storage(&mut text)
            .map_err(|_| decimal_resource_error("storage text emission failed"))?;
        Ok(text)
    }

    fn write_result(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        if self.result_frac_cnt < self.frac_cnt {
            let rounded = self
                .try_round_exact(i64::from(self.result_scale()), RoundMode::HalfEven)
                .map_err(|_| fmt::Error)?;
            return rounded.write_storage(out);
        }
        // Presentation-only padding borrows the value. In particular a u32
        // visible scale is NOT a request for a dense decimal or temporary String.
        // Equal/growing scales preserve a raw/error zero's original sign.
        let mut text = DecimalTextWriter::new(out);
        self.write_storage_into(&mut text)?;
        if self.result_frac_cnt > self.frac_cnt {
            if self.frac_cnt == 0 {
                text.byte(b'.')?;
            }
            text.zeroes(self.result_frac_cnt - self.frac_cnt)?;
        }
        text.finish()
    }

    fn write_legacy_result(&self, out: &mut dyn fmt::Write) -> fmt::Result {
        let scale = u8::try_from(self.result_frac_cnt).map_err(|_| fmt::Error)?;
        let scale = i128::from(i8::from_ne_bytes([scale])).min(MAX_FRACTION as i128);
        let rounded = self
            .try_clone_for_worker()
            .map_err(|_| fmt::Error)?
            .round_with_limit(scale, WordLimit::Fixed(WORD_BUF_LEN), RoundMode::HalfEven)
            .map_err(|_| fmt::Error)?
            .unwrap();
        rounded.write_storage(out)
    }

    /// Get the least precision and fraction count to encode this decimal
    /// completely.
    pub fn prec_and_frac(&self) -> (usize, u32) {
        let (_, int_cnt) = self.remove_leading_zeroes(self.int_cnt);
        let prec = int_cnt + self.frac_cnt;
        (prec.max(1), self.storage_scale())
    }

    /// `frac_cnt` returns the full storage fraction count.
    pub fn frac_cnt(&self) -> u32 {
        self.storage_scale()
    }

    /// `digit_bounds` returns bounds of decimal digits in the number.
    fn digit_bounds(&self) -> (usize, usize) {
        let mut buf_beg = 0;
        let buf_len = self.int_words() + self.frac_words();
        if buf_len == 0 {
            return (0, 0);
        }
        let mut buf_end = buf_len - 1;

        while buf_beg < buf_len && self.word_buf[buf_beg] == 0 {
            buf_beg += 1;
        }
        if buf_beg >= buf_len {
            return (0, 0);
        }

        let mut i;
        let mut start = if buf_beg == 0 && self.int_cnt > 0 {
            i = (self.int_cnt - 1) % DIGITS_PER_WORD;
            DIGITS_PER_WORD - i - 1
        } else {
            i = DIGITS_PER_WORD - 1;
            buf_beg * DIGITS_PER_WORD
        };
        if buf_beg < buf_len {
            start += count_leading_zeroes(i, self.word_buf[buf_beg]);
        }

        while buf_end > buf_beg && self.word_buf[buf_end] == 0 {
            buf_end -= 1;
        }
        let (i, mut end) = if buf_end == buf_len - 1 && self.frac_cnt > 0 {
            i = (self.frac_cnt - 1) % DIGITS_PER_WORD + 1;
            (DIGITS_PER_WORD - i + 1, buf_end * DIGITS_PER_WORD + i)
        } else {
            (1, (buf_end + 1) * DIGITS_PER_WORD)
        };
        end -= count_trailing_zeroes(i, self.word_buf[buf_end]);
        (start, end)
    }

    /// `do_mini_left_shift` does left shift for alignment of data in buffer.
    ///
    /// Result fitting in the buffer should be garanted.
    /// 'shift' have to be from 1 to DIGITS_PER_WORD - 1 (inclusive)
    fn do_mini_left_shift(mut self, shift: usize, beg: usize, end: usize) -> Decimal {
        let shift = shift as usize;
        let mut buf_from = (beg / DIGITS_PER_WORD) as usize;
        let buf_end = ((end - 1) / DIGITS_PER_WORD) as usize;
        let c_shift = DIGITS_PER_WORD as usize - shift;
        if beg % DIGITS_PER_WORD < shift {
            self.word_buf[buf_from - 1] = self.word_buf[buf_from] / TEN_POW[c_shift];
        }
        while buf_from < buf_end {
            self.word_buf[buf_from] = (self.word_buf[buf_from] % TEN_POW[c_shift]) * TEN_POW[shift]
                + self.word_buf[buf_from + 1] / TEN_POW[c_shift];
            buf_from += 1;
        }
        self.word_buf[buf_from] = (self.word_buf[buf_from] % TEN_POW[c_shift]) * TEN_POW[shift];
        self
    }

    /// `do_mini_right_shift` does right shift for alignment of data in buffer.
    ///
    /// Result fitting in the buffer should be garanted.
    /// 'shift' have to be from 1 to DIGITS_PER_WORD - 1 (inclusive)
    fn do_mini_right_shift(mut self, shift: usize, beg: usize, end: usize) -> Decimal {
        let shift = shift as usize;
        let mut buf_from = ((end - 1) / DIGITS_PER_WORD) as usize;
        let buf_end = (beg / DIGITS_PER_WORD) as usize;
        let c_shift = DIGITS_PER_WORD as usize - shift;
        if DIGITS_PER_WORD - ((end - 1) % DIGITS_PER_WORD + 1) < shift {
            self.word_buf[buf_from + 1] =
                (self.word_buf[buf_from] % TEN_POW[shift]) * TEN_POW[c_shift];
        }
        while buf_from > buf_end {
            self.word_buf[buf_from] = self.word_buf[buf_from] / TEN_POW[shift]
                + (self.word_buf[buf_from - 1] % TEN_POW[shift]) * TEN_POW[c_shift];
            buf_from -= 1;
        }
        self.word_buf[buf_from] /= TEN_POW[shift];
        self
    }

    /// Checked declaration bridge for codec callers. Either-UNSPECIFIED policy
    /// belongs to the caller; preserve the old ordering error before bounds.
    pub(in crate::codec) fn checked_declared_fixed_target(
        precision: isize,
        scale: isize,
    ) -> Result<(u8, u8)> {
        if precision < scale {
            return Err(Error::m_bigger_than_d(""));
        }
        let target_error = || {
            Error::InvalidDataType(format!(
                "decimal target ({precision},{scale}) is outside the nonnegative byte-count domain"
            ))
        };
        let prec = u8::try_from(precision).map_err(|_| target_error())?;
        let frac = u8::try_from(scale).map_err(|_| target_error())?;
        checked_fixed_decimal_target(prec, frac)?;
        Ok((prec, frac))
    }

    pub(in crate::codec) fn try_max_or_min_for_target(
        negative: bool,
        precision: u8,
        scale: u8,
    ) -> Result<Self> {
        let mut value = try_max_decimal(precision, scale)?;
        // Negation is not equivalent here: retain valid-target negative zero.
        value.negative = negative;
        Ok(value)
    }

    /// Borrowed checked bridge for nonnegative declared scales. Preserve all
    /// raw cells when copying and the legacy Fixed/MAX30 result/status policy.
    pub(in crate::codec) fn try_round_fixed(
        &self,
        scale: u8,
        mode: RoundMode,
    ) -> Result<Res<Self>> {
        self.try_clone_all()?.round_with_limit(
            i128::from(scale).min(MAX_FRACTION as i128),
            WordLimit::Fixed(WORD_BUF_LEN),
            mode,
        )
    }

    // TODO: remove this after merge the `refactor ScalarFunc::builtin_cast`
    //
    /// convert_to(ProduceDecWithSpecifiedTp in tidb)
    /// produces a new decimal according to `flen` and `decimal`.
    pub fn convert_to(self, ctx: &mut EvalContext, flen: u8, decimal: u8) -> Result<Decimal> {
        // Validate even a zero/no-op target before any saturation or narrowing.
        let (target_int_digits, _) = checked_fixed_decimal_target(flen, decimal)?;
        let (prec, frac) = self.prec_and_frac();
        if !self.is_zero() && prec - frac as usize > target_int_digits {
            let mut maximum = try_max_decimal(flen, decimal)?;
            maximum.negative = self.negative;
            return Ok(maximum);
            // TODO:select (cast 111 as decimal(1)) causes a warning in MySQL.
        }

        if frac == u32::from(decimal) {
            return Ok(self);
        }

        let tmp = self.try_clone_all()?;
        let ret = self
            .round_with_limit(
                i128::from(decimal).min(MAX_FRACTION as i128),
                WordLimit::Fixed(WORD_BUF_LEN),
                RoundMode::HalfEven,
            )?
            .unwrap();
        // TODO: process over_flow
        if !ret.is_zero() && frac > u32::from(decimal) && ret != tmp {
            // TODO handle InInsertStmt in ctx
            ctx.handle_truncate(true)?;
        }
        Ok(ret)
    }

    /// Round rounds the decimal to "frac" digits.
    ///
    /// NOTES
    ///  scale can be negative !
    ///  one TRUNCATED error (line XXX below) isn't treated very logical :(
    pub fn round(self, frac: i8, round_mode: RoundMode) -> Res<Decimal> {
        self.round_with_word_buf_len(isize::from(frac), WORD_BUF_LEN, round_mode)
    }

    fn round_with_word_buf_len(
        self,
        frac: isize,
        word_buf_len: usize,
        round_mode: RoundMode,
    ) -> Res<Decimal> {
        // This cap belongs to the legacy wrapper, not to exact rounding.
        self.round_with_limit(
            frac.min(MAX_FRACTION as isize) as i128,
            WordLimit::Fixed(word_buf_len),
            round_mode,
        )
        .expect("bounded Decimal rounding count or allocation failed")
    }

    fn round_with_limit(
        self,
        mut frac: i128,
        limit: WordLimit,
        round_mode: RoundMode,
    ) -> Result<Res<Decimal>> {
        let mut frac_words_to = if frac > 0 {
            let stored = checked_fraction(
                usize::try_from(frac)
                    .map_err(|_| decimal_resource_error("round scale exceeds indexing width"))?,
            )?;
            stored.div_ceil(DIGITS_PER_WORD) as i128
        } else {
            // Preserve the source negative-scale grouping, including -9=>0.
            (frac + 1) / DIGITS_PER_WORD as i128
        };
        let (int_word_cnt, frac_word_cnt) = (self.int_words(), self.frac_words());
        let clipped = match limit {
            WordLimit::Fixed(words) => int_word_cnt as i128 + frac_words_to > words as i128,
            WordLimit::Grow => false,
        };
        let mut res = if clipped {
            let WordLimit::Fixed(words) = limit else {
                unreachable!()
            };
            frac_words_to = words as i128 - int_word_cnt as i128;
            frac = frac_words_to * DIGITS_PER_WORD as i128;
            Res::Truncated(self)
        } else if self.int_cnt as i128 + frac < 0 {
            return Ok(Res::Ok(Self::zero()));
        } else {
            Res::Ok(self)
        };
        let selected_int_words = match limit {
            WordLimit::Fixed(words) => int_word_cnt.min(words),
            WordLimit::Grow => int_word_cnt,
        };
        res.int_cnt = checked_word_digits(selected_int_words)?;
        let retained_frac_words = usize::try_from(frac_words_to.max(0))
            .map_err(|_| decimal_resource_error("round retained word count overflow"))?;
        let word_buf_len = match limit {
            WordLimit::Fixed(words) => words,
            WordLimit::Grow => {
                checked_word_extent(int_word_cnt, frac_word_cnt.max(retained_frac_words))?
                    .max(WORD_BUF_LEN)
            }
        };
        // Extension writes precede the final header update, so initialize the
        // entire working window now. A carry grows it only when actually needed.
        res.try_reserve_words(word_buf_len)?;
        if frac_words_to > frac_word_cnt as i128 {
            let start = checked_word_extent(int_word_cnt, frac_word_cnt)?;
            let end = checked_word_extent(int_word_cnt, retained_frac_words)?;
            res.word_buf[start..end].fill(0);
            res.frac_cnt = checked_fraction(
                usize::try_from(frac)
                    .map_err(|_| decimal_resource_error("round storage count overflow"))?,
            )?;
            res.result_frac_cnt = res.frac_cnt;
            res.try_ensure_storage()?;
            return Ok(res);
        }
        if frac >= res.frac_cnt as i128 {
            res.frac_cnt = checked_fraction(
                usize::try_from(frac)
                    .map_err(|_| decimal_resource_error("round storage count overflow"))?,
            )?;
            res.result_frac_cnt = res.frac_cnt;
            res.try_ensure_storage()?;
            return Ok(res);
        }

        Decimal::handle_incr(
            res,
            int_word_cnt,
            frac_words_to,
            frac,
            frac_word_cnt,
            round_mode,
            limit,
        )
    }

    fn handle_incr(
        mut res: Res<Decimal>,
        int_word_cnt: usize,
        frac_words_to: i128,
        frac: i128,
        frac_word_cnt: usize,
        round_mode: RoundMode,
        limit: WordLimit,
    ) -> Result<Res<Decimal>> {
        // -1 is the intentional carry-before-first-word sentinel.
        let mut to_idx = int_word_cnt as i128 + frac_words_to - 1;
        if frac == frac_words_to * DIGITS_PER_WORD as i128 {
            let do_inc = match round_mode {
                // Notice: No support for ceiling mode now.
                RoundMode::Ceiling => {
                    // If any word after scale is not zero, do increment.
                    // e.g ceiling 3.0001 to scale 1, gets 3.1
                    let idx = to_idx + frac_word_cnt as i128 - frac_words_to;
                    if idx > to_idx {
                        res.word_buf[(to_idx + 1) as usize..=(idx as usize)]
                            .iter()
                            .any(|c| *c != 0)
                    } else {
                        false
                    }
                }
                RoundMode::HalfEven => {
                    // If first digit after scale is 5 and round even,
                    // do increment if digit at scale is odd.
                    res.word_buf[(to_idx + 1) as usize] / DIG_MASK >= 5
                }
                RoundMode::Truncate => false,
            };
            if do_inc {
                if to_idx >= 0 {
                    res.word_buf[to_idx as usize] += 1;
                } else {
                    to_idx += 1;
                    res.word_buf[to_idx as usize] = WORD_BASE;
                }
            } else if int_word_cnt as i128 + frac_words_to == 0 {
                return Ok(Res::Ok(Self::zero()));
            }
        } else {
            // TODO - fix this code as it won't work for CEILING mode
            let pos = usize::try_from(frac_words_to * DIGITS_PER_WORD as i128 - frac - 1)
                .ok()
                .filter(|position| *position < DIGITS_PER_WORD)
                .ok_or_else(|| {
                    decimal_resource_error("round partial-word digit position is invalid")
                })?;
            let word_index = usize::try_from(to_idx)
                .map_err(|_| decimal_resource_error("round partial-word index is negative"))?;
            let mut shifted_number = res.word_buf[word_index] / TEN_POW[pos];
            let dig_after_scale = shifted_number % 10;
            let round_digit = match round_mode {
                RoundMode::Ceiling => 0,
                RoundMode::HalfEven => 5,
                RoundMode::Truncate => 10,
            };
            if dig_after_scale > round_digit || (round_digit == 5 && dig_after_scale == 5) {
                shifted_number += 10;
            }
            res.word_buf[word_index] = TEN_POW[pos] * (shifted_number - dig_after_scale);
        }

        if frac_words_to < frac_word_cnt as i128 {
            let idx = if frac == 0 && int_word_cnt == 0 {
                1
            } else {
                usize::try_from(int_word_cnt as i128 + frac_words_to)
                    .map_err(|_| decimal_resource_error("round discard index is negative"))?
            };
            // Initialized Grow scratch is a clearing window, never a Fixed
            // arithmetic budget. Fixed keeps its original n-word window.
            let clear_end = match limit {
                WordLimit::Fixed(words) => words,
                WordLimit::Grow => res.word_buf.len(),
            };
            if idx < clear_end {
                res.word_buf[idx..clear_end].fill(0);
            }
        }

        Decimal::handle_carry(
            res,
            usize::try_from(to_idx)
                .map_err(|_| decimal_resource_error("round carry index is negative"))?,
            frac,
            frac_words_to,
            int_word_cnt,
            limit,
        )
    }

    fn handle_carry(
        mut dec: Res<Decimal>,
        mut to_idx: usize,
        mut frac: i128,
        mut frac_word_to: i128,
        int_word_cnt: usize,
        limit: WordLimit,
    ) -> Result<Res<Decimal>> {
        if dec.word_buf[to_idx] >= WORD_BASE {
            let mut carry = 1;
            dec.word_buf[to_idx] -= WORD_BASE;
            while carry == 1 && to_idx > 0 {
                to_idx -= 1;
                add(
                    dec.word_buf[to_idx],
                    0,
                    &mut carry,
                    &mut dec.word_buf[to_idx],
                );
            }
            if carry > 0 {
                if let WordLimit::Fixed(words) = limit {
                    if int_word_cnt as i128 + frac_word_to >= words as i128 {
                        frac_word_to -= 1;
                        frac = frac_word_to * DIGITS_PER_WORD as i128;
                        dec = Res::Truncated(dec.unwrap());
                    }
                }
                let frac_words = usize::try_from(frac_word_to.max(0))
                    .map_err(|_| decimal_resource_error("round carry fraction extent overflow"))?;
                let shift_words = checked_word_extent(int_word_cnt, frac_words)?;
                let shift_capacity = match limit {
                    WordLimit::Grow => {
                        let needed = shift_words.checked_add(1).ok_or_else(|| {
                            decimal_resource_error("round carry word count overflow")
                        })?;
                        dec.try_reserve_words(needed)?;
                        needed
                    }
                    WordLimit::Fixed(words) => words,
                };
                for i in (0..shift_words).rev() {
                    if i + 1 < shift_capacity {
                        dec.word_buf[i + 1] = dec.word_buf[i];
                    } else if !dec.is_overflow() {
                        dec = Res::Overflow(dec.unwrap());
                    }
                }
                to_idx = 0;
                dec.word_buf[0] = 1;
                let can_grow_integer = match limit {
                    WordLimit::Grow => true,
                    WordLimit::Fixed(words) => dec.int_cnt < checked_word_digits(words)?,
                };
                if can_grow_integer {
                    dec.int_cnt = dec.int_cnt.checked_add(1).ok_or_else(|| {
                        decimal_resource_error("round integer digit carry overflow")
                    })?;
                } else {
                    dec = Res::Overflow(dec.unwrap());
                }
            }
        } else {
            while dec.word_buf[to_idx] == 0 {
                if to_idx == 0 {
                    dec.int_cnt = if limit == WordLimit::Grow && frac > 0 {
                        0
                    } else {
                        1
                    };
                    dec.negative = false;
                    dec.frac_cnt = checked_fraction(
                        usize::try_from(frac.max(0))
                            .map_err(|_| decimal_resource_error("rounded zero scale overflow"))?,
                    )?;
                    dec.result_frac_cnt = dec.frac_cnt;
                    // Fixed retains the old extra integer-zero header and
                    // status cancellation. Fresh Grow zero uses its canonical
                    // fractional-only shape without a speculative heap word.
                    dec.try_ensure_storage()?;
                    let clear = if limit == WordLimit::Grow {
                        checked_word_extent(dec.int_words(), dec.frac_words())?
                    } else {
                        usize::try_from((frac_word_to + 1).max(0)).map_err(|_| {
                            decimal_resource_error("rounded zero clearing extent overflow")
                        })?
                    };
                    dec.word_buf[..clear].fill(0);
                    return Ok(Res::Ok(dec.unwrap()));
                }
                to_idx -= 1;
            }
        }
        let first_dig = dec.int_cnt % DIGITS_PER_WORD;
        if first_dig > 0 && dec.word_buf[to_idx] >= TEN_POW[first_dig] {
            dec.int_cnt = dec
                .int_cnt
                .checked_add(1)
                .ok_or_else(|| decimal_resource_error("round integer precision overflow"))?;
        }
        dec.frac_cnt = checked_fraction(
            usize::try_from(frac.max(0))
                .map_err(|_| decimal_resource_error("rounded storage scale overflow"))?,
        )?;
        dec.result_frac_cnt = dec.frac_cnt;
        dec.try_ensure_storage()?;
        Ok(dec)
    }

    /// `shift` shifts decimal digits in given number (with rounding if it
    /// need), shift > 0 means shift to left shift, shift < 0 means right
    /// shift.
    ///
    /// In fact it is multiplying on 10^shift.
    pub fn shift(self, shift: isize) -> Res<Decimal> {
        self.shift_with_word_buf_len(shift, WORD_BUF_LEN)
    }

    fn shift_with_word_buf_len(self, shift: isize, word_buf_len: usize) -> Res<Decimal> {
        self.shift_with_limit(
            shift as i128,
            WordLimit::Fixed(word_buf_len),
            ShiftDisposition::Legacy,
        )
        .expect("bounded Decimal shift count or allocation failed")
        .result
    }

    fn shift_with_limit(
        self,
        shift: i128,
        limit: WordLimit,
        disposition: ShiftDisposition,
    ) -> Result<DecimalShiftOutcome> {
        i64::try_from(shift).map_err(|_| decimal_resource_error("shift count exceeds i64"))?;
        if shift == 0 {
            return Ok(DecimalShiftOutcome::direct(Res::Ok(self)));
        }
        let input_words = checked_word_extent(self.int_words(), self.frac_words())?;
        // digit_bounds uses word-aligned positions; validate those counts
        // before its usize arithmetic, independently of allocation capacity.
        checked_word_digits(input_words)?;
        let (mut beg, mut end) = self.digit_bounds();
        if beg == end {
            return Ok(DecimalShiftOutcome::direct(Res::Ok(Self::zero_for_shift(
                disposition,
            )?)));
        }
        if let WordLimit::Fixed(words) = limit {
            if disposition == ShiftDisposition::Legacy {
                let upper = checked_word_digits(words)? as i128 * 2;
                if shift > upper {
                    return Ok(DecimalShiftOutcome::direct(Res::Overflow(self)));
                }
                if shift < -upper {
                    return Ok(DecimalShiftOutcome::direct(Res::Truncated(
                        Self::zero_for_shift(disposition)?,
                    )));
                }
            }
        }
        let point = checked_word_digits(self.int_words())? as i128;
        let mut new_point = point + shift;
        let int_cnt = (new_point - beg as i128).max(0);
        let mut frac_cnt = (end as i128 - new_point).max(0);
        if let WordLimit::Fixed(words) = limit {
            if int_cnt > checked_word_digits(words)? as i128 {
                return Ok(DecimalShiftOutcome::direct(Res::Overflow(self)));
            }
        }
        let int_words = checked_decimal_position(int_cnt)?.div_ceil(DIGITS_PER_WORD);
        let desired_frac_words = if frac_cnt == 0 {
            0
        } else {
            (frac_cnt - 1) / DIGITS_PER_WORD as i128 + 1
        };
        let fraction_request = match limit {
            WordLimit::Grow => checked_decimal_position(desired_frac_words)?,
            WordLimit::Fixed(words) => {
                checked_decimal_position(desired_frac_words.min(words.saturating_add(1) as i128))?
            }
        };
        let selected = limit.apply(int_words, fraction_request)?;
        let selected_frac_words = selected.1;
        let clipped = selected.is_truncated();
        let word_buf_len = match limit {
            WordLimit::Fixed(words) => words,
            WordLimit::Grow => checked_word_extent(int_words, selected_frac_words)?
                .max(input_words)
                .max(WORD_BUF_LEN),
        };
        let window_digits = checked_word_digits(word_buf_len)? as i128;
        // Preselection precedes all expansion/reservation, including huge
        // in-range exponent equalities. Nothing allocates in proportion to a
        // rejected Fixed exponent.
        let mut res = if clipped {
            let retained_digits = checked_word_digits(selected_frac_words)? as i128;
            let diff = frac_cnt - retained_digits;
            frac_cnt = retained_digits;
            let rounded_end = end as i128 - diff;
            if disposition == ShiftDisposition::Legacy && rounded_end <= beg as i128 {
                return Ok(DecimalShiftOutcome::direct(Res::Truncated(
                    Self::zero_for_shift(disposition)?,
                )));
            }
            let mut round_scale = rounded_end - point;
            if disposition == ShiftDisposition::Legacy {
                round_scale = round_scale.min(MAX_FRACTION as i128);
            }
            let rounded = self.round_with_limit(round_scale, limit, RoundMode::HalfEven)?;
            if disposition == ShiftDisposition::Mysql && !rounded.is_ok() {
                // Go traces a nested Round error. Its caller's direct error
                // comparison does not saturate this Overflow's partial value.
                return Ok(DecimalShiftOutcome {
                    result: rounded,
                    origin: ShiftStatusOrigin::Rounding,
                });
            }
            if rounded_end <= beg as i128 {
                return Ok(DecimalShiftOutcome::direct(Res::Truncated(
                    Self::zero_for_shift(disposition)?,
                )));
            }
            end = checked_decimal_position(rounded_end)?;
            Res::Truncated(rounded.unwrap())
        } else {
            Res::Ok(self)
        };
        checked_fraction(checked_decimal_position(frac_cnt)?)?;
        res.try_reserve_words(word_buf_len)?;

        if shift % DIGITS_PER_WORD as i128 != 0 {
            let (l_mini_shift, r_mini_shift, mini_shift, do_left);
            if shift > 0 {
                l_mini_shift = checked_decimal_position(shift % DIGITS_PER_WORD as i128)?;
                r_mini_shift = DIGITS_PER_WORD - l_mini_shift;
                do_left = l_mini_shift <= beg;
            } else {
                r_mini_shift = checked_decimal_position((-shift) % DIGITS_PER_WORD as i128)?;
                l_mini_shift = DIGITS_PER_WORD - r_mini_shift;
                do_left = window_digits - (end as i128) < r_mini_shift as i128;
            }
            if do_left {
                if beg < l_mini_shift {
                    return Err(decimal_resource_error(
                        "left mini-shift precedes initialized words",
                    ));
                }
                res = res.map(|d| d.do_mini_left_shift(l_mini_shift, beg, end));
                mini_shift = -(l_mini_shift as i128);
            } else {
                if end as i128 + r_mini_shift as i128 > window_digits {
                    return Err(decimal_resource_error(
                        "right mini-shift exceeds selected word window",
                    ));
                }
                res = res.map(|d| d.do_mini_right_shift(r_mini_shift, beg, end));
                mini_shift = r_mini_shift as i128;
            }
            new_point += mini_shift;
            if shift + mini_shift == 0 && (new_point - int_cnt) < DIGITS_PER_WORD as i128 {
                res.int_cnt = checked_decimal_position(int_cnt)?;
                res.frac_cnt = checked_fraction(checked_decimal_position(frac_cnt)?)?;
                res.try_ensure_storage()?;
                return Ok(DecimalShiftOutcome::direct(res));
            }
            beg = checked_decimal_position(beg as i128 + mini_shift)?;
            end = checked_decimal_position(end as i128 + mini_shift)?;
        }

        let new_front = new_point - int_cnt;
        if new_front >= DIGITS_PER_WORD as i128 || new_front < 0 {
            let word_shift;
            if new_front > 0 {
                let words = checked_decimal_position(new_front / DIGITS_PER_WORD as i128)?;
                let to = (beg / DIGITS_PER_WORD)
                    .checked_sub(words)
                    .ok_or_else(|| decimal_resource_error("left shift target underflow"))?;
                let source_end = (end - 1) / DIGITS_PER_WORD;
                let barrier = source_end
                    .checked_sub(words)
                    .ok_or_else(|| decimal_resource_error("left shift end underflow"))?;
                for i in to..=barrier {
                    res.word_buf[i] = res.word_buf[i + words];
                }
                for i in barrier + 1..=source_end {
                    res.word_buf[i] = 0;
                }
                word_shift = -(words as i128);
            } else {
                let words = checked_decimal_position((1 - new_front) / DIGITS_PER_WORD as i128)?;
                let to = checked_word_extent((end - 1) / DIGITS_PER_WORD, words)?;
                let source_start = beg / DIGITS_PER_WORD;
                let barrier = checked_word_extent(source_start, words)?;
                if to >= res.word_buf.len() {
                    return Err(decimal_resource_error(
                        "right shift exceeds initialized words",
                    ));
                }
                for i in (barrier..=to).rev() {
                    res.word_buf[i] = res.word_buf[i - words];
                }
                for i in source_start..barrier {
                    res.word_buf[i] = 0;
                }
                word_shift = words as i128;
            }
            let shift_digits = word_shift * DIGITS_PER_WORD as i128;
            beg = checked_decimal_position(beg as i128 + shift_digits)?;
            end = checked_decimal_position(end as i128 + shift_digits)?;
            new_point += shift_digits;
        }
        let beg_word = beg / DIGITS_PER_WORD;
        let end_word = (end - 1) / DIGITS_PER_WORD;
        if new_point < 0 {
            return Err(decimal_resource_error(
                "shift retained a negative decimal point",
            ));
        }
        let new_point_word = if new_point != 0 {
            checked_decimal_position((new_point - 1) / DIGITS_PER_WORD as i128)?
        } else {
            0
        };
        if new_point_word > end_word {
            if new_point_word >= res.word_buf.len() {
                return Err(decimal_resource_error(
                    "shift zero fill exceeds initialized words",
                ));
            }
            for i in end_word + 1..=new_point_word {
                res.word_buf[i] = 0;
            }
        } else {
            for i in new_point_word..beg_word {
                res.word_buf[i] = 0;
            }
        }
        res.int_cnt = checked_decimal_position(int_cnt)?;
        res.frac_cnt = checked_fraction(checked_decimal_position(frac_cnt)?)?;
        res.try_ensure_storage()?;
        Ok(DecimalShiftOutcome::direct(res))
    }

    /// `as_i64` returns int part of the decimal.
    pub fn as_i64(&self) -> Res<i64> {
        let mut x = 0i64;
        let int_word_cnt = self.int_words();
        for word_idx in 0..int_word_cnt {
            let y = x;
            x = x
                .wrapping_mul(i64::from(WORD_BASE))
                .wrapping_sub(i64::from(self.word_buf[word_idx]));
            if y < i64::MIN / i64::from(WORD_BASE) || x > y {
                if self.negative {
                    return Res::Overflow(i64::MIN);
                }
                return Res::Overflow(i64::MAX);
            }
        }
        if !self.negative && x == i64::MIN {
            return Res::Overflow(i64::MAX);
        }
        if !self.negative {
            x = -x;
        }
        for word_idx in int_word_cnt..int_word_cnt + self.frac_words() {
            if self.word_buf[word_idx] != 0 {
                return Res::Truncated(x);
            }
        }
        Res::Ok(x)
    }

    /// `as_i64_with_ctx` returns int part of the decimal.
    pub fn as_i64_with_ctx(&self, ctx: &mut EvalContext) -> Result<i64> {
        let res = self.as_i64();
        ctx.handle_truncate(res.is_truncated())?;
        res.into()
    }

    /// `as_u64` returns int part of the decimal
    pub fn as_u64(&self) -> Res<u64> {
        if self.negative {
            return Res::Overflow(0);
        }
        let mut x = 0u64;
        let int_cnt = self.int_words();
        for word_idx in 0..int_cnt {
            x = match x.overflowing_mul(u64::from(WORD_BASE)) {
                (_, true) => return Res::Overflow(u64::MAX),
                (x, _) => match x.overflowing_add(u64::from(self.word_buf[word_idx])) {
                    (_, true) => return Res::Overflow(u64::MAX),
                    (x, _) => x,
                },
            };
        }
        for word_idx in int_cnt..int_cnt + self.frac_words() {
            if self.word_buf[word_idx] != 0 {
                return Res::Truncated(x);
            }
        }
        Res::Ok(x)
    }

    pub fn from_f64(val: f64) -> Result<Decimal> {
        if val.is_infinite() {
            Err(invalid_type!("{} can't be convert to decimal'", val))
        } else {
            let r = val.to_string();
            Decimal::from_str(r.as_str())
        }
    }

    /// Returns a `Decimal` from a given bytes slice
    ///
    /// # Notes
    ///
    /// An error will be returned if the given input is as follows:
    /// 1. empty string
    /// 2. string which cannot be converted to decimal
    pub fn from_bytes(s: &[u8]) -> Result<Res<Decimal>> {
        Decimal::from_bytes_with_word_buf(s, WORD_BUF_LEN)
    }

    /// Returns a `Decimal` from a given bytes slice buffer and specified buffer
    /// length
    ///
    /// # Notes
    ///
    /// An error will be returned if the given input is as follows:
    /// 1. an empty string
    /// 2. a string which cannot be converted to decimal
    fn from_bytes_with_word_buf(s: &[u8], word_buf_len: usize) -> Result<Res<Decimal>> {
        let outcome = Self::parse_with_policy(s, DecimalParsePolicy::Legacy(word_buf_len))?;
        match outcome.status {
            DecimalParseStatus::Ok => Ok(Res::Ok(outcome.value)),
            DecimalParseStatus::Truncated => Ok(Res::Truncated(outcome.value)),
            DecimalParseStatus::Overflow => Ok(Res::Overflow(outcome.value)),
            DecimalParseStatus::BadNumber | DecimalParseStatus::TruncatedWrongValue => Err(
                Error::InvalidDataType("unexpected legacy decimal parse disposition".to_owned()),
            ),
        }
    }

    fn parse_mysql(s: &[u8]) -> Result<DecimalParseOutcome> {
        Self::parse_with_policy(s, DecimalParsePolicy::Mysql(WORD_BUF_LEN))
    }

    fn try_from_literal(text: &str) -> Result<Self> {
        Ok(Self::parse_with_policy(text.as_bytes(), DecimalParsePolicy::Canonical)?.value)
    }

    fn parse_with_policy(s: &[u8], policy: DecimalParsePolicy) -> Result<DecimalParseOutcome> {
        let start = s
            .iter()
            .position(|byte| match policy {
                DecimalParsePolicy::Legacy(_) => !byte.is_ascii_whitespace(),
                DecimalParsePolicy::Mysql(_) => *byte != b' ' && *byte != b'\t',
                DecimalParsePolicy::Canonical => true,
            })
            .unwrap_or(s.len());
        if start == s.len() && policy.legacy() {
            return Err(box_err!("\"{}\" is empty", escape(s)));
        }
        let mut bs = &s[start..];
        let negative = bs.first() == Some(&b'-');
        if matches!(bs.first(), Some(b'-' | b'+')) {
            bs = &bs[1..];
        }
        let int_end = first_non_digit(bs, 0);
        let has_dot = bs.get(int_end) == Some(&b'.');
        let frac_start = int_end + usize::from(has_dot);
        let end = if has_dot {
            first_non_digit(bs, frac_start)
        } else {
            int_end
        };
        let mut int_cnt = int_end;
        let mut frac_cnt = end - frac_start;
        if int_cnt == 0 && frac_cnt == 0 {
            return match policy {
                DecimalParsePolicy::Legacy(_) => {
                    Err(box_err!("\"{}\" is invalid number", escape(s)))
                }
                DecimalParsePolicy::Mysql(_) => Ok(DecimalParseOutcome {
                    value: Self::try_new(0, 0, false)?,
                    status: DecimalParseStatus::TruncatedWrongValue,
                }),
                DecimalParsePolicy::Canonical => Err(Error::InvalidDataType(
                    "invalid canonical decimal literal".to_owned(),
                )),
            };
        }
        if policy == DecimalParsePolicy::Canonical {
            if end != bs.len() {
                return Err(Error::InvalidDataType(
                    "canonical decimal literal has a suffix".to_owned(),
                ));
            }
            // Fixed planning must count leading zeroes. Canonical Grow has no
            // capacity disposition, so it can omit that needless integer prefix.
            let leading = bs[..int_end]
                .iter()
                .take_while(|byte| **byte == b'0')
                .count();
            int_cnt -= leading;
        }
        let selected = policy.limit().apply(
            int_cnt.div_ceil(DIGITS_PER_WORD),
            frac_cnt.div_ceil(DIGITS_PER_WORD),
        )?;
        let (int_words, frac_words) = (selected.0, selected.1);
        let mut status = if selected.is_overflow() {
            DecimalParseStatus::Overflow
        } else if selected.is_truncated() {
            DecimalParseStatus::Truncated
        } else {
            DecimalParseStatus::Ok
        };
        if !selected.is_ok() {
            frac_cnt = checked_word_digits(frac_words)?;
            if selected.is_overflow() {
                int_cnt = checked_word_digits(int_words)?;
            }
        }
        // Capacity selection happens before u32 storage validation/allocation.
        let mut value = Self::try_new(int_cnt, frac_cnt, negative)?;
        let mut inner = 0;
        let mut word_index = int_words;
        let mut word = 0;
        for byte in bs[int_end - int_cnt..int_end].iter().rev() {
            word += u32::from(byte - b'0') * TEN_POW[inner];
            inner += 1;
            if inner == DIGITS_PER_WORD {
                word_index -= 1;
                value.word_buf[word_index] = word;
                word = 0;
                inner = 0;
            }
        }
        if inner != 0 {
            word_index -= 1;
            value.word_buf[word_index] = word;
        }
        word_index = int_words;
        word = 0;
        inner = 0;
        for byte in &bs[frac_start..frac_start + frac_cnt] {
            word = u32::from(byte - b'0') + word * 10;
            inner += 1;
            if inner == DIGITS_PER_WORD {
                value.word_buf[word_index] = word;
                word_index += 1;
                word = 0;
                inner = 0;
            }
        }
        if inner != 0 {
            value.word_buf[word_index] = word * TEN_POW[DIGITS_PER_WORD - inner];
        }

        if end < bs.len() {
            if matches!(bs[end], b'e' | b'E') {
                let exponent = scan_decimal_exponent(&bs[end + 1..], policy.legacy())?;
                if exponent.status != DecimalParseStatus::Ok {
                    status = exponent.status;
                    if status == DecimalParseStatus::BadNumber {
                        value = Self::try_new(0, 0, false)?;
                    }
                }
                // The legacy private n-word parser historically shifts and
                // saturates at nine words. Source policy uses n throughout.
                let (shift_words, disposition) = match policy {
                    DecimalParsePolicy::Legacy(_) => (WORD_BUF_LEN, ShiftDisposition::Legacy),
                    DecimalParsePolicy::Mysql(words) => (words, ShiftDisposition::Mysql),
                    DecimalParsePolicy::Canonical => unreachable!(),
                };
                if exponent.value > i64::from(i32::MAX) / 2 {
                    value = Self::try_max_for_words(shift_words, value.negative)?;
                    status = DecimalParseStatus::Overflow;
                }
                if exponent.value < i64::from(i32::MIN) / 2
                    && status != DecimalParseStatus::Overflow
                {
                    value = Self::zero_for_shift(disposition)?;
                    status = DecimalParseStatus::Truncated;
                }
                if status != DecimalParseStatus::Overflow {
                    let shifted = value.shift_with_limit(
                        i128::from(exponent.value),
                        WordLimit::Fixed(shift_words),
                        disposition,
                    )?;
                    let saturate = policy.legacy() || shifted.origin == ShiftStatusOrigin::Direct;
                    match shifted.result {
                        Res::Ok(shifted) => value = shifted,
                        Res::Truncated(shifted) => {
                            value = shifted;
                            status = DecimalParseStatus::Truncated;
                        }
                        Res::Overflow(shifted) => {
                            // Go wraps a nested Round error; FromString's direct
                            // ErrOverflow comparison then retains its partial
                            // value instead of saturating. Preserve that event
                            // origin here, never on the owning Decimal itself.
                            value = if saturate {
                                Self::try_max_for_words(shift_words, shifted.negative)?
                            } else {
                                shifted
                            };
                            status = DecimalParseStatus::Overflow;
                        }
                    }
                }
            } else {
                let junk = if policy.legacy() {
                    bs[end..].iter().any(|byte| !byte.is_ascii_whitespace())
                } else {
                    !trim_unicode_space(&bs[end..]).is_empty()
                };
                if junk {
                    status = DecimalParseStatus::Truncated;
                }
            }
        }
        value.result_frac_cnt = value.frac_cnt;
        if policy == DecimalParsePolicy::Canonical {
            value = Self::try_finish_exact(Res::Ok(value), frac_cnt)?;
        } else {
            let zero = match policy {
                DecimalParsePolicy::Legacy(_) => value.word_buf.iter().all(|word| *word == 0),
                DecimalParsePolicy::Mysql(words) => {
                    value.word_buf.iter().take(words).all(|word| *word == 0)
                }
                DecimalParsePolicy::Canonical => unreachable!(),
            };
            if zero {
                value.negative = false;
            }
        }
        Ok(DecimalParseOutcome { value, status })
    }

    fn try_max_for_words(words: usize, negative: bool) -> Result<Self> {
        let mut value = Self::try_new(checked_word_digits(words)?, 0, negative)?;
        value.word_buf[..words].fill(WORD_MAX);
        Ok(value)
    }

    fn zero_for_shift(disposition: ShiftDisposition) -> Result<Self> {
        match disposition {
            ShiftDisposition::Legacy => Ok(Self::zero()),
            ShiftDisposition::Mysql => Self::try_new(0, 0, false),
        }
    }

    /// Get the approximate needed capacity to encode this decimal.
    ///
    /// see also `encode_decimal`.
    pub fn approximate_encoded_size(&self) -> usize {
        let (prec, frac) = self.prec_and_frac();
        let (Ok(prec), Ok(frac)) = (u8::try_from(prec), u8::try_from(frac)) else {
            // A wire-sized estimate is not a logical owner's heap footprint.
            return 3;
        };
        dec_encoded_len(&[prec, frac]).unwrap_or(3)
    }

    pub fn div(&self, rhs: &Decimal, frac_incr: u8) -> Option<Res<Decimal>> {
        let frac_incr = usize::from(frac_incr);
        let result_frac_cnt =
            cmp::min(self.result_frac_cnt.saturating_add(frac_incr), MAX_FRACTION);
        let mut res = do_div_mod_impl(self, rhs, frac_incr, false, Some(result_frac_cnt));
        if let Some(ref mut dec) = res {
            dec.result_frac_cnt = result_frac_cnt;
        }
        res
    }

    pub fn is_zero(&self) -> bool {
        let len = self.int_words() + self.frac_words();
        self.word_buf[0..len as usize].iter().all(|&x| x == 0)
    }

    /// Returns the result/display scale, independently of stored fraction
    /// digits.
    pub fn result_frac_cnt(&self) -> u32 {
        self.result_scale()
    }
}

macro_rules! enable_conv_for_int {
    ($s:ty, $t:ty) => {
        impl From<$s> for Decimal {
            fn from(t: $s) -> Decimal {
                #[allow(clippy::cast_lossless)]
                (t as $t).into()
            }
        }
    };
}

enable_conv_for_int!(u32, u64);
enable_conv_for_int!(u16, u64);
enable_conv_for_int!(u8, u64);
enable_conv_for_int!(i32, i64);
enable_conv_for_int!(i16, i64);
enable_conv_for_int!(i8, i64);
enable_conv_for_int!(usize, u64);
enable_conv_for_int!(isize, i64);

impl ConvertTo<f64> for Decimal {
    /// Preserve native STORAGE-value Rust parsing, including infinity and
    /// signed zero. This is not the Go/TiDB result-scale projection policy.
    /// Count/allocation failures are outer codec errors, not SQL warnings.
    #[inline]
    fn convert(&self, _: &mut EvalContext) -> Result<f64> {
        Ok(self.try_storage_text()?.parse::<f64>()?)
    }
}

impl From<i64> for Decimal {
    fn from(i: i64) -> Decimal {
        let (neg, mut d) = if i < 0 {
            (true, Decimal::from(i.overflowing_neg().0 as u64))
        } else {
            (false, Decimal::from(i as u64))
        };
        d.negative = neg;
        d
    }
}

impl From<u64> for Decimal {
    fn from(u: u64) -> Decimal {
        let (mut x, mut word_idx) = (u, 1);
        while x >= u64::from(WORD_BASE) {
            word_idx += 1;
            x /= u64::from(WORD_BASE);
        }
        let mut d = Decimal::new(word_idx * DIGITS_PER_WORD, 0, false);
        x = u;
        while word_idx > 0 {
            word_idx -= 1;
            d.word_buf[word_idx as usize] = (x % u64::from(WORD_BASE)) as u32;
            x /= u64::from(WORD_BASE);
        }
        d
    }
}

impl ConvertTo<Decimal> for i64 {
    #[inline]
    fn convert(&self, _: &mut EvalContext) -> Result<Decimal> {
        Ok(Decimal::from(*self))
    }
}

impl ConvertTo<Decimal> for u64 {
    #[inline]
    fn convert(&self, _: &mut EvalContext) -> Result<Decimal> {
        Ok(Decimal::from(*self))
    }
}

impl ConvertTo<Decimal> for f64 {
    /// Convert a float number to decimal.
    ///
    /// This function will use float's canonical string representation
    /// rather than the accurate value the float represent.
    #[inline]
    fn convert(&self, _: &mut EvalContext) -> Result<Decimal> {
        Decimal::from_f64(*self)
    }
}

impl ConvertTo<Decimal> for Real {
    #[inline]
    fn convert(&self, ctx: &mut EvalContext) -> Result<Decimal> {
        self.into_inner().convert(ctx)
    }
}

impl ConvertTo<Decimal> for &[u8] {
    // FIXME: the err handle is not exactly same as TiDB's,
    //  TiDB's seems has bug, fix this after fix TiDB's
    #[inline]
    fn convert(&self, ctx: &mut EvalContext) -> Result<Decimal> {
        let r = Decimal::from_bytes(self).unwrap_or_else(|_| Res::Ok(Decimal::zero()));
        let err = Error::overflow("DECIMAL", "");
        r.into_result_with_overflow_err(ctx, err)
    }
}

impl ConvertTo<Decimal> for std::borrow::Cow<'_, [u8]> {
    #[inline]
    fn convert(&self, ctx: &mut EvalContext) -> Result<Decimal> {
        self.as_ref().convert(ctx)
    }
}

impl ConvertTo<Decimal> for Bytes {
    #[inline]
    fn convert(&self, ctx: &mut EvalContext) -> Result<Decimal> {
        self.as_slice().convert(ctx)
    }
}

impl ConvertTo<Decimal> for Json {
    /// Port from TiDB's types.ConvertJSONToDecimal
    #[inline]
    fn convert(&self, ctx: &mut EvalContext) -> Result<Decimal> {
        self.as_ref().convert(ctx)
    }
}

impl ConvertTo<Decimal> for JsonRef<'_> {
    /// Port from TiDB's types.ConvertJSONToDecimal
    #[inline]
    fn convert(&self, ctx: &mut EvalContext) -> Result<Decimal> {
        match self.get_type() {
            JsonType::String => {
                Decimal::from_str(self.get_str()?).or_else(|e| {
                    ctx.handle_truncate_err(e)?;
                    // FIXME: if TiDB's MyDecimal::FromString return err,
                    //  it may has res. However, if TiKV's Decimal::from_str
                    //  return err, it has no res, so I return zero here,
                    //  but it may different from TiDB's MyDecimal::FromString
                    Ok(Decimal::zero())
                })
            }
            _ => {
                let r: f64 = self.convert(ctx)?;
                Decimal::from_f64(r)
            }
        }
    }
}

/// Get the first non-digit ascii char in `bs` from `start_idx`.
fn first_non_digit(bs: &[u8], start_idx: usize) -> usize {
    bs.iter()
        .skip(start_idx)
        .position(|c| !c.is_ascii_digit())
        .map_or_else(|| bs.len(), |s| s + start_idx)
}

impl FromStr for Decimal {
    type Err = Error;

    fn from_str(s: &str) -> Result<Decimal> {
        match Decimal::from_bytes(s.as_bytes())? {
            Res::Ok(d) => Ok(d),
            Res::Overflow(_) => Err(box_err!("parsing {} will overflow", s)),
            Res::Truncated(_) => Err(box_err!("parsing {} will truncated", s)),
        }
    }
}

impl crate::codec::convert::TryToStringValue for Decimal {
    fn try_to_string_value(&self) -> Result<String> {
        self.try_storage_text()
    }
}

impl ToStringValue for Decimal {
    fn to_string_value(&self) -> String {
        let (_, int_digits) = self.remove_leading_zeroes(self.int_cnt);
        let bytes = int_digits
            .max(1)
            .checked_add(self.frac_cnt)
            .and_then(|len| len.checked_add(usize::from(self.negative)))
            .and_then(|len| len.checked_add(usize::from(self.frac_cnt > 0)))
            .expect("Decimal storage text length overflow");
        let mut text = String::with_capacity(bytes);
        self.write_storage(&mut text)
            .expect("Decimal storage formatting failed");
        text
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let active = self
            .int_words()
            .checked_add(self.frac_words())
            .ok_or(fmt::Error)?;
        if active <= WORD_BUF_LEN && self.result_frac_cnt <= u8::MAX as usize {
            // Extensional legacy domain, including signed-byte interpretation
            // of raw result headers128..255; no operand-origin tag.
            self.write_legacy_result(fmt)
        } else {
            // Approved domain extension: overcapacity status payloads and
            // full-u32 visible scales use the same general result writer.
            self.write_result(fmt)
        }
    }
}

impl crate::codec::data_type::AsMySqlBool for Decimal {
    #[inline]
    fn as_mysql_bool(&self, _ctx: &mut EvalContext) -> crate::codec::Result<bool> {
        Ok(!self.is_zero())
    }
}

macro_rules! write_u8 {
    ($writer:ident, $b:expr, $written:ident) => {{
        let mut b = $b;
        if $written == 0 {
            b ^= 0x80;
        }
        $writer.write_bytes(&[b])?;
        $written += 1;
    }};
}

macro_rules! write_word {
    ($writer:expr, $word:expr, $size:expr, $written:ident) => {{
        let word = $word;
        let size = $size;
        let mut data: [u8; 4] = match size {
            1 => [word as u8, 0, 0, 0],
            2 => [(word >> 8) as u8, word as u8, 0, 0],
            3 => [(word >> 16) as u8, (word >> 8) as u8, word as u8, 0],
            4 => [
                (word >> 24) as u8,
                (word >> 16) as u8,
                (word >> 8) as u8,
                word as u8,
            ],
            _ => unreachable!(),
        };
        if $written == 0 {
            data[0] ^= 0x80;
        }
        ($writer).write_bytes(&data[..size as usize])?;
        $written += size;
    }};
}

pub trait DecimalEncoder: NumberEncoder {
    /// Encode decimal to comparable bytes.
    // TODO: resolve following warnings.
    fn write_decimal(&mut self, d: &Decimal, prec: u8, frac: u8) -> Result<Res<()>> {
        // Header validation must precede all writes. Keep the existing byte
        // target domain; this is not the decoder's nine-word admission check.
        dec_encoded_len(&[prec, frac])?;
        self.write_bytes(&[prec, frac])?;
        let (prec, frac) = (usize::from(prec), usize::from(frac));
        let mut mask = if d.negative { u32::MAX } else { 0 };
        let mut int_cnt = prec - frac;
        let int_word_cnt = int_cnt / DIGITS_PER_WORD;
        let leading_digits = (int_cnt - int_word_cnt * DIGITS_PER_WORD) as usize;

        let frac_word_cnt = frac / DIGITS_PER_WORD;
        let trailing_digits = (frac - frac_word_cnt * DIGITS_PER_WORD) as usize;
        let mut src_frac_word_cnt = d.frac_cnt / DIGITS_PER_WORD;
        let mut src_trailing_digits = (d.frac_cnt - src_frac_word_cnt * DIGITS_PER_WORD) as usize;

        let int_size = int_word_cnt * WORD_SIZE + DIG_2_BYTES[leading_digits];
        let mut frac_size = frac_word_cnt * WORD_SIZE + DIG_2_BYTES[trailing_digits];
        let src_frac_size = src_frac_word_cnt * WORD_SIZE + DIG_2_BYTES[src_trailing_digits];

        let (mut src_word_start_idx, src_int_cnt) = d.remove_leading_zeroes(d.int_cnt);
        if src_int_cnt + src_frac_size == 0 {
            mask = 0;
            int_cnt = 1;
        }

        let mut src_int_word_cnt = src_int_cnt / DIGITS_PER_WORD;
        let mut src_leading_digits = (src_int_cnt - src_int_word_cnt * DIGITS_PER_WORD) as usize;
        let src_int_size = src_int_word_cnt * WORD_SIZE + DIG_2_BYTES[src_leading_digits];

        let mut written: usize = 0;
        let mut res = Res::Ok(());

        if int_cnt < src_int_cnt {
            src_word_start_idx += (src_int_word_cnt - int_word_cnt) as usize;
            if src_leading_digits > 0 {
                src_word_start_idx += 1;
            }
            if leading_digits > 0 {
                src_word_start_idx -= 1;
            }
            src_int_word_cnt = int_word_cnt;
            src_leading_digits = leading_digits;
            res = Res::Overflow(());
            error!(
                "encode decimal overflow";
                "source_int_digits" => d.int_cnt,
                "source_storage_frac" => d.frac_cnt,
                "source_result_frac" => d.result_frac_cnt,
                "source_negative" => d.negative,
                "source_initialized_words" => d.word_buf.len(),
                "prec" => prec,
                "frac" => frac,
            );
        } else if int_size > src_int_size {
            for _ in src_int_size..int_size {
                write_u8!(self, mask as u8, written);
            }
        }

        if frac_size < src_frac_size {
            src_frac_word_cnt = frac_word_cnt;
            src_trailing_digits = trailing_digits;
            res = Res::Truncated(());
            warn!(
                "encode decimal truncated";
                "source_int_digits" => d.int_cnt,
                "source_storage_frac" => d.frac_cnt,
                "source_result_frac" => d.result_frac_cnt,
                "source_negative" => d.negative,
                "source_initialized_words" => d.word_buf.len(),
                "prec" => prec,
                "frac" => frac,
            );
        } else if frac_size > src_frac_size && src_trailing_digits > 0 {
            if frac_word_cnt == src_frac_word_cnt {
                src_trailing_digits = trailing_digits;
                frac_size = src_frac_size;
            } else {
                src_frac_word_cnt += 1;
                src_trailing_digits = 0;
            }
        }

        if src_leading_digits > 0 {
            let i = DIG_2_BYTES[src_leading_digits] as usize;
            let x = (d.word_buf[src_word_start_idx] % TEN_POW[src_leading_digits]) ^ mask;
            src_word_start_idx += 1;
            write_word!(self, x, i, written);
        }

        let stop = src_word_start_idx + src_int_word_cnt as usize + src_frac_word_cnt as usize;
        while src_word_start_idx < stop {
            write_word!(self, d.word_buf[src_word_start_idx] ^ mask, 4, written);
            src_word_start_idx += 1;
        }

        if src_trailing_digits > 0 {
            let i = DIG_2_BYTES[src_trailing_digits];
            let lim = if src_frac_word_cnt < frac_word_cnt {
                DIGITS_PER_WORD as usize
            } else {
                trailing_digits
            };
            while src_trailing_digits < lim && DIG_2_BYTES[src_trailing_digits] == i {
                src_trailing_digits += 1;
            }
            let x = (d.word_buf[src_word_start_idx]
                / TEN_POW[DIGITS_PER_WORD as usize - src_trailing_digits])
                ^ mask;
            write_word!(self, x, i as usize, written);
        }

        if frac_size > src_frac_size {
            for _ in (src_frac_size..frac_size).zip(written..(int_size + frac_size) as usize) {
                write_u8!(self, mask as u8, written);
            }
        }
        Ok(res)
    }

    #[inline]
    fn write_decimal_to_chunk(&mut self, v: &Decimal) -> Result<()> {
        let parts = v.fixed_parts(true)?;
        let mut cell = DecimalCell([0; DECIMAL_STRUCT_SIZE]);
        cell.0[..4].copy_from_slice(&[
            parts.int_digits,
            parts.frac_digits,
            parts.result_frac_digits,
            u8::from(parts.negative),
        ]);
        for (slot, word) in cell.0[4..].chunks_exact_mut(WORD_SIZE).zip(parts.words) {
            slot.copy_from_slice(&word.to_ne_bytes());
        }
        self.write_bytes(&cell.0)?;
        Ok(())
    }
}

impl<T: BufferWriter> DecimalEncoder for T {}

pub trait DecimalDatumPayloadChunkEncoder: NumberEncoder + DecimalEncoder {
    #[inline]
    fn write_decimal_to_chunk_by_datum_payload(&mut self, mut src_payload: &[u8]) -> Result<()> {
        let decimal = src_payload.read_decimal()?;
        self.write_decimal_to_chunk(&decimal)
    }
}

impl<T: BufferWriter> DecimalDatumPayloadChunkEncoder for T {}

// Mark as `#[inline]` since in many cases `size` is a constant.
#[inline]
fn read_word<T: BufferReader + ?Sized>(
    data: &mut T,
    size: usize,
    is_first: &mut bool,
) -> Result<u32> {
    // Note: In TiDB's implementation, the first byte to read is flipped:
    // dCopy[0] ^= 0x80
    //
    // In TiKV, we do zero copy so that we need `is_first` flag.
    let buf = data.bytes();
    if buf.len() < size {
        return Err(Error::unexpected_eof());
    }
    let mut first = buf[0];
    if *is_first {
        first ^= 0x80;
        *is_first = false;
    }
    let res = match size {
        1 => i32::from(first as i8) as u32,
        2 => ((i32::from(first as i8) << 8) + i32::from(buf[1])) as u32,
        3 => {
            if first & 128 > 0 {
                (255 << 24)
                    | (u32::from(first) << 16)
                    | (u32::from(buf[1]) << 8)
                    | u32::from(buf[2])
            } else {
                (u32::from(first) << 16) | (u32::from(buf[1]) << 8) | u32::from(buf[2])
            }
        }
        4 => {
            ((i32::from(first as i8) << 24)
                + (i32::from(buf[1]) << 16)
                + (i32::from(buf[2]) << 8)
                + i32::from(buf[3])) as u32
        }
        _ => unreachable!(),
    };
    data.advance(size);
    Ok(res)
}

pub trait DecimalDecoder: NumberDecoder {
    /// `read_decimal` decodes value encoded by `write_decimal`.
    fn read_decimal(&mut self) -> Result<Decimal> {
        if self.bytes().len() < 3 {
            return Err(box_err!("decimal too short: {} < 3", self.bytes().len()));
        }
        let (prec, frac_cnt) = (
            usize::from(self.read_u8().unwrap()),
            usize::from(self.read_u8().unwrap()),
        );

        if prec < frac_cnt {
            return Err(box_err!(
                "invalid decimal, precision {} < frac_cnt {}",
                prec,
                frac_cnt
            ));
        }

        let int_cnt = prec - frac_cnt;
        let int_word_cnt = int_cnt / DIGITS_PER_WORD;
        let leading_digits = (int_cnt - int_word_cnt * DIGITS_PER_WORD) as usize;
        let frac_word_cnt = frac_cnt / DIGITS_PER_WORD;
        let trailing_digits = (frac_cnt - frac_word_cnt * DIGITS_PER_WORD) as usize;
        let mut int_word_to = int_word_cnt;
        if leading_digits > 0 {
            int_word_to += 1;
        }
        let mut frac_word_to = frac_word_cnt;
        if trailing_digits > 0 {
            frac_word_to += 1;
        }
        let mask = if self.bytes()[0] & 0x80 > 0 {
            0
        } else {
            u32::MAX
        };
        let res = fix_word_cnt_err(int_word_to, frac_word_to, WORD_BUF_LEN);
        if !res.is_ok() {
            return Err(box_err!("decoding decimal failed: {:?}", res));
        }
        let mut d = Decimal::new(int_cnt, frac_cnt, mask != 0);
        d.result_frac_cnt = frac_cnt;
        let mut word_idx = 0;
        let mut is_first = true;
        if leading_digits > 0 {
            let i = DIG_2_BYTES[leading_digits];
            d.word_buf[word_idx] = read_word(self, i as usize, &mut is_first)? ^ mask;
            if d.word_buf[word_idx] >= TEN_POW[leading_digits + 1] {
                return Err(box_err!("invalid leading digits for decimal number"));
            }
            if d.word_buf[word_idx] != 0 {
                word_idx += 1;
            } else {
                d.int_cnt -= leading_digits;
            }
        }
        for _ in 0..int_word_cnt {
            d.word_buf[word_idx] = read_word(self, 4, &mut is_first)? ^ mask;
            if d.word_buf[word_idx] > WORD_MAX {
                return Err(box_err!("invalid int part for decimal number"));
            }
            if word_idx > 0 || d.word_buf[word_idx] != 0 {
                word_idx += 1;
            } else {
                d.int_cnt -= DIGITS_PER_WORD;
            }
        }
        for _ in 0..frac_word_cnt {
            d.word_buf[word_idx] = read_word(self, 4, &mut is_first)? ^ mask;
            if d.word_buf[word_idx] > WORD_MAX {
                return Err(box_err!("invalid frac part decimal number"));
            }
            word_idx += 1;
        }
        if trailing_digits > 0 {
            let x = read_word(self, DIG_2_BYTES[trailing_digits] as usize, &mut is_first)? ^ mask;
            d.word_buf[word_idx] =
                match x.checked_mul(TEN_POW[DIGITS_PER_WORD as usize - trailing_digits]) {
                    Some(v) if v <= WORD_MAX => v,
                    _ => {
                        return Err(box_err!("invalid trailing digits for decimal number"));
                    }
                }
        }
        if d.int_cnt == 0 && d.frac_cnt == 0 {
            d = Decimal::zero();
        }
        d.result_frac_cnt = frac_cnt;
        Ok(d)
    }

    /// `read_decimal_from_chunk` decode Decimal encoded by
    /// `write_decimal_to_chunk`.
    fn read_decimal_from_chunk(&mut self) -> Result<Decimal> {
        // Consume one complete cell, retaining the reader's following bytes.
        let buf = self.read_bytes(DECIMAL_STRUCT_SIZE)?;
        let negative = match buf[3] {
            0 => false,
            1 => true,
            byte => {
                return Err(Error::InvalidDataType(format!(
                    "invalid decimal sign byte {byte}"
                )));
            }
        };
        let mut words = [0; WORD_BUF_LEN];
        for (word, bytes) in words.iter_mut().zip(buf[4..].chunks_exact(WORD_SIZE)) {
            *word = u32::from_ne_bytes(bytes.try_into().expect("physical word is four bytes"));
        }
        Decimal::from_fixed_parts(
            DecimalParts {
                int_digits: buf[0],
                frac_digits: buf[1],
                result_frac_digits: buf[2],
                negative,
                words,
            },
            true,
        )
    }
}

impl<T: BufferReader> DecimalDecoder for T {}

impl PartialEq for Decimal {
    fn eq(&self, right: &Decimal) -> bool {
        self.cmp(right) == Ordering::Equal
    }
}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, right: &Decimal) -> Option<Ordering> {
        Some(self.cmp(right))
    }
}

impl Eq for Decimal {}

impl Ord for Decimal {
    fn cmp(&self, right: &Decimal) -> Ordering {
        if self.negative == right.negative {
            let (carry, ..) = calc_sub_carry(self, right);
            carry.map_or(Ordering::Equal, |carry| {
                if (carry > 0) == self.negative {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            })
        } else if self.negative {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    }
}

impl<'a> Add<&'a Decimal> for &Decimal {
    type Output = Res<Decimal>;

    fn add(self, rhs: &'a Decimal) -> Res<Decimal> {
        let result_frac_cnt = cmp::max(self.result_frac_cnt, rhs.result_frac_cnt);
        let mut res = if self.negative == rhs.negative {
            do_add(self, rhs)
        } else {
            do_sub(self, rhs)
        };
        res.result_frac_cnt = result_frac_cnt;
        res
    }
}

impl<'a> Sub<&'a Decimal> for &Decimal {
    type Output = Res<Decimal>;

    fn sub(self, rhs: &'a Decimal) -> Res<Decimal> {
        let result_frac_cnt = cmp::max(self.result_frac_cnt, rhs.result_frac_cnt);
        let mut res = if self.negative == rhs.negative {
            do_sub(self, rhs)
        } else {
            do_add(self, rhs)
        };
        res.result_frac_cnt = result_frac_cnt;
        res
    }
}

impl<'a> Mul<&'a Decimal> for &Decimal {
    type Output = Res<Decimal>;

    fn mul(self, rhs: &'a Decimal) -> Res<Decimal> {
        do_mul(self, rhs)
    }
}

impl<'a> Div<&'a Decimal> for &Decimal {
    type Output = Option<Res<Decimal>>;

    fn div(self, rhs: &'a Decimal) -> Self::Output {
        self.div(rhs, DEFAULT_DIV_FRAC_INCR)
    }
}

impl Rem for Decimal {
    type Output = Option<Res<Decimal>>;

    #[allow(clippy::op_ref)]
    fn rem(self, rhs: Decimal) -> Option<Res<Decimal>> {
        &self % &rhs
    }
}

impl<'a> Rem<&'a Decimal> for &Decimal {
    type Output = Option<Res<Decimal>>;
    fn rem(self, rhs: &'a Decimal) -> Self::Output {
        let result_frac_cnt = cmp::max(self.result_frac_cnt, rhs.result_frac_cnt);
        let mut res = do_div_mod_impl(self, rhs, 0, true, Some(result_frac_cnt));
        if let Some(ref mut dec) = res {
            dec.result_frac_cnt = result_frac_cnt;
        }
        res
    }
}

impl Neg for Decimal {
    type Output = Decimal;

    fn neg(mut self) -> Decimal {
        if !self.is_zero() {
            self.negative = !self.negative;
        }
        self
    }
}

impl Hash for Decimal {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let (int_word_cnt, frac_word_cnt) = (self.int_words(), self.frac_words());

        let (stop, mut idx) = (int_word_cnt as usize, 0usize);
        while idx < stop && self.word_buf[idx] == 0 {
            idx += 1;
        }
        let start = idx;
        let int_word_cnt = stop - idx;

        int_word_cnt.hash(state);
        let mut end = (stop + frac_word_cnt as usize) as isize - 1;
        // trims suffix 0(also trims the suffix 0 before the point
        // when there is no digit after point).
        while start as isize <= end && self.word_buf[end as usize] == 0 {
            end -= 1;
        }

        self.word_buf[start..((end + 1) as usize)].hash(state);
        // -0 should be not negative.
        let negative = self.negative && (start as isize <= end);
        negative.hash(state);
    }
}

#[cfg(test)]
mod native_binary_tests {
    use super::*;

    #[test]
    fn signed_fast_layout_and_unsupported_are_distinct() {
        let minimum = NativeDecimalFastValue {
            coefficient: i128::MIN,
            storage_scale: 2,
            scale: 1,
        };
        let shared = Decimal::try_from_native_fast(minimum, 256).unwrap();
        assert_eq!(shared.try_native_fast_value(256).unwrap(), Some(minimum));
        assert_eq!(
            native_decimal_fast_binary(minimum, minimum, NativeDecimalBinaryOp::Subtract),
            None
        );
        let zero = NativeDecimalFastValue {
            coefficient: 0,
            storage_scale: 40,
            scale: 30,
        };
        assert_eq!(
            Decimal::try_from_native_fast(zero, 256)
                .unwrap()
                .try_native_fast_value(256)
                .unwrap(),
            Some(zero)
        );
        assert!(matches!(
            shared.try_native_fast_value(1),
            Err(NativeDecimalError::Resource(_))
        ));
        let sum = native_decimal_fast_binary(
            NativeDecimalFastValue {
                coefficient: 12,
                storage_scale: 1,
                scale: 1,
            },
            NativeDecimalFastValue {
                coefficient: 3,
                storage_scale: 2,
                scale: 2,
            },
            NativeDecimalBinaryOp::Add,
        )
        .unwrap();
        assert_eq!(
            sum,
            NativeDecimalFastValue {
                coefficient: 123,
                storage_scale: 2,
                scale: 2
            }
        );
    }

    #[test]
    fn native_binary_double_carry_and_mysql_dispositions() {
        use NativeDecimalBinaryOp::*;
        use NativeDecimalBinaryPolicy::*;
        let nines =
            Decimal::try_from_native_digits(false, "9".repeat(27).as_bytes(), 0, 0, 4096).unwrap();
        let square = nines
            .try_native_binary(&nines, Multiply, Exact, 4096)
            .unwrap();
        assert!(square.is_ok());
        assert_eq!(
            square.to_string_value(),
            format!("{}8{}1", "9".repeat(26), "0".repeat(26))
        );
        // B=1e9: (B²-B-1)(B²-3B-1) = (B-4)B³ + B² + 4B + 1.
        let left =
            Decimal::try_from_native_digits(false, b"999999998999999999", 0, 0, 4096).unwrap();
        let right =
            Decimal::try_from_native_digits(false, b"999999996999999999", 0, 0, 4096).unwrap();
        assert_eq!(
            left.try_native_binary(&right, Multiply, Exact, 4096)
                .unwrap()
                .to_string_value(),
            "999999996000000001000000004000000001"
        );
        assert_eq!(
            native_decimal_coefficient_binary(b"0099", b"1", Add, 4096).unwrap(),
            b"0100"
        );
        assert_eq!(
            native_decimal_coefficient_binary(b"0100", b"1", Subtract, 4096).unwrap(),
            b"0099"
        );
        let magnitude = format!("1{}", "0".repeat(60));
        let positive =
            Decimal::try_from_native_digits(false, magnitude.as_bytes(), 0, 0, 4096).unwrap();
        let negative = positive
            .try_native_math(NativeDecimalOp::Negate, 4096)
            .unwrap();
        let overflow = negative
            .try_native_binary(&positive, Multiply, MySql, 4096)
            .unwrap();
        assert!(overflow.is_overflow());
        assert!(overflow.is_zero() && overflow.is_negative());
        // Each operand is 1 + 5e-40. Nine-word planning drops its final
        // fractional word; unlike wire multiplication native retains 72 places.
        let coefficient = format!("1{}5", "0".repeat(39));
        let fractional =
            Decimal::try_from_native_digits(false, coefficient.as_bytes(), 40, 20, 4096).unwrap();
        let clipped = fractional
            .try_native_binary(&fractional, Multiply, MySql, 4096)
            .unwrap();
        assert!(clipped.is_truncated());
        assert_eq!((clipped.storage_scale(), clipped.result_scale()), (72, 30));
        assert_eq!(clipped.to_string_value(), format!("1.{}", "0".repeat(72)));
    }
}

#[cfg(test)]
mod native_negate_tests {
    use super::*;

    #[test]
    fn native_negate_keeps_wide_scales_and_separate_wire_zero_policy() {
        // Hand-derived exact coefficient/sign boundaries, not provider output.
        let digits = format!("{}{}", "9".repeat(108), "1".repeat(120));
        let wide =
            Decimal::try_from_native_digits(false, digits.as_bytes(), 120, 31, 4096).unwrap();
        let negative = wide.try_native_math(NativeDecimalOp::Negate, 4096).unwrap();
        assert!(negative.is_negative());
        assert_eq!(
            (negative.storage_scale(), negative.result_scale()),
            (120, 31)
        );
        assert!(wide.words().words.len() > 9);
        assert_eq!(negative.words().int_digits, wide.words().int_digits);
        assert_eq!(negative.words().words, wide.words().words);
        let restored = negative
            .try_native_math(NativeDecimalOp::Negate, 4096)
            .unwrap();
        assert!(!restored.is_negative());
        assert_eq!(restored.words().words, wide.words().words);
        let zero = Decimal::try_from_native_digits(true, b"000", 3, 3, 1024).unwrap();
        assert!((-zero.clone()).is_negative()); // original wire Neg retains raw -0
        let native_zero = zero.try_native_math(NativeDecimalOp::Negate, 1024).unwrap();
        assert!(native_zero.is_zero());
        assert!(!native_zero.is_negative());
        assert_eq!(
            (native_zero.storage_scale(), native_zero.result_scale()),
            (3, 3)
        );
        assert!(matches!(
            wide.try_native_math(NativeDecimalOp::Negate, 1),
            Err(NativeDecimalError::Resource(_))
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::{cmp::Ordering, collections::hash_map::DefaultHasher, sync::Arc};

    use super::{DEFAULT_DIV_FRAC_INCR, WORD_BUF_LEN, *};
    use crate::{
        codec::error::*,
        expr::{EvalConfig, Flag},
    };

    #[test]
    fn test_native_math_five_policies_and_limits() {
        use NativeDecimalOp::*;
        let input = Decimal::try_from_native_digits(true, b"155", 1, 1, 1024).unwrap();
        for (operation, expected) in [
            (Abs, "15.5"),
            (Ceil, "-15"),
            (Floor, "-16"),
            (Round(0), "-16"),
            (Truncate(0), "-15"),
            (Round(-1), "-20"),
            (Truncate(-1), "-10"),
        ] {
            let output = input.try_native_math(operation, 1024).unwrap();
            assert_eq!(output.to_string_value(), expected);
        }
        let retained = input
            .try_native_round_with_storage(0, true, 4, 1024)
            .unwrap();
        assert_eq!(retained.to_string_value(), "-16.0000");
        assert_eq!((retained.storage_scale(), retained.result_scale()), (4, 0));
        let coefficient = format!("{}5", "9".repeat(108));
        let wide =
            Decimal::try_from_native_digits(false, coefficient.as_bytes(), 1, 1, 4096).unwrap();
        assert_eq!(
            wide.try_native_math(Round(0), 4096)
                .unwrap()
                .to_string_value(),
            format!("1{}", "0".repeat(108))
        );
        assert!(matches!(
            input.try_clone_native_math(1),
            Err(NativeDecimalError::Resource(_))
        ));
        assert!(matches!(
            input.try_native_math(Abs, 1),
            Err(NativeDecimalError::Resource(_))
        ));
        assert!(matches!(
            input.try_native_math(Round(i32::MAX), 64),
            Err(NativeDecimalError::Resource(_))
        ));
        assert!(matches!(
            Decimal::try_from_native_digits(false, b"1", 0, 0, 1),
            Err(NativeDecimalError::Resource(_))
        ));
        for (digits, storage, result) in [
            (b"".as_slice(), 0, 0),
            (b"x".as_slice(), 0, 0),
            (b"1".as_slice(), 2, 1),
            (b"1".as_slice(), 0, 1),
        ] {
            assert!(matches!(
                Decimal::try_from_native_digits(false, digits, storage, result, 1024),
                Err(NativeDecimalError::InvalidInput(_))
            ));
        }
        let failure = NativeDecimalError::Core(Error::InvalidDataType("original cause".to_owned()));
        let NativeDecimalError::Core(original) = &failure else {
            unreachable!()
        };
        let source = std::error::Error::source(&failure)
            .unwrap()
            .downcast_ref::<Error>()
            .unwrap();
        assert!(std::ptr::eq(original, source));
        assert_eq!(failure.to_string(), original.to_string());
    }

    #[test]
    fn test_native_math_scale_policy_follows_workspace_overflow() {
        use std::{hint::black_box, panic::catch_unwind};
        // Probe only the original arithmetic, never its enormous repeat/Vec.
        // This compares the actual compiled overflow policy, not a debug cfg.
        for storage in [0_u32, 1] {
            let original = catch_unwind(|| {
                let storage = black_box(storage);
                let target = black_box(i32::MIN);
                let shift = storage as i32 - target;
                assert!(shift <= 0);
                (target.max(0) as u32) - storage
            });
            let value = Decimal::try_from_native_digits(false, b"1", storage, storage, 64).unwrap();
            for operation in [
                NativeDecimalOp::Round(black_box(i32::MIN)),
                NativeDecimalOp::Truncate(black_box(i32::MIN)),
            ] {
                let actual = catch_unwind(|| value.try_native_math(operation, 64));
                match &original {
                    Err(_) => assert!(actual.is_err()),
                    Ok(0) => assert_eq!(actual.unwrap().unwrap().to_string_value(), "1"),
                    Ok(_) => assert!(matches!(
                        actual.unwrap(),
                        Err(NativeDecimalError::Resource(_))
                    )),
                }
            }
            let zero = Decimal::try_from_native_digits(true, b"0", storage, storage, 64).unwrap();
            let actual = catch_unwind(|| {
                zero.try_native_math(NativeDecimalOp::Round(black_box(i32::MIN)), 64)
            });
            if original.is_err() {
                assert!(actual.is_err());
            } else {
                let output = actual.unwrap().unwrap();
                assert!(output.is_zero());
                assert!(!output.is_negative());
            }
        }
    }

    #[test]
    fn test_default_is_valid_zero() {
        let value = Decimal::default();
        assert_eq!(
            value.try_to_parts().unwrap(),
            Decimal::zero().try_to_parts().unwrap()
        );
        assert_eq!(value.try_to_parts().unwrap().int_digits, 1);
        assert!(value.is_zero());
        assert!(!value.is_negative());
    }

    #[test]
    fn test_private_grow_integer_workers() {
        // Exact word input, not a second parser or a widened public admission.
        let mut power = Decimal::try_new(100, 0, false).unwrap();
        power.word_buf[0] = 1; // 10^99: one head digit and eleven zero words.
        let one = Decimal::from(1);
        let sum = power.try_add_exact(&one).unwrap();
        assert_eq!(sum.to_string_value(), format!("1{}1", "0".repeat(98)));
        assert_eq!(sum.words().int_digits, 100);
        let difference = power.try_sub_exact(&one).unwrap();
        assert_eq!(difference.to_string_value(), "9".repeat(99));
        assert_eq!(difference.words().int_digits, 99);
        let product = power.try_mul_exact(&power).unwrap();
        assert_eq!(product.to_string_value(), format!("1{}", "0".repeat(198)));
        assert_eq!(product.words().int_digits, 199);
        assert_eq!(sum.try_sub_exact(&one).unwrap(), power);
        assert_eq!(difference.try_add_exact(&one).unwrap(), power);
        let mut negative = power.clone();
        negative.negative = true;
        assert_eq!(
            negative.try_add_exact(&one).unwrap().to_string_value(),
            format!("-{}", "9".repeat(99))
        );
        assert_eq!(
            negative.try_mul_exact(&power).unwrap().to_string_value(),
            format!("-1{}", "0".repeat(198))
        );
    }

    #[test]
    fn test_private_grow_fraction_alignment() {
        for scale in [1, 8, 9, 10, 71, 72, 73, 81, 82, 100, 101, 255, 256, 300] {
            let mut value = Decimal::try_new(0, scale, false).unwrap();
            let words = value.frac_words();
            value.word_buf[words - 1] = TEN_POW[words * DIGITS_PER_WORD - scale];
            assert!(
                !value.is_zero(),
                "last initialized fraction word at scale {scale}"
            );
            let sum = value.try_add_exact(&value).unwrap();
            assert_eq!(
                sum.to_string_value(),
                format!("0.{}2", "0".repeat(scale - 1))
            );
            let product = value.try_mul_exact(&value).unwrap();
            assert_eq!(
                product.to_string_value(),
                format!("0.{}1", "0".repeat(scale * 2 - 1))
            );
            assert_eq!(product.storage_scale(), (scale * 2) as u32);
            assert_eq!(product.result_scale(), (scale * 2) as u32);
            let zero = value.try_sub_exact(&value).unwrap();
            assert!(zero.is_zero());
            assert!(!zero.is_negative());
            assert_eq!(
                (zero.storage_scale(), zero.result_scale()),
                (scale as u32, scale as u32)
            );
            assert_eq!(zero.to_string_value(), format!("0.{}", "0".repeat(scale)));
            let mut negative_zero = zero.clone();
            negative_zero.negative = true;
            let zero_product = value.try_mul_exact(&negative_zero).unwrap();
            assert!(zero_product.is_zero());
            assert!(!zero_product.is_negative());
            assert_eq!(zero_product.storage_scale(), (scale * 2) as u32);
        }
    }

    #[test]
    fn test_private_grow_hidden_scales_and_resource_checks() {
        let mut hidden = Decimal::from_str("0.000000001").unwrap();
        hidden.result_frac_cnt = 7;
        let sum = hidden.try_add_exact(&hidden).unwrap();
        assert_eq!(sum.to_string_value(), "0.000000002");
        assert_eq!((sum.storage_scale(), sum.result_scale()), (9, 7));
        let product = hidden.try_mul_exact(&hidden).unwrap();
        assert_eq!(product.to_string_value(), "0.000000000000000001");
        assert_eq!((product.storage_scale(), product.result_scale()), (18, 14));
        assert!(WordLimit::Grow.apply(usize::MAX, 1).is_err());
        assert!(WordLimit::Grow.apply(0, usize::MAX).is_err());
        assert_eq!(
            WordLimit::Fixed(9).apply(usize::MAX, usize::MAX).unwrap(),
            Res::Overflow((9, 0))
        );
        assert!(Decimal::try_new(usize::MAX, 1, false).is_err());
        if let Some(too_wide) = (u32::MAX as usize).checked_add(1) {
            assert!(Decimal::try_new(0, too_wide, false).is_err());
        }
        let mut huge_result = Decimal::from(1);
        huge_result.result_frac_cnt = u32::MAX as usize;
        assert!(
            huge_result
                .try_mul_exact(&Decimal::from_str("1.0").unwrap())
                .is_err()
        );
    }

    #[test]
    fn test_private_grow_comparison_hash_and_clone() {
        let mut value = Decimal::try_new(100, 101, false).unwrap();
        value.word_buf[0] = 1;
        let last = value.int_words() + value.frac_words() - 1;
        value.word_buf[last] = 10_000_000;
        let mut different = value.clone();
        different.word_buf[last] = 20_000_000;
        assert!(value < different);
        assert_eq!(value.word_buf[last], 10_000_000);
        let mut padded = value.clone();
        padded.frac_cnt += DIGITS_PER_WORD;
        padded.try_ensure_storage().unwrap();
        assert_eq!(value, padded);
        let mut lhs_hash = DefaultHasher::new();
        let mut rhs_hash = DefaultHasher::new();
        value.hash(&mut lhs_hash);
        padded.hash(&mut rhs_hash);
        assert_eq!(lhs_hash.finish(), rhs_hash.finish());
    }

    // Test-fixture word loading only: these trusted independent literals are
    // not routed through an uncompleted production parser or public admission.
    fn independent_literal_words(text: &str) -> Decimal {
        let negative = text.starts_with('-');
        let text = text.strip_prefix('-').unwrap_or(text);
        let (integer, fraction) = text.split_once('.').unwrap();
        assert!(!integer.is_empty() && !fraction.is_empty());
        assert!(
            integer
                .bytes()
                .chain(fraction.bytes())
                .all(|byte| byte.is_ascii_digit())
        );
        let mut value = Decimal::try_new(integer.len(), fraction.len(), negative).unwrap();
        let int_words = value.int_words();
        for (index, digits) in integer.as_bytes().rchunks(DIGITS_PER_WORD).enumerate() {
            value.word_buf[int_words - index - 1] =
                str::from_utf8(digits).unwrap().parse().unwrap();
        }
        for (index, digits) in fraction.as_bytes().chunks(DIGITS_PER_WORD).enumerate() {
            let word: u32 = str::from_utf8(digits).unwrap().parse().unwrap();
            value.word_buf[int_words + index] = word * TEN_POW[DIGITS_PER_WORD - digits.len()];
        }
        value
    }

    fn assert_independent_value(value: &Decimal, coefficient: &str, scale: usize) {
        // Numerical normalization of STORAGE text only. This intentionally
        // says nothing about result scale, declared shape or SQL disposition.
        let storage = value.to_string_value();
        let negative = storage.starts_with('-');
        let magnitude = storage.strip_prefix('-').unwrap_or(&storage);
        let (integer, fraction) = magnitude.split_once('.').unwrap_or((magnitude, ""));
        let digits = format!("{integer}{fraction}");
        let mut digits = digits.trim_start_matches('0').to_owned();
        let mut stored_scale = fraction.len();
        while stored_scale > 0 && digits.ends_with('0') {
            digits.pop();
            stored_scale -= 1;
        }
        if digits.is_empty() {
            digits.push('0');
            stored_scale = 0;
        } else if negative {
            digits.insert(0, '-');
        }
        assert_eq!((digits.as_str(), stored_scale), (coefficient, scale));
    }

    #[test]
    fn test_private_grow_integer_pair_signs_and_zero() {
        // Independent decimal-exact-v1 / seed20260928 four-sign cases.
        for (lhs, rhs, quotient, remainder) in [
            ("5.25", "2.0", "2", "1.25"),
            ("-5.25", "2.0", "-2", "-1.25"),
            ("5.25", "-2.0", "-2", "1.25"),
            ("-5.25", "-2.0", "2", "-1.25"),
        ] {
            let lhs = Decimal::from_str(lhs).unwrap();
            let rhs = Decimal::from_str(rhs).unwrap();
            let (q, r) = lhs.try_div_rem_exact(&rhs).unwrap().unwrap();
            assert_eq!(q.to_string_value(), quotient);
            assert_eq!(r.to_string_value(), remainder);
            // Separate source-policy assertions, not numerical-oracle claims.
            assert_eq!((q.storage_scale(), q.result_scale()), (0, 0));
            assert_eq!((r.storage_scale(), r.result_scale()), (2, 2));
        }
        let mut zero = Decimal::from_str("0.000").unwrap();
        zero.negative = true;
        let rhs = Decimal::from_str("2.0").unwrap();
        let (q, r) = zero.try_div_rem_exact(&rhs).unwrap().unwrap();
        assert_eq!(q.to_string_value(), "0");
        assert_eq!(r.to_string_value(), "0.000");
        assert!(!q.is_negative() && !r.is_negative());
        assert_eq!((r.storage_scale(), r.result_scale()), (3, 3));
        assert!(rhs.try_div_rem_exact(&zero).unwrap().is_none());
        assert!(zero.try_div_rem_exact(&zero).unwrap().is_none());
    }

    #[test]
    fn test_private_grow_integer_pair_large_quotient() {
        let lhs = Decimal::from(u64::MAX);
        let rhs = Decimal::from_str("1.5").unwrap();
        let (q, r) = lhs.try_div_rem_exact(&rhs).unwrap().unwrap();
        assert_eq!(q.to_string_value(), "12297829382473034410");
        assert_eq!(r.to_string_value(), "0.0");
        let mut lhs =
            Decimal::from_str("3428138243708624600000000000000000000000000000000000").unwrap();
        let rhs =
            Decimal::from_str("0.000000000000000000000000000000000000000000010962196522059515")
                .unwrap();
        let (q, r) = lhs.try_div_rem_exact(&rhs).unwrap().unwrap();
        // Independent Fraction identity and pinned-Go remainder observation;
        // deliberately NOT the existing capped Fixed MOD value oracle.
        let full_quotient = "312723662343590746587750435944686855597018456899102054479447138416084646758822877655408325148828";
        let full_remainder = "0.000000000000000000000000000000000000000000010939552551501580";
        assert_eq!(q.to_string_value(), full_quotient);
        assert_eq!(r.to_string_value(), full_remainder);
        lhs.negative = true;
        let (q, r) = lhs.try_div_rem_exact(&rhs).unwrap().unwrap();
        assert_eq!(q.to_string_value(), format!("-{full_quotient}"));
        assert_eq!(r.to_string_value(), format!("-{full_remainder}"));
    }

    #[test]
    fn test_private_grow_independent_fraction_values() {
        // Self-contained independent Fraction/int oracle decimal-exact-v1,
        // seed20260928; cases SHA256
        // 86a2722320feb68ba6e399507c0cccb12a6bc1ff0c24e496ba6a7bc72a01964b.
        // Rows word-18-dense-opposite-signs and fraction-256-by-300.
        let lhs = independent_literal_words("184628305780335313.494431762906928909");
        let rhs = independent_literal_words("-930718901249546327.853367952721884931");
        assert_independent_value(
            &lhs.try_add_exact(&rhs).unwrap(),
            "-746090595469211014358936189814956022",
            18,
        );
        assert_independent_value(
            &lhs.try_sub_exact(&rhs).unwrap(),
            "111534720702988164134779971562881384",
            17,
        );
        assert_independent_value(
            &lhs.try_mul_exact(&rhs).unwrap(),
            "-171837053895438946112299425621433320984073416945493974733201102895370279",
            36,
        );
        let (q, r) = lhs.try_div_rem_exact(&rhs).unwrap().unwrap();
        assert_independent_value(&q, "0", 0);
        assert_independent_value(&r, "184628305780335313494431762906928909", 18);

        let lhs = independent_literal_words(
            "-0.5439767909187346061089285520915574831506891562065998621311102453539558970127812305859011973741924537857663161480124750658918326829210957444713125200350066344909747537788039187593544618553689143985694843419709780548776579533285833378481022689204141610805813",
        );
        let rhs = independent_literal_words(
            "0.372737857310148440963747137278784278874471758359866056322842216165299227102842007169287638674684924853646138589983270541785229545432034636246831853813697228796843641812710635642988226016938952975738080343197697768588166841654971293421688110661107743280333999626957534421653121891315338593576299055183",
        );
        let difference = "-171238933608586165145181414812773204276217397846733805808268029188656669909939223416613558699507528932120177558029204524106603137489061108224480666221309405694131111966093283116366235838429961422831403998773280286289491111673612044426414158259306417800247300373042465578346878108684661406423700944817";
        assert_independent_value(&lhs.try_add_exact(&rhs).unwrap(), difference, 300);
        assert_independent_value(
            &lhs.try_sub_exact(&rhs).unwrap(),
            "-916714648228883047072675689370341762025160914566465918453952461519255124115623237755188836048877378639412454737995745607677062228353130380718144373848703863287818395591514554402342687872307867374307564685168675823465824794983554631269790379581521904360915299626957534421653121891315338593576299055183",
            300,
        );
        assert_independent_value(
            &lhs.try_mul_exact(&rhs).unwrap(),
            "-2027607434734997518566889169254489182455881016422477992140640099125994577672044398567928443083917725128752225927497194977069537167707824504993847407376884558653128387855385403512917842398877367576914740672859270773383918114555635585065883243959246822153350731822005671966294910431238920759179648542877836630823293478019830858487382636578568929642058831525910814974249647981131402806984496956443774141493215680292119626653236997954509239023157786186870041786818820974974243987602952399349572409078478842909379431111526795782640145160574332159034735184178779",
            556,
        );
        let (q, r) = lhs.try_div_rem_exact(&rhs).unwrap().unwrap();
        assert_independent_value(&q, "-1", 0);
        assert_independent_value(&r, difference, 300);
    }

    #[test]
    fn test_private_grow_independent_498_digit_quotient() {
        // decimal-exact-v1 row integer-199-by-tiny-300. No experiment path or
        // generator dependency is required to build/run this native test.
        let lhs = independent_literal_words(
            "5225910969883206332688559313566302417354660470502923306617865703804401723273576942809373538438175031163964200694666348267915625866124196226659535325278231330210693826502374526883827355835920931354941.0",
        );
        let rhs = independent_literal_words(&format!("0.{}7", "0".repeat(299)));
        let (q, r) = lhs.try_div_rem_exact(&rhs).unwrap().unwrap();
        assert_independent_value(
            &q,
            "746558709983315190384079901938043202479237210071846186659695100543485960467653848972767648348310718737709171527809478323987946552303456603808505046468318761458670546643196360983403907976560133050705857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142857142",
            0,
        );
        assert_independent_value(&r, "6", 300);
    }

    #[test]
    fn test_private_grow_avg_retention() {
        let eight = Decimal::from(8).try_div_round_exact(7, 7).unwrap().unwrap();
        let nine = Decimal::from(9).try_div_round_exact(7, 7).unwrap().unwrap();
        assert_eq!(eight.to_string_value(), "1.142857142");
        assert_eq!((eight.storage_scale(), eight.result_scale()), (9, 7));
        let sum = eight.try_add_exact(&nine).unwrap();
        let average = sum.try_div_round_exact(2, 14).unwrap().unwrap();
        assert_eq!(average.to_string_value(), "1.214285713500000000");
        assert_eq!((average.storage_scale(), average.result_scale()), (18, 14));
        for target in [9_u32, 31, 81, 300] {
            let value = Decimal::from(2)
                .try_div_round_exact(3, target)
                .unwrap()
                .unwrap();
            let retained = (target as usize).div_ceil(DIGITS_PER_WORD) * DIGITS_PER_WORD;
            assert_eq!(
                value.to_string_value(),
                format!("0.{}", "6".repeat(retained))
            );
            assert_eq!(
                (value.storage_scale(), value.result_scale()),
                (retained as u32, target)
            );
        }
        // Exactly nine retained digits have NO tenth guard digit to round.
        let no_guard = Decimal::from(2).try_div_round_exact(3, 9).unwrap().unwrap();
        assert_eq!(no_guard.to_string_value(), "0.666666666");
        let zero = Decimal::from_str("0.00").unwrap();
        let average_zero = zero.try_div_round_exact(3, 10).unwrap().unwrap();
        assert_eq!(
            (average_zero.storage_scale(), average_zero.result_scale()),
            (18, 10)
        );
        assert_eq!(average_zero.to_string_value(), "0.000000000000000000");
        // Native fixed zero retains its existing visible-scale shape instead.
        let legacy_zero = zero.div(&Decimal::from(3), 8).unwrap();
        assert_eq!(
            (legacy_zero.storage_scale(), legacy_zero.result_scale()),
            (10, 10)
        );
    }

    #[test]
    fn test_private_division_preflight_and_fixed_domain() {
        let value = Decimal::from_str("1.00").unwrap();
        assert!(value.try_div_round_exact(0, 2).unwrap().is_none());
        assert!(value.try_div_round_exact(-1, 2).is_err());
        assert!(value.try_div_round_exact(1, 1).is_err());
        assert!(Decimal::from(1).try_div_round_exact(1, u32::MAX).is_err());
        assert!(
            divide_with_limit(
                &Decimal::from(1),
                &Decimal::from(2),
                DivisionRequest::RetainedQuotient {
                    frac_words: usize::MAX
                },
                WordLimit::Grow,
                None
            )
            .is_err()
        );

        // The original private witness recorded a checked header-underflow
        // Err here; immutable before logs and a separate actual RED preceded
        // the approved FULL-fraction budget fix. This new-domain expectation
        // is updated under that approval, not an old bounded numeric oracle.
        let mut lhs = Decimal::try_new(45, 100, false).unwrap();
        lhs.word_buf[0] = 150_000_000;
        let last = lhs.int_words() + lhs.frac_words() - 1;
        lhs.word_buf[last] = 100_000_000;
        let mut rhs = Decimal::try_new(45, 0, false).unwrap();
        rhs.word_buf[0] = 100_000_000;
        let (q, r) = lhs.try_div_rem_exact(&rhs).unwrap().unwrap();
        assert_eq!(q.to_string_value(), "1");
        assert_eq!(
            r.to_string_value(),
            format!("5{}.{}1", "0".repeat(43), "0".repeat(99))
        );
        let limited = divide_with_limit(
            &lhs,
            &rhs,
            DivisionRequest::Remainder,
            WordLimit::Fixed(WORD_BUF_LEN),
            None,
        )
        .unwrap()
        .unwrap()
        .remainder
        .unwrap();
        assert!(limited.is_truncated());
        assert_eq!((limited.storage_scale(), limited.result_scale()), (36, 100));
        assert_eq!(
            limited.to_string_value(),
            format!("5{}.{}", "0".repeat(43), "0".repeat(36))
        );
    }

    #[test]
    fn test_private_words_import_exact_fields_and_ownership() {
        let mut source = vec![0; 14];
        source[0] = 1;
        source[1] = 500_000_000;
        source[13] = u32::MAX; // initialized inactive cell, not a digit
        let value = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 100,
            result_frac: 7,
            negative: true,
            words: &source,
        })
        .unwrap();
        assert_eq!(
            (
                value.words().int_digits,
                value.storage_scale(),
                value.result_scale(),
                value.is_negative()
            ),
            (1, 100, 7, true)
        );
        assert_eq!(value.words().words, source);
        assert_eq!(value.words().words.len(), 14);
        assert!(value.spill_capacity_bytes() >= 14 * WORD_SIZE);
        assert!(value.try_to_parts().is_err());
        source[0] = 9;
        source[13] = 0;
        assert_eq!(value.words().words[0], 1);
        assert_eq!(value.words().words[13], u32::MAX);
        assert_eq!(value.to_string_value(), format!("-1.5{}", "0".repeat(99)));

        let metadata_only = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 0,
            result_frac: u32::MAX,
            negative: false,
            words: &[1],
        })
        .unwrap();
        assert_eq!(
            (metadata_only.storage_scale(), metadata_only.result_scale()),
            (0, u32::MAX)
        );
        assert_eq!(metadata_only.words().words.len(), WORD_BUF_LEN);
        assert_eq!(metadata_only.words().words[0], 1);
        assert!(
            metadata_only.words().words[1..]
                .iter()
                .all(|word| *word == 0)
        );
        assert_eq!(metadata_only.spill_capacity_bytes(), 0);
        assert_eq!(metadata_only.as_i64(), Res::Ok(1));
        assert_eq!(metadata_only.as_u64(), Res::Ok(1));
        assert!(metadata_only.try_to_parts().is_err());
    }

    #[test]
    fn test_private_words_import_rejects_invalid_shape() {
        for view in [
            DecimalWordsRef {
                int_digits: 0,
                storage_frac: 0,
                result_frac: 0,
                negative: false,
                words: &[0; 9],
            },
            DecimalWordsRef {
                int_digits: 0,
                storage_frac: 0,
                result_frac: 1,
                negative: false,
                words: &[0; 9],
            },
            DecimalWordsRef {
                int_digits: 100,
                storage_frac: 0,
                result_frac: 0,
                negative: false,
                words: &[0; 9],
            },
            DecimalWordsRef {
                int_digits: 1,
                storage_frac: 0,
                result_frac: 0,
                negative: false,
                words: &[WORD_BASE],
            },
            DecimalWordsRef {
                int_digits: 1,
                storage_frac: 0,
                result_frac: 0,
                negative: false,
                words: &[10],
            },
            DecimalWordsRef {
                int_digits: 0,
                storage_frac: 1,
                result_frac: 1,
                negative: false,
                words: &[1],
            },
            DecimalWordsRef {
                int_digits: usize::MAX,
                storage_frac: 1,
                result_frac: 0,
                negative: false,
                words: &[],
            },
            DecimalWordsRef {
                int_digits: usize::MAX,
                storage_frac: 0,
                result_frac: 0,
                negative: false,
                words: &[],
            },
        ] {
            assert!(
                matches!(
                    Decimal::try_from_words(view),
                    Err(Error::InvalidDataType(_))
                ),
                "{view:?}"
            );
        }
        let fraction = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 0,
            storage_frac: 1,
            result_frac: 31,
            negative: false,
            words: &[100_000_000],
        })
        .unwrap();
        assert_eq!(fraction.to_string_value(), "0.1");
        assert_eq!(fraction.result_scale(), 31);
        // Inactive words are deliberately NOT subject to base/padding checks.
        let inactive = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 0,
            result_frac: 0,
            negative: false,
            words: &[1, u32::MAX],
        })
        .unwrap();
        assert_eq!(inactive.words().words[1], u32::MAX);
        assert_eq!(inactive.spill_capacity_bytes(), 0);
    }

    #[test]
    fn test_private_wide_integer_conversion_extents() {
        // This expectation must fail against the old capped fraction scan:
        // scale91 places its nonzero digit in word11, beyond the ten sentinel.
        for scale in [91_u32, 100, 101, 300, 82, 90] {
            let count = (scale as usize).div_ceil(DIGITS_PER_WORD);
            let mut words = vec![0; count];
            words[count - 1] = TEN_POW[count * DIGITS_PER_WORD - scale as usize];
            let view = DecimalWordsRef {
                int_digits: 0,
                storage_frac: scale,
                result_frac: scale,
                negative: false,
                words: &words,
            };
            let value = Decimal::try_from_words(view).unwrap();
            assert!(!value.is_zero());
            assert_eq!(value.as_i64(), Res::Truncated(0), "storage scale {scale}");
            assert_eq!(value.as_u64(), Res::Truncated(0), "storage scale {scale}");
            let negative = Decimal::try_from_words(DecimalWordsRef {
                negative: true,
                ..view
            })
            .unwrap();
            assert_eq!(negative.as_i64(), Res::Truncated(0));
            assert_eq!(negative.as_u64(), Res::Overflow(0));
        }
        let mut leading = [0; 11];
        leading[10] = 1;
        let padded = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 99,
            storage_frac: 0,
            result_frac: 0,
            negative: false,
            words: &leading,
        })
        .unwrap();
        assert_eq!(padded.words().int_digits, 99); // import must not normalize
        assert_eq!(padded.as_i64(), Res::Ok(1));
        assert_eq!(padded.as_u64(), Res::Ok(1));
        let raw_negative_zero = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 91,
            result_frac: 7,
            negative: true,
            words: &[0; 12],
        })
        .unwrap();
        assert_eq!(raw_negative_zero.as_i64(), Res::Ok(0));
        assert_eq!(raw_negative_zero.as_u64(), Res::Overflow(0));
        assert!(raw_negative_zero.is_negative());
    }

    #[test]
    fn test_private_words_import_equality_and_physical_boundary() {
        let mut words = [0; 14];
        words[0] = 1;
        words[1] = 500_000_000;
        words[13] = u32::MAX;
        let wide_scale = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 100,
            result_frac: 300,
            negative: false,
            words: &words,
        })
        .unwrap();
        let short = Decimal::from_str("1.50").unwrap();
        assert_eq!(wide_scale, short);
        let mut wide_hash = DefaultHasher::new();
        let mut short_hash = DefaultHasher::new();
        wide_scale.hash(&mut wide_hash);
        short.hash(&mut short_hash);
        assert_eq!(wide_hash.finish(), short_hash.finish());
        let negative_zero = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 2,
            result_frac: 7,
            negative: true,
            words: &[0; 9],
        })
        .unwrap();
        assert!(negative_zero < Decimal::zero());
        assert!(negative_zero.clone().neg().is_negative());
        assert!(!negative_zero.clone().abs().is_negative());

        let mut ten_words = [0; 10];
        ten_words[0] = 1;
        let wide_integer = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 82,
            storage_frac: 0,
            result_frac: 0,
            negative: false,
            words: &ten_words,
        })
        .unwrap();
        let wide_result = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 0,
            result_frac: 256,
            negative: false,
            words: &[1],
        })
        .unwrap();
        for value in [&wide_integer, &wide_result] {
            assert!(value.try_to_parts().is_err());
            let mut encoded = vec![42];
            assert!(encoded.write_decimal_to_chunk(value).is_err());
            assert_eq!(encoded, vec![42]);
        }
    }

    #[test]
    fn test_private_fixed_mod_full_fraction_budget() {
        // Approved-in-principle NEW canonical-wide capacity contract; obtain
        // actual RED before changing the held remainder planner. This does
        // not change the capped quotient loop or any old numeric fixture.
        for (negative_lhs, negative_rhs) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let mut lhs_words = [0; 17];
            lhs_words[0] = 150_000_000;
            lhs_words[16] = 100_000_000;
            let lhs = Decimal::try_from_words(DecimalWordsRef {
                int_digits: 45,
                storage_frac: 100,
                result_frac: 100,
                negative: negative_lhs,
                words: &lhs_words,
            })
            .unwrap();
            let rhs = Decimal::try_from_words(DecimalWordsRef {
                int_digits: 45,
                storage_frac: 0,
                result_frac: 0,
                negative: negative_rhs,
                words: &[100_000_000, 0, 0, 0, 0],
            })
            .unwrap();
            let remainder = (&lhs % &rhs).unwrap();
            assert!(remainder.is_truncated());
            assert_eq!(
                (
                    remainder.words().int_digits,
                    remainder.storage_scale(),
                    remainder.result_scale(),
                    remainder.is_negative()
                ),
                (45, 36, 100, negative_lhs)
            );
            assert_eq!(
                remainder.to_string_value(),
                format!(
                    "{}5{}.{}",
                    if negative_lhs { "-" } else { "" },
                    "0".repeat(43),
                    "0".repeat(36)
                )
            );
            assert_eq!(remainder.words().words.len(), WORD_BUF_LEN);
        }
        for exponent in [50_usize, 80, 91] {
            for negative in [false, true] {
                let mut words = [0; 12];
                let index = (exponent - 1) / DIGITS_PER_WORD;
                words[index] = TEN_POW[DIGITS_PER_WORD - 1 - (exponent - 1) % DIGITS_PER_WORD];
                let lhs = Decimal::try_from_words(DecimalWordsRef {
                    int_digits: 0,
                    storage_frac: 100,
                    result_frac: 100,
                    negative,
                    words: &words,
                })
                .unwrap();
                let rhs = Decimal::from(1);
                let remainder = (&lhs % &rhs).unwrap();
                assert!(remainder.is_truncated());
                assert_eq!(remainder.result_scale(), 100);
                assert_eq!(remainder.words().words.len(), WORD_BUF_LEN);
                if exponent <= 81 {
                    // Leading zero words count toward the output fraction
                    // budget; copying fewer nonzero words is not enough.
                    assert_eq!(
                        (
                            remainder.words().int_digits,
                            remainder.storage_scale(),
                            remainder.is_negative()
                        ),
                        (0, 81, negative)
                    );
                    assert_eq!(
                        remainder.to_string_value(),
                        format!(
                            "{}0.{}1{}",
                            if negative { "-" } else { "" },
                            "0".repeat(exponent - 1),
                            "0".repeat(81 - exponent)
                        )
                    );
                } else {
                    // Existing gap>=n early zero/status rule remains.
                    assert_eq!(
                        (
                            remainder.words().int_digits,
                            remainder.storage_scale(),
                            remainder.is_negative()
                        ),
                        (1, 0, false)
                    );
                    assert_eq!(remainder.to_string_value(), "0");
                }
            }
        }
    }

    #[test]
    fn test_private_fixed_mod_visible_only_zero_budget() {
        let negative_zero = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 0,
            result_frac: 0,
            negative: true,
            words: &[0],
        })
        .unwrap();
        // Existing raw result-byte domain must keep its old visible-as-stored
        // shape, even when that zero has more than nine active fraction words.
        for visible in [30_u8, 255] {
            let mut cell = [0; DECIMAL_STRUCT_SIZE];
            cell[..4].copy_from_slice(&[1, 0, visible, 0]);
            cell[4..8].copy_from_slice(&1_u32.to_ne_bytes());
            let rhs = cell.as_slice().read_decimal_from_chunk().unwrap();
            let remainder = (&negative_zero % &rhs).unwrap();
            assert!(remainder.is_ok());
            assert_eq!(
                (
                    remainder.words().int_digits,
                    remainder.storage_scale(),
                    remainder.result_scale(),
                    remainder.is_negative()
                ),
                (0, u32::from(visible), u32::from(visible), false)
            );
        }
        let lhs = Decimal::from_str(&format!("-1{}.1", "0".repeat(60))).unwrap();
        let rhs = Decimal::from_str(&format!("1{}.1", "0".repeat(60))).unwrap();
        let diagnostic = (&lhs * &rhs).unwrap();
        let remainder = (&diagnostic % &Decimal::from(1)).unwrap();
        assert!(remainder.is_ok());
        assert_eq!(
            (
                remainder.words().int_digits,
                remainder.storage_scale(),
                remainder.result_scale(),
                remainder.is_negative()
            ),
            (0, 2, 2, false)
        );
        assert_eq!(
            (
                diagnostic.words().int_digits,
                diagnostic.storage_scale(),
                diagnostic.result_scale(),
                diagnostic.is_negative()
            ),
            (81, 2, 2, true)
        );

        // Moderate safe RED analogue only. Do NOT run max-u32 dense MOD
        // until the checked zero allocation path has been implemented/reviewed.
        let metadata_only = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 0,
            result_frac: 300,
            negative: false,
            words: &[1],
        })
        .unwrap();
        let remainder = (&negative_zero % &metadata_only).unwrap();
        assert!(remainder.is_ok());
        assert_eq!(
            (
                remainder.words().int_digits,
                remainder.storage_scale(),
                remainder.result_scale(),
                remainder.is_negative()
            ),
            (0, 0, 300, false)
        );
        assert_eq!(remainder.words().words.len(), WORD_BUF_LEN);
        assert_eq!(remainder.spill_capacity_bytes(), 0);
        assert_eq!(remainder.to_string_value(), "0");
    }

    #[test]
    fn test_private_fixed_mod_max_visible_zero_budget() {
        // Added ONLY after reading the corrected zero path: remainder_scale
        // comes from input STORAGE, and visible>255 selects it before try_new.
        // For these inputs the constructor initializes just nine inline cells;
        // max-u32 is assigned afterward as metadata. Never Display this value.
        let rhs = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 0,
            result_frac: u32::MAX,
            negative: false,
            words: &[1],
        })
        .unwrap();
        for (stored, negative) in [(0_u32, false), (0, true), (2, false)] {
            let lhs = Decimal::try_from_words(DecimalWordsRef {
                int_digits: 1,
                storage_frac: stored,
                result_frac: stored,
                negative,
                words: &[0; 9],
            })
            .unwrap();
            let remainder = (&lhs % &rhs).unwrap();
            assert!(remainder.is_ok());
            assert!(remainder.is_zero());
            assert_eq!(
                (
                    remainder.words().int_digits,
                    remainder.storage_scale(),
                    remainder.result_scale(),
                    remainder.is_negative()
                ),
                (0, stored, u32::MAX, false)
            );
            assert_eq!(remainder.words().words.len(), WORD_BUF_LEN);
            assert_eq!(remainder.spill_capacity_bytes(), 0);
            assert_eq!(remainder.words().words, &[0; WORD_BUF_LEN]);
        }
    }

    #[test]
    fn test_private_producer_wide_current_observations() {
        use std::sync::Arc;

        use tipb::FieldType;

        use crate::{
            codec::convert::produce_dec_with_specified_tp,
            expr::{EvalConfig, Flag},
        };

        fn observe(label: &str, input: Decimal, precision: i32, scale: i32) {
            let mut target = FieldType::default();
            target.set_flen(precision);
            target.set_decimal(scale);
            let config =
                EvalConfig::from_flag(Flag::OVERFLOW_AS_WARNING | Flag::TRUNCATE_AS_WARNING);
            let mut context = EvalContext::new(Arc::new(config));
            let result = produce_dec_with_specified_tp(&mut context, input, &target);
            let observation = result
                .as_ref()
                .map(|value| {
                    let stored = value.to_string_value();
                    assert!(stored.len() <= 1024);
                    (
                        stored,
                        value.words().int_digits,
                        value.storage_scale(),
                        value.result_scale(),
                        value.is_negative(),
                        value.words().words.to_vec(),
                    )
                })
                .map_err(|error| format!("{error:?}"));
            let warnings: Vec<_> = context
                .warnings
                .warnings
                .iter()
                .map(|warning| warning.get_code())
                .collect();
            eprintln!(
                "wide producer {label}: target=({precision},{scale}) result={observation:?} warning_count={} codes={warnings:?}",
                context.warnings.warning_cnt
            );
        }
        observe(
            "integer101",
            Decimal::try_from_literal(&format!("1{}", "0".repeat(100))).unwrap(),
            65,
            30,
        );
        observe(
            "fraction100",
            Decimal::try_from_literal(&format!("-0.{}1", "0".repeat(99))).unwrap(),
            65,
            30,
        );
        observe(
            "hidden-stored100",
            Decimal::try_from_literal(&format!("1.25{}", "0".repeat(98))).unwrap(),
            65,
            1,
        );
        let mut words = [0; 14];
        words[0] = 1;
        words[13] = u32::MAX;
        let visible = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 0,
            result_frac: u32::MAX,
            negative: false,
            words: &words,
        })
        .unwrap();
        // Existing producer's no-op preserves raw result metadata/all cells.
        // Observe STORAGE only, never enormous result/Display formatting.
        observe("visibleMAX-no-op-all-cells", visible, 81, 0);
        // Exact intended81-fraction-digit/nine-word shape. The bounded literal parser
        // returned Truncated for the earlier0.+81ones fixture; do not alter
        // that parser/status or substitute its shorter partial as this input.
        let fraction81 = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 0,
            storage_frac: 81,
            result_frac: 81,
            negative: false,
            words: &[111_111_111; 9],
        })
        .unwrap();
        assert_eq!(
            fraction81.to_string_value(),
            format!("0.{}", "1".repeat(81))
        );
        observe("fraction81-no-op", fraction81, 81, 81);
        for header in [[1, 0, 0, 0], [0, 0, 0, 0]] {
            let mut cell = [0; DECIMAL_STRUCT_SIZE];
            cell[..4].copy_from_slice(&header);
            cell[4..8].copy_from_slice(&12_u32.to_ne_bytes());
            cell[36..40].copy_from_slice(&u32::MAX.to_ne_bytes());
            let raw = cell.as_slice().read_decimal_from_chunk().unwrap();
            observe("raw-physical-no-op", raw, 81, 0);
        }
    }

    #[test]
    fn test_private_boundary_rejects_invalid_encode_target() {
        // RED first: validation must precede even the two header bytes.
        let value = Decimal::from(1);
        let mut output = vec![42];
        let result = output.write_decimal(&value, 1, 2);
        assert!(result.is_err());
        assert_eq!(output, vec![42]);
    }

    #[test]
    fn test_private_boundary_rejects_invalid_convert_target() {
        // A zero/no-op may not bypass the separately aligned word budget.
        // No new SQL65/30 cap is proposed for memory-valid targets here.
        for (input, precision, scale) in
            [("0.0", 81, 1), ("0", 82, 0), ("0", 128, 128), ("1", 1, 2)]
        {
            let value = Decimal::from_str(input).unwrap();
            let mut context = EvalContext::default();
            assert!(
                value.convert_to(&mut context, precision, scale).is_err(),
                "target=({precision},{scale})"
            );
        }
        // The same invalid layout must not reach infallible max construction
        // through the saturation branch either; the input itself is bounded.
        let value = Decimal::from_str(&format!("1{}", "0".repeat(80))).unwrap();
        let mut context = EvalContext::default();
        assert!(value.convert_to(&mut context, 81, 1).is_err());
    }

    #[test]
    fn test_private_codec_max_visible_bounded_diagnostics() {
        // Added only after reading BOTH logging branches: they now contain
        // scalar shape metadata, never Decimal Display/storage materialization.
        let value = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 2,
            storage_frac: 0,
            result_frac: u32::MAX,
            negative: false,
            words: &[12],
        })
        .unwrap();
        let before = value.words().words.to_vec();
        let mut bytes = Vec::new();
        assert!(bytes.write_decimal(&value, 1, 0).unwrap().is_overflow());
        assert_eq!(bytes, vec![1, 0, 0x82]);
        assert_eq!(
            (
                value.words().int_digits,
                value.storage_scale(),
                value.result_scale()
            ),
            (2, 0, u32::MAX)
        );
        assert_eq!(value.words().words, before);

        let fraction = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 4,
            result_frac: u32::MAX,
            negative: false,
            words: &[1, 234_500_000],
        })
        .unwrap();
        let mut bytes = Vec::new();
        assert!(bytes.write_decimal(&fraction, 2, 1).unwrap().is_truncated());
        assert_eq!(bytes, vec![2, 1, 0x81, 2]);
        assert_eq!(
            (fraction.storage_scale(), fraction.result_scale()),
            (4, u32::MAX)
        );
    }

    #[test]
    fn test_private_checked_max_and_raw_full_clone() {
        for target in [(1, 2), (81, 1), (82, 0), (128, 128)] {
            assert!(try_max_decimal(target.0, target.1).is_err());
        }
        assert_eq!(
            try_max_decimal(81, 0).unwrap().to_string_value(),
            "9".repeat(81)
        );
        assert_eq!(
            try_max_decimal(81, 81).unwrap().to_string_value(),
            format!("0.{}", "9".repeat(81))
        );

        let mut words = [0; 14];
        words[0] = 1;
        words[13] = u32::MAX;
        let value = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 0,
            result_frac: 7,
            negative: true,
            words: &words,
        })
        .unwrap();
        let mut copied = value.try_clone_all().unwrap();
        assert_eq!(
            (
                copied.words().int_digits,
                copied.storage_scale(),
                copied.result_scale(),
                copied.is_negative()
            ),
            (1, 0, 7, true)
        );
        assert_eq!(copied.words().words, words);
        copied.word_buf[13] = 0;
        assert_eq!(value.words().words[13], u32::MAX);
        let mut context = EvalContext::default();
        let unchanged = value.convert_to(&mut context, 1, 0).unwrap();
        assert_eq!(unchanged.words().words, words);
        assert_eq!(unchanged.result_scale(), 7);
        assert!(unchanged.is_negative());

        // This physical head cannot pass strict logical import, so an exact
        // fallible raw copy must not implement itself through that importer.
        let mut cell = [0; DECIMAL_STRUCT_SIZE];
        cell[..4].copy_from_slice(&[1, 0, 0, 0]);
        cell[4..8].copy_from_slice(&12_u32.to_ne_bytes());
        cell[36..40].copy_from_slice(&u32::MAX.to_ne_bytes());
        let raw = cell.as_slice().read_decimal_from_chunk().unwrap();
        let copied = raw.try_clone_all().unwrap();
        assert_eq!(raw.words().words, copied.words().words);
        assert_eq!(raw.words().int_digits, copied.words().int_digits);
        assert_eq!(raw.try_storage_text().unwrap(), "2");
        let mut encoded = Vec::new();
        encoded.write_decimal_to_chunk(&copied).unwrap();
        assert_eq!(encoded, cell);
        let mut count = DecimalTextCounter { bytes: usize::MAX };
        assert!(fmt::Write::write_str(&mut count, "0").is_err());
    }

    #[test]
    fn test_private_codec_visible_300_observation() {
        // CURRENT safe-size observation only. Do not replace300 withMAX until
        // both diagnostic full-Display calls have been removed and reviewed.
        let value = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 2,
            storage_frac: 0,
            result_frac: 300,
            negative: false,
            words: &[12],
        })
        .unwrap();
        let mut bytes = Vec::new();
        let status = bytes.write_decimal(&value, 1, 0).unwrap();
        assert!(status.is_overflow());
        assert_eq!(bytes, vec![1, 0, 0x82]);
        assert_eq!(
            (
                value.words().int_digits,
                value.storage_scale(),
                value.result_scale()
            ),
            (2, 0, 300)
        );
        eprintln!(
            "moderate codec observation: Overflow bytes={bytes:?}; source int2/storage0/result300; no source mutation"
        );
    }

    #[test]
    fn test_private_storage_float_current_observations() {
        fn observe(label: &str, value: &Decimal) -> f64 {
            // All STORAGE strings in this characterization are moderate.
            // This is the existing native policy, not Go/TDB projection.
            let stored = value.to_string_value();
            assert!(stored.len() <= 1024);
            let expected = stored.parse::<f64>().unwrap();
            let mut context = EvalContext::default();
            let result = <Decimal as ConvertTo<f64>>::convert(value, &mut context).unwrap();
            assert_eq!(result.to_bits(), expected.to_bits());
            assert_eq!(context.warnings.warning_cnt, 0);
            eprintln!(
                "storage float {label}: bytes={} bits={:016x} infinite={} warnings={}",
                stored.len(),
                result.to_bits(),
                result.is_infinite(),
                context.warnings.warning_cnt
            );
            result
        }
        for negative in [false, true] {
            let sign = if negative { "-" } else { "" };
            let large = Decimal::try_from_literal(&format!("{sign}1{}", "0".repeat(400))).unwrap();
            assert!(observe("integer400", &large).is_infinite());
            let tiny = Decimal::try_from_literal(&format!("{sign}0.{}1", "0".repeat(399))).unwrap();
            let result = observe("fraction400", &tiny);
            assert_eq!(
                result.to_bits(),
                if negative {
                    (-0.0_f64).to_bits()
                } else {
                    0.0_f64.to_bits()
                }
            );
        }
        for last in ['2', '3', '5'] {
            let value = Decimal::try_from_literal(&format!("0.{}{last}", "0".repeat(323))).unwrap();
            observe("subnormal324", &value);
        }
        for head in ["17976931348623157", "17976931348623159"] {
            let value = Decimal::try_from_literal(&format!("{head}{}", "0".repeat(292))).unwrap();
            observe("finite-neighbor309", &value);
        }
        let ordinary = Decimal::try_from_literal(&format!("1.25{}", "0".repeat(398))).unwrap();
        let high_visible = Decimal::try_from_words(DecimalWordsRef {
            result_frac: u32::MAX,
            ..ordinary.words()
        })
        .unwrap();
        assert_eq!(
            observe("visible400", &ordinary).to_bits(),
            observe("visibleMAXignored", &high_visible).to_bits()
        );
        let negative_zero = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 2,
            result_frac: 2,
            negative: true,
            words: &[0; 9],
        })
        .unwrap();
        assert_eq!(
            observe("raw-negative-zero", &negative_zero).to_bits(),
            (-0.0_f64).to_bits()
        );
        // Allowed physical but noncanonical logical heads/empty prefixes:
        // future allocation sizing must follow the ACTUAL shared emitter.
        for (header, word) in [([1, 0, 0, 0], 12_u32), ([0, 0, 0, 0], u32::MAX)] {
            let mut cell = [0; DECIMAL_STRUCT_SIZE];
            cell[..4].copy_from_slice(&header);
            cell[4..8].copy_from_slice(&word.to_ne_bytes());
            let raw = cell.as_slice().read_decimal_from_chunk().unwrap();
            observe("raw-physical-storage", &raw);
        }
        let hidden = Decimal::from(1).div(&Decimal::from(3), 4).unwrap().unwrap();
        let native = observe("bounded-hidden-one-third", &hidden);
        assert_eq!(
            native.to_bits(),
            "0.333333333".parse::<f64>().unwrap().to_bits()
        );
        assert_ne!(native.to_bits(), "0.3333".parse::<f64>().unwrap().to_bits());
    }

    #[test]
    fn test_private_storage_bridge_current_observations() {
        fn observe(label: &str, value: &Decimal, expected: &str) {
            // These fixtures have bounded ACTUAL storage even if visible scale
            // is MAX. Never format/Debug the Decimal or its result projection.
            assert!(value.int_cnt <= 128 && value.frac_cnt <= 128);
            let before = (
                value.int_cnt,
                value.frac_cnt,
                value.result_frac_cnt,
                value.negative,
                value.word_buf.to_vec(),
            );
            let legacy = value.to_string_value();
            assert!(legacy.len() <= 260);
            assert_eq!(legacy, expected);
            assert_eq!(value.try_storage_text().unwrap(), expected);
            for flag in [
                Flag::empty(),
                Flag::TRUNCATE_AS_WARNING | Flag::OVERFLOW_AS_WARNING,
                Flag::IGNORE_TRUNCATE,
            ] {
                for cap in [0, 1, 4] {
                    let mut config = EvalConfig::from_flag(flag);
                    config.set_max_warning_cnt(cap);
                    let mut context = EvalContext::new(Arc::new(config));
                    context
                        .warnings
                        .append_warning(Error::truncated_wrong_val("prefix", "retained"));
                    let count = context.warnings.warning_cnt;
                    let prefix = context.warnings.warnings.clone();
                    let text =
                        <Decimal as ConvertTo<String>>::convert(value, &mut context).unwrap();
                    let bytes =
                        <Decimal as ConvertTo<Bytes>>::convert(value, &mut context).unwrap();
                    assert_eq!(text, expected);
                    assert_eq!(bytes, expected.as_bytes());
                    assert_eq!(context.warnings.warning_cnt, count);
                    assert_eq!(context.warnings.warnings, prefix);
                }
            }
            assert_eq!(
                (
                    value.int_cnt,
                    value.frac_cnt,
                    value.result_frac_cnt,
                    value.negative,
                    value.word_buf.to_vec()
                ),
                before
            );
            eprintln!(
                "storage bridge decimal {label}: storage={legacy:?} shape_cells={before:?} bytes_match=true contexts=9 unchanged"
            );
        }
        for literal in ["12.3400", "-12.3400", "0"] {
            observe(
                "bounded-literal",
                &literal.parse::<Decimal>().unwrap(),
                literal,
            );
        }
        let negative_zero = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 2,
            result_frac: 2,
            negative: true,
            words: &[0; 9],
        })
        .unwrap();
        observe("raw-negative-zero", &negative_zero, "-0.00");
        let third = Decimal::from(1).div(&Decimal::from(3), 4).unwrap().unwrap();
        assert_eq!(third.result_scale(), 4);
        observe("hidden-one-third", &third, "0.333333333");
        let integer = format!("1{}", "0".repeat(100));
        observe(
            "integer101",
            &Decimal::try_from_literal(&integer).unwrap(),
            &integer,
        );
        let fraction = format!("-0.{}1", "0".repeat(99));
        observe(
            "fraction100",
            &Decimal::try_from_literal(&fraction).unwrap(),
            &fraction,
        );
        let hidden_text = format!("1.25{}", "0".repeat(98));
        let stored = Decimal::try_from_literal(&hidden_text).unwrap();
        let hidden = Decimal::try_from_words(DecimalWordsRef {
            result_frac: 1,
            ..stored.words()
        })
        .unwrap();
        observe("hidden-storage100", &hidden, &hidden_text);
        for visible in [0, 30, 81, 127, 128, 255] {
            let mut cell = [0; DECIMAL_STRUCT_SIZE];
            cell[..4].copy_from_slice(&[2, 1, visible, 1]);
            cell[4..8].copy_from_slice(&12_u32.to_ne_bytes());
            cell[8..12].copy_from_slice(&300_000_000_u32.to_ne_bytes());
            cell[36..40].copy_from_slice(&u32::MAX.to_ne_bytes());
            let value = cell.as_slice().read_decimal_from_chunk().unwrap();
            observe("raw-visible-byte", &value, "-12.3");
        }
        for (header, word, expected) in [([1, 0, 0, 0], 12_u32, "2"), ([0, 0, 0, 0], u32::MAX, "5")]
        {
            let mut cell = [0; DECIMAL_STRUCT_SIZE];
            cell[..4].copy_from_slice(&header);
            cell[4..8].copy_from_slice(&word.to_ne_bytes());
            cell[36..40].copy_from_slice(&u32::MAX.to_ne_bytes());
            let value = cell.as_slice().read_decimal_from_chunk().unwrap();
            observe("raw-physical-noncanonical", &value, expected);
        }
        let mut words = [0; 14];
        words[0] = 1;
        words[1] = 234_500_000;
        words[13] = u32::MAX;
        for visible in [300, u32::MAX] {
            let value = Decimal::try_from_words(DecimalWordsRef {
                int_digits: 1,
                storage_frac: 4,
                result_frac: visible,
                negative: false,
                words: &words,
            })
            .unwrap();
            observe("tiny-storage-wide-visible-all-cells", &value, "1.2345");
        }
    }

    #[test]
    fn test_private_fixed_policy_current_observations() {
        fn observe(label: &str, result: &Res<Decimal>) {
            eprintln!(
                "{label}: ok={} truncated={} overflow={} fields={:?}; storage={}",
                result.is_ok(),
                result.is_truncated(),
                result.is_overflow(),
                result.words(),
                result.to_string_value(),
            );
        }
        // CURRENT behavior characterization only, not an approved extension
        // policy or a mathematical Grow oracle. No arithmetic is corrected.
        let mut fraction_words = [0; 10];
        fraction_words[0] = 100_000_000;
        let wide_fraction = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 0,
            storage_frac: 90,
            result_frac: 90,
            negative: false,
            words: &fraction_words,
        })
        .unwrap();
        observe(
            "unadmitted-wide MUL positive1 x0.1/storage90",
            &(&Decimal::from(1) * &wide_fraction),
        );
        observe(
            "unadmitted-wide MUL negative1 x0.1/storage90",
            &(&Decimal::from(-1) * &wide_fraction),
        );

        // This overlap IS representable through the old public bounded
        // parser: the leading dot avoids charging an extra integer word.
        let bounded_fraction = Decimal::from_str(&format!(".1{}", "0".repeat(80))).unwrap();
        let bounded_integer = Decimal::from(1_000_000_001_u64);
        observe(
            "canonical-bounded MUL integer2words x0.1/storage81",
            &(&bounded_integer * &bounded_fraction),
        );

        let mut power_words = [0; 11];
        power_words[0] = 1;
        let wide_integer = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 91,
            storage_frac: 0,
            result_frac: 0,
            negative: false,
            words: &power_words,
        })
        .unwrap();
        observe(
            "unadmitted-wide Fixed round10^90 scale0",
            &wide_integer.round(0, RoundMode::HalfEven),
        );

        // A bounded-size analogue of the metadata-only zero allocation risk.
        // NEVER replace300 by u32::MAX here or render an enormous header.
        let metadata_only = Decimal::try_from_words(DecimalWordsRef {
            int_digits: 1,
            storage_frac: 0,
            result_frac: 300,
            negative: false,
            words: &[1],
        })
        .unwrap();
        let zero = Decimal::zero();
        let remainder = (&zero % &metadata_only).unwrap();
        assert!(remainder.words().words.len() <= 64);
        observe(
            "unadmitted-wide Fixed zero MOD metadata-visible300",
            &remainder,
        );
    }

    fn assert_source_parse(
        input: &[u8],
        status: DecimalParseStatus,
        storage: &str,
        shape: (usize, u32, u32, bool),
    ) -> Decimal {
        let outcome = Decimal::parse_mysql(input).unwrap();
        assert_eq!(outcome.status, status, "input={input:?}");
        assert_eq!(outcome.value.to_string_value(), storage, "input={input:?}");
        let view = outcome.value.words();
        assert_eq!(
            (
                view.int_digits,
                view.storage_frac,
                view.result_frac,
                view.negative
            ),
            shape,
            "input={input:?}"
        );
        outcome.value
    }

    #[test]
    fn test_private_mysql_parse_executed_guards() {
        use DecimalParseStatus::{Ok as Success, Overflow, Truncated, TruncatedWrongValue};
        // Pinned Go reference364aef2b, independently executed policy rows3..18.
        // Every huge exponent here is handled by word/count preflight, never
        // the old host decimal string-expansion path.
        let maximum = "9".repeat(81);
        for (input, status, is_max) in [
            ("-1e+9223372036854775808", Overflow, true),
            ("-1e-9223372036854775809", Truncated, false),
            ("\x0b1", TruncatedWrongValue, false),
            ("\x0c1", TruncatedWrongValue, false),
            ("", TruncatedWrongValue, false),
            ("junk", TruncatedWrongValue, false),
            (" 0e1073741823", Success, false),
            ("0e1073741824", Overflow, true),
            ("\t0e-1073741823", Success, false),
            ("0e-1073741824", Success, false),
            ("1e1073741823", Overflow, true),
            ("1e1073741824", Overflow, true),
            ("1e-1073741823", Truncated, false),
            ("1e-1073741824", Truncated, false),
            ("0e-1073741825", Truncated, false),
            ("1e-1073741825", Truncated, false),
        ] {
            let expected = if is_max { maximum.as_str() } else { "0" };
            let value = assert_source_parse(
                input.as_bytes(),
                status,
                expected,
                (if is_max { 81 } else { 0 }, 0, 0, false),
            );
            assert_eq!(value.spill_capacity_bytes(), 0);
        }
    }

    #[test]
    fn test_private_mysql_parse_order_and_round_origin() {
        let mantissa = format!("1{}98765", "0".repeat(84));
        assert_eq!(mantissa.len(), 90);
        assert_source_parse(
            format!("{mantissa}e-9").as_bytes(),
            DecimalParseStatus::Overflow,
            "98765",
            (81, 0, 0, false),
        );
        assert_source_parse(
            format!("{mantissa}e-9x").as_bytes(),
            DecimalParseStatus::Truncated,
            "0.000098765",
            (0, 9, 9, false),
        );
        // Independently executed Go v2 rows19/20: a traced nested Round
        // Overflow is not pointer-equal to direct Shift Overflow. It keeps
        // the partial 1e72 payload, not81 nines, despite the same SQL code.
        let long = format!("9{}e-162", "0".repeat(80));
        let partial = assert_source_parse(
            long.as_bytes(),
            DecimalParseStatus::Overflow,
            &format!("1{}", "0".repeat(72)),
            (81, 0, 0, false),
        );
        assert_eq!(partial.words().words[0], 1);
        assert!(partial.words().words[1..].iter().all(|word| *word == 0));
        assert_source_parse(
            b"9e-82",
            DecimalParseStatus::Truncated,
            "0",
            (0, 0, 0, false),
        );
        // The legacy worker retains its all-lost-before-Round ordering.
        let legacy = Decimal::from_bytes(long.as_bytes()).unwrap();
        assert!(legacy.is_truncated());
        assert_eq!(legacy.to_string_value(), "0");
        assert_eq!(legacy.words().int_digits, 1);
    }

    #[test]
    fn test_private_parser_legacy_policy_differences() {
        // Existing native fixtures deliberately suppress exponent-junk
        // warnings; source rows are also pinned in Go TestFromStringMyDecimal.
        for (input, storage) in [
            ("1e", "1"),
            ("1eabc", "1"),
            ("1e 1dddd ", "10"),
            ("1e - 1", "1"),
        ] {
            let legacy = Decimal::from_bytes(input.as_bytes()).unwrap();
            assert!(legacy.is_ok());
            assert_eq!(legacy.to_string_value(), storage);
            let source = Decimal::parse_mysql(input.as_bytes()).unwrap();
            assert_eq!(source.status, DecimalParseStatus::Truncated);
            assert_eq!(source.value.to_string_value(), storage);
        }
        // Independent pre-parser rlib observation: Rust byte ASCII whitespace
        // excludes VT but includes FF. The initial new-test VT hypothesis was
        // wrong; the unchanged production scanner correctly rejected it.
        assert!(Decimal::from_bytes(b"\x0b1").is_err());
        assert_eq!(
            Decimal::parse_mysql(b"\x0b1").unwrap().status,
            DecimalParseStatus::TruncatedWrongValue
        );
        let legacy_ff = Decimal::from_bytes(b"\x0c1").unwrap();
        assert!(legacy_ff.is_ok());
        assert_eq!(legacy_ff.to_string_value(), "1");
        assert_eq!(
            Decimal::parse_mysql(b"\x0c1").unwrap().status,
            DecimalParseStatus::TruncatedWrongValue
        );
        let bad_exponent = b"1e18446744073709551620";
        assert!(Decimal::from_bytes(bad_exponent).is_err());
        assert_source_parse(
            bad_exponent,
            DecimalParseStatus::BadNumber,
            "0",
            (0, 0, 0, false),
        );
        // Invalid UTF-8 is suffix junk, not a new outer codec failure.
        let source = Decimal::parse_mysql(b"223\xe0\x80\x80").unwrap();
        assert_eq!(source.status, DecimalParseStatus::Truncated);
        assert_eq!(source.value.to_string_value(), "223");
        assert_eq!(trim_unicode_space(b"\xc2\xa0\xff\xe3\x80\x80"), b"\xff");
        assert_eq!(trim_unicode_space(b"\xc2\xa0 \t\xe3\x80\x80"), b"");
    }

    #[test]
    fn test_private_canonical_parser_grow() {
        for (input, expected) in [
            ("+.1", "0.1"),
            ("1.", "1"),
            ("0000123.4500", "123.4500"),
            ("-000.000", "0.000"),
        ] {
            let value = Decimal::try_from_literal(input).unwrap();
            assert_eq!(value.to_string_value(), expected);
            assert_eq!(value.storage_scale(), value.result_scale());
        }
        let input = format!("1{}.{}1", "0".repeat(99), "0".repeat(100));
        let parsed = Decimal::try_from_literal(&input).unwrap();
        let independently_loaded = independent_literal_words(&input);
        assert_eq!(parsed, independently_loaded);
        assert_eq!(parsed.to_string_value(), input);
        assert_eq!(
            (
                parsed.words().int_digits,
                parsed.storage_scale(),
                parsed.result_scale()
            ),
            (100, 101, 101)
        );
        let wide_fraction = format!("-0.{}7", "0".repeat(299));
        let parsed = Decimal::try_from_literal(&wide_fraction).unwrap();
        assert_eq!(parsed.to_string_value(), wide_fraction);
        assert_eq!((parsed.storage_scale(), parsed.result_scale()), (300, 300));
        for invalid in ["", ".", "+", "-", " 1", "1 ", "1e2", "1a", "--1", "1.2.3"] {
            assert!(Decimal::try_from_literal(invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(Decimal::from_str("1e").unwrap().to_string_value(), "1");
        assert!(Decimal::try_from_literal("1e").is_err());
    }

    #[test]
    fn test_private_shared_shift_grow_alignment() {
        for (input, shift, expected) in [
            ("1.2300", 1, "12.3"),
            ("1.2300", -1, "0.123"),
            ("123456789.000000001", 9, "123456789000000001"),
            ("123456789.000000001", -9, "0.123456789000000001"),
            ("0.000000001", 9, "1"),
            ("-15.00", -2, "-0.15"),
        ] {
            let value = Decimal::try_from_literal(input).unwrap();
            let shifted = value
                .shift_with_limit(shift, WordLimit::Grow, ShiftDisposition::Legacy)
                .unwrap();
            assert!(shifted.result.is_ok());
            assert_eq!(
                shifted.result.to_string_value(),
                expected,
                "{input} by {shift}"
            );
        }
        let value = Decimal::try_from_literal("1").unwrap();
        let shifted = value
            .shift_with_limit(1000, WordLimit::Grow, ShiftDisposition::Legacy)
            .unwrap()
            .result
            .unwrap();
        assert_eq!(shifted.to_string_value(), format!("1{}", "0".repeat(1000)));
        let restored = shifted
            .shift_with_limit(-1000, WordLimit::Grow, ShiftDisposition::Legacy)
            .unwrap()
            .result
            .unwrap();
        assert_eq!(restored.to_string_value(), "1");
        let tiny = restored
            .shift_with_limit(-1000, WordLimit::Grow, ShiftDisposition::Legacy)
            .unwrap()
            .result
            .unwrap();
        assert_eq!(tiny.to_string_value(), format!("0.{}1", "0".repeat(999)));
        assert!(
            Decimal::from(1)
                .shift_with_limit(
                    -(i128::from(u32::MAX) + 1),
                    WordLimit::Grow,
                    ShiftDisposition::Legacy
                )
                .is_err()
        );
    }

    #[test]
    fn test_private_parser_mantissa_capacity_preselection() {
        // Original pinned Go one-word fixture rows, plus the required leading
        // zero capacity distinction: selection is before normalization.
        for (input, status, expected) in [
            ("123450000098765", DecimalParseStatus::Overflow, "98765"),
            ("123450.000098765", DecimalParseStatus::Truncated, "123450"),
            ("0.1", DecimalParseStatus::Truncated, "0"),
            (".1", DecimalParseStatus::Ok, "0.1"),
            ("000000000000000001", DecimalParseStatus::Overflow, "1"),
        ] {
            let parsed =
                Decimal::parse_with_policy(input.as_bytes(), DecimalParsePolicy::Mysql(1)).unwrap();
            assert_eq!(parsed.status, status);
            assert_eq!(parsed.value.to_string_value(), expected);
        }
        let canonical = Decimal::try_from_literal("000000000000000001").unwrap();
        assert_eq!(canonical.to_string_value(), "1");
        assert_eq!(canonical.words().int_digits, 1);
    }

    #[test]
    fn test_private_grow_round_modes() {
        // Source low-level modes: HalfEven is half-away, and Ceiling is
        // magnitude-away with only the first discarded digit off a word edge.
        for (input, scale, mode, expected) in [
            ("2.5", 0, RoundMode::HalfEven, "3"),
            ("-2.5", 0, RoundMode::HalfEven, "-3"),
            ("-15.5", 0, RoundMode::HalfEven, "-16"),
            ("10.99", 1, RoundMode::Truncate, "10.9"),
            ("-10.99", 1, RoundMode::Truncate, "-10.9"),
            ("-15.1", 0, RoundMode::Ceiling, "-16"),
            ("1.0001", 1, RoundMode::Ceiling, "1.0"),
            ("1.0001", 3, RoundMode::Ceiling, "1.001"),
            ("1.000000001", 0, RoundMode::Ceiling, "2"),
            ("999999999", -9, RoundMode::HalfEven, "1000000000"),
            ("999999999", -9, RoundMode::Truncate, "0"),
            (
                "999999999999999999",
                -18,
                RoundMode::HalfEven,
                "1000000000000000000",
            ),
        ] {
            let value = Decimal::from_str(input).unwrap();
            let rounded = value.try_round_exact(scale, mode).unwrap();
            assert_eq!(
                rounded.to_string_value(),
                expected,
                "input={input} scale={scale}"
            );
            assert_eq!(
                value.to_string_value(),
                Decimal::from_str(input).unwrap().to_string_value()
            );
        }
        for scale in [31, 81, 300] {
            let value = Decimal::from_str("1.25")
                .unwrap()
                .try_round_exact(scale, RoundMode::HalfEven)
                .unwrap();
            assert_eq!(
                value.to_string_value(),
                format!("1.25{}", "0".repeat(scale as usize - 2))
            );
            assert_eq!(
                (value.storage_scale(), value.result_scale()),
                (scale as u32, scale as u32)
            );
        }
    }

    #[test]
    fn test_private_grow_round_wide_carry_and_zero() {
        let value = independent_literal_words(&format!("{}.5", "9".repeat(108)));
        let rounded = value.try_round_exact(0, RoundMode::HalfEven).unwrap();
        assert_eq!(rounded.to_string_value(), format!("1{}", "0".repeat(108)));
        assert_eq!(rounded.words().int_digits, 109);
        let value = independent_literal_words(&format!("9.{}", "9".repeat(301)));
        let rounded = value.try_round_exact(300, RoundMode::HalfEven).unwrap();
        assert_eq!(rounded.to_string_value(), format!("10.{}", "0".repeat(300)));
        assert_eq!(
            (rounded.storage_scale(), rounded.result_scale()),
            (300, 300)
        );

        let mut tiny = Decimal::try_new(0, 81, false).unwrap();
        tiny.word_buf[8] = 5;
        let rounded = tiny.try_round_exact(80, RoundMode::HalfEven).unwrap();
        assert_eq!(rounded.to_string_value(), format!("0.{}1", "0".repeat(79)));
        assert_eq!(rounded.spill_capacity_bytes(), 0);
        tiny.word_buf[8] = 1;
        let rounded = tiny.try_round_exact(80, RoundMode::HalfEven).unwrap();
        assert!(rounded.is_zero());
        assert_eq!((rounded.storage_scale(), rounded.result_scale()), (80, 80));
        assert_eq!(rounded.spill_capacity_bytes(), 0);
    }

    #[test]
    fn test_private_streaming_result_scales() {
        for scale in [31, 81, 300] {
            let mut value = Decimal::from_str("1.25").unwrap();
            value.result_frac_cnt = scale;
            let before = value.words().words.to_vec();
            let mut storage = String::new();
            value.write_storage(&mut storage).unwrap();
            assert_eq!(storage, "1.25");
            let mut result = String::new();
            value.write_result(&mut result).unwrap();
            assert_eq!(result, format!("1.25{}", "0".repeat(scale - 2)));
            assert_eq!(value.storage_scale(), 2);
            assert_eq!(value.words().words, before);
            if scale == 300 {
                assert_eq!(value.to_string(), result);
            } else {
                assert_eq!(value.to_string(), format!("1.25{}", "0".repeat(28)));
            }
        }
        let mut value = independent_literal_words(&format!("1.{}56", "0".repeat(80)));
        value.result_frac_cnt = 81;
        let before = value.to_string_value();
        let mut result = String::new();
        value.write_result(&mut result).unwrap();
        assert_eq!(result, format!("1.{}6", "0".repeat(80)));
        assert_eq!(value.to_string(), result);
        assert_eq!(value.to_string_value(), before);
        assert_eq!((value.storage_scale(), value.result_scale()), (82, 81));
    }

    #[test]
    fn test_private_streaming_raw_result_255() {
        let mut cell = [0; DECIMAL_STRUCT_SIZE];
        cell[..4].copy_from_slice(&[1, 0, 255, 0]);
        cell[4..8].copy_from_slice(&1_u32.to_ne_bytes());
        let raw = cell.as_slice().read_decimal_from_chunk().unwrap();
        let mut logical = Decimal::from(1);
        logical.result_frac_cnt = 255;
        for value in [&raw, &logical] {
            assert_eq!(value.to_string(), "0");
            let mut full = String::new();
            value.write_result(&mut full).unwrap();
            assert_eq!(full, format!("1.{}", "0".repeat(255)));
            assert_eq!(value.to_string_value(), "1");
            assert_eq!((value.storage_scale(), value.result_scale()), (0, 255));
        }
        assert_eq!(raw.words().words, logical.words().words);
        let mut roundtrip = Vec::new();
        roundtrip.write_decimal_to_chunk(&raw).unwrap();
        assert_eq!(roundtrip, cell);
    }

    #[test]
    fn test_private_streaming_full_u32_failure() {
        #[derive(Default)]
        struct RejectingWriter {
            calls: usize,
            largest_chunk: usize,
        }
        impl fmt::Write for RejectingWriter {
            fn write_str(&mut self, text: &str) -> fmt::Result {
                self.calls += 1;
                self.largest_chunk = self.largest_chunk.max(text.len());
                Err(fmt::Error)
            }
        }
        let mut value = Decimal::from(1);
        value.result_frac_cnt = u32::MAX as usize;
        let mut writer = RejectingWriter::default();
        assert!(value.write_result(&mut writer).is_err());
        assert_eq!((writer.calls, writer.largest_chunk), (1, 128));
        let mut writer = RejectingWriter::default();
        assert!(fmt::write(&mut writer, format_args!("{value}")).is_err());
        assert_eq!((writer.calls, writer.largest_chunk), (1, 128));
        assert_eq!((value.storage_scale(), value.result_scale()), (0, u32::MAX));
        assert_eq!(value.words().words[0], 1);
        assert_eq!(value.spill_capacity_bytes(), 0);
    }

    #[test]
    fn test_private_round_zero_policy_and_count_preflight() {
        let mut value = Decimal::from_str("0.00").unwrap();
        value.negative = true;
        let rounded = value.try_round_exact(2, RoundMode::HalfEven).unwrap();
        assert_eq!(rounded.to_string_value(), "0.00");
        assert!(!rounded.is_negative());
        assert!(value.is_negative());
        let mut full = String::new();
        value.write_result(&mut full).unwrap();
        assert_eq!(full, "-0.00");
        assert_eq!(value.to_string(), "-0.00");
        assert_eq!(
            Decimal::from(123)
                .try_round_exact(i64::MIN, RoundMode::HalfEven)
                .unwrap()
                .to_string_value(),
            "0"
        );
        assert!(
            value
                .try_round_exact(i64::MAX, RoundMode::HalfEven)
                .is_err()
        );
        assert!(
            value
                .try_round_exact(i64::from(u32::MAX) + 1, RoundMode::Truncate)
                .is_err()
        );
    }

    #[test]
    fn test_legacy_boundary_characterization() {
        let lhs = Decimal::from_str(&format!("-1{}.1", "0".repeat(60))).unwrap();
        let rhs = Decimal::from_str(&format!("1{}.1", "0".repeat(60))).unwrap();
        let status = &lhs * &rhs;
        assert!(status.is_overflow());
        let value = status.unwrap();
        // Immutable B2.1/Go observations justify this explicit approved
        // domain extension, not a changed numerical/status oracle. Go's
        // nine-cell String/ToString both panic on this ten-active-word shape.
        let before_words = value.words().words.to_vec();
        let mut legacy = String::new();
        value.write_legacy_result(&mut legacy).unwrap();
        assert_eq!(legacy, "0");
        let mut full = String::new();
        value.write_result(&mut full).unwrap();
        assert_eq!(full, "-0.00");
        assert_eq!(value.to_string(), "-0.00");
        assert_eq!(
            (
                value.words().int_digits,
                value.storage_scale(),
                value.result_scale(),
                value.is_negative()
            ),
            (81, 2, 2, true)
        );
        assert_eq!(value.words().words, before_words);
        eprintln!(
            "fractional MUL: {:?}; private legacy={}; Display={}; storage={}",
            value.words(),
            legacy,
            value,
            value.to_string_value()
        );
        let lhs =
            Decimal::from_str("3428138243708624600000000000000000000000000000000000").unwrap();
        let rhs =
            Decimal::from_str("0.000000000000000000000000000000000000000000010962196522059515")
                .unwrap();
        let remainder = do_div_mod(&lhs, &rhs, 5, true).unwrap();
        assert_eq!(
            remainder.to_string_value(),
            "0.000000000000000000000000000000000003564345362392880000000000"
        );
        eprintln!(
            "legacy MOD: ok={} truncated={} overflow={} fields={:?}; storage={}",
            remainder.is_ok(),
            remainder.is_truncated(),
            remainder.is_overflow(),
            remainder.words(),
            remainder.to_string_value()
        );
    }

    #[test]
    fn test_inline_words_and_clone_ownership() {
        let value = Decimal::from_str("-123.4500").unwrap();
        let view = value.words();
        assert_eq!(view.int_digits, 3);
        assert_eq!((view.storage_frac, view.result_frac), (4, 4));
        assert!(view.negative);
        assert_eq!(view.words.len(), WORD_BUF_LEN);
        assert_eq!(value.spill_capacity_bytes(), 0);
        assert_eq!(value.natural_storage_shape(), (7, 4));
        let mut cloned = value.clone();
        cloned.word_buf[0] = 321;
        assert_eq!(value.words().words[0], 123);

        let lhs = Decimal::from_str(&format!("-1{}.1", "0".repeat(60))).unwrap();
        let rhs = Decimal::from_str(&format!("1{}.1", "0".repeat(60))).unwrap();
        let status = &lhs * &rhs;
        assert!(status.is_overflow());
        let value = status.unwrap();
        assert_eq!(value.words().words.len(), 10);
        assert!(value.spill_capacity_bytes() >= 10 * mem::size_of::<u32>());
        let mut cloned = value.clone();
        cloned.word_buf[9] = 10_000_000;
        assert_eq!(value.words().words[9], 0);
        assert!(value.is_zero());
    }

    #[test]
    fn test_fixed_word_limit_uses_wide_count_arithmetic() {
        assert_eq!(
            fix_word_cnt_err(usize::MAX, usize::MAX, 9),
            Res::Overflow((9, 0))
        );
        assert_eq!(fix_word_cnt_err(8, usize::MAX, 9), Res::Truncated((8, 1)));
        assert_eq!(word_cnt!(usize::MAX), WORD_BUF_LEN + 1);
    }

    #[test]
    fn test_chunk_fields_are_independent_of_owning_layout() {
        let mut words = [0; WORD_BUF_LEN];
        words[0] = 123;
        words[1] = 450_000_000;
        words[8] = u32::MAX; // Inactive physical bytes are not normalized.
        let parts = DecimalParts {
            int_digits: 3,
            frac_digits: 2,
            result_frac_digits: 7,
            negative: true,
            words,
        };
        let value = Decimal::try_from_parts(parts).unwrap();
        let mut encoded = Vec::new();
        encoded.write_decimal_to_chunk(&value).unwrap();
        assert_eq!(encoded.len(), 40);
        assert_ne!(mem::size_of::<Decimal>(), DECIMAL_STRUCT_SIZE);
        assert_eq!(&encoded[..4], &[3, 2, 7, 1]);
        for (i, word) in words.iter().enumerate() {
            assert_eq!(&encoded[4 + i * 4..8 + i * 4], &word.to_ne_bytes());
        }
        encoded.extend_from_slice(&[17, 22]);
        let mut reader = encoded.as_slice();
        let decoded = reader.read_decimal_from_chunk().unwrap();
        assert_eq!(decoded.try_to_parts().unwrap(), parts);
        assert_eq!(reader, &[17, 22]);
    }

    #[test]
    fn test_chunk_empty_prefix_is_safe_physical_zero() {
        for negative in [0, 1] {
            let mut cell = [0; DECIMAL_STRUCT_SIZE];
            cell[3] = negative;
            cell[36..40].copy_from_slice(&u32::MAX.to_ne_bytes());
            let value = cell.as_slice().read_decimal_from_chunk().unwrap();
            assert_eq!(value.words().int_digits, 0);
            assert!(value.is_zero());
            assert_eq!(value.is_negative(), negative != 0);
            assert!(value.try_to_parts().is_err());
            assert_eq!(value.cmp(&value), Ordering::Equal);
            assert!(value.clone().shift(1).unwrap().is_zero());
            assert!((&value * &value).unwrap().is_zero());
            let mut original_hash = DefaultHasher::new();
            let mut zero_hash = DefaultHasher::new();
            value.hash(&mut original_hash);
            Decimal::zero().hash(&mut zero_hash);
            assert_eq!(original_hash.finish(), zero_hash.finish());
            let mut encoded = Vec::new();
            encoded.write_decimal_to_chunk(&value).unwrap();
            assert_eq!(encoded, cell);
        }
    }

    #[test]
    fn test_chunk_preserves_raw_shape_and_result_header_domain() {
        // TiDB checked MyDecimal raw import admits result scale 127 and does
        // not require partial-word/padding normalization. TiKV raw transport
        // additionally has an unsigned result header, including 128..=255.
        for (header, word) in [
            ([1, 0, 81, 0], 1_u32),
            ([1, 0, 82, 0], 1),
            ([1, 0, 127, 0], 1),
            ([1, 0, 128, 0], 1),
            ([1, 0, 255, 0], 1),
            ([1, 0, 0, 0], 12),
            ([0, 1, 127, 1], 1),
        ] {
            let mut cell = [0; DECIMAL_STRUCT_SIZE];
            cell[..4].copy_from_slice(&header);
            cell[4..8].copy_from_slice(&word.to_ne_bytes());
            let value = cell.as_slice().read_decimal_from_chunk().unwrap();
            assert_eq!(value.result_scale(), u32::from(header[2]));
            assert_eq!(value.words().words[0], word);
            let mut encoded = Vec::new();
            encoded.write_decimal_to_chunk(&value).unwrap();
            assert_eq!(encoded, cell);
            if header[2] == 255 {
                // The existing Fixed9 Display interprets its result byte as i8.
                assert_eq!(value.to_string(), "0");
            }
        }
    }

    #[test]
    fn test_chunk_rejects_invalid_owned_value_fields() {
        let mut valid = [0; DECIMAL_STRUCT_SIZE];
        valid[0] = 1;
        let mut bad_sign = valid;
        bad_sign[3] = 2;
        let mut bad_count = valid;
        bad_count[0] = u8::MAX;
        let mut bad_word = valid;
        bad_word[4..8].copy_from_slice(&WORD_BASE.to_ne_bytes());
        for cell in [bad_sign, bad_count, bad_word] {
            let mut bytes = cell.to_vec();
            bytes.push(17);
            let mut reader = bytes.as_slice();
            assert!(matches!(
                reader.read_decimal_from_chunk(),
                Err(Error::InvalidDataType(_))
            ));
            assert_eq!(reader, &[17]);
        }
    }

    // TiDB's source-backed decimal multiplication preserves the result scale
    // when a successful negative zero product is normalized to positive zero.
    // Keep this regression distinct from Overflow's intentional signed zero.
    #[test]
    fn test_mul_zero_preserves_result_scale() {
        let lhs = Decimal::from_str("0.000").unwrap();
        let rhs = Decimal::from_str("-1").unwrap();
        let result = &lhs * &rhs;
        assert!(result.is_ok());
        let value = result.unwrap();
        assert_eq!(value.to_string(), "0.000");
        assert_eq!(value.frac_cnt(), 3);
        assert_eq!(value.result_frac_cnt(), 3);
        assert!(!value.is_negative());
    }

    #[test]
    fn test_mul_zero_sign_and_operand_symmetry() {
        let zero_parts = Decimal::from_str("0.000").unwrap().try_to_parts().unwrap();
        for (rhs_source, expected_scale) in [("1", 3), ("0.00", 5)] {
            let rhs_parts = Decimal::from_str(rhs_source)
                .unwrap()
                .try_to_parts()
                .unwrap();
            for lhs_negative in [false, true] {
                for rhs_negative in [false, true] {
                    let lhs = Decimal::try_from_parts(DecimalParts {
                        negative: lhs_negative,
                        ..zero_parts
                    })
                    .unwrap();
                    let rhs = Decimal::try_from_parts(DecimalParts {
                        negative: rhs_negative,
                        ..rhs_parts
                    })
                    .unwrap();
                    for (lhs, rhs) in [(&lhs, &rhs), (&rhs, &lhs)] {
                        let result = lhs * rhs;
                        assert!(result.is_ok());
                        let value = result.unwrap();
                        assert!(value.is_zero());
                        assert!(!value.is_negative());
                        assert_eq!(value.frac_cnt(), expected_scale);
                        assert_eq!(value.result_frac_cnt(), expected_scale);
                        assert_eq!(
                            value.to_string(),
                            format!("0.{}", "0".repeat(usize::try_from(expected_scale).unwrap()))
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_exact_parts_round_trip() {
        // Exercise every storage digit-count split, not just total precision:
        // e.g. (80, 1) fits 81 digits but needs ten separately aligned words.
        for int_digits in 0_u8..=81 {
            for frac_digits in 0_u8..=81 {
                let int_words = usize::from(int_digits).div_ceil(9);
                let frac_words = usize::from(frac_digits).div_ceil(9);
                let used_words = int_words + frac_words;
                let mut parts = DecimalParts {
                    int_digits,
                    frac_digits,
                    result_frac_digits: frac_digits,
                    negative: false,
                    words: [WORD_MAX; 9],
                };
                if used_words == 0 || used_words > 9 {
                    assert!(matches!(
                        Decimal::try_from_parts(parts),
                        Err(Error::InvalidDataType(_))
                    ));
                    continue;
                }
                if int_digits % 9 != 0 {
                    parts.words[0] = TEN_POW[usize::from(int_digits % 9)] - 1;
                }
                if frac_digits % 9 != 0 {
                    let padding = TEN_POW[usize::from(9 - frac_digits % 9)];
                    parts.words[used_words - 1] = WORD_MAX / padding * padding;
                }
                let value = Decimal::try_from_parts(parts).unwrap();
                assert_eq!(value.try_to_parts().unwrap(), parts);
                assert_eq!(value.frac_cnt(), u32::from(frac_digits));
                assert_eq!(value.result_frac_cnt(), u32::from(frac_digits));
            }
        }

        // Inactive capacity is retained, including bits which would not be
        // valid if an importer incorrectly interpreted them as active digits.
        let mut parts = DecimalParts {
            int_digits: 1,
            frac_digits: 1,
            result_frac_digits: 3,
            negative: false,
            words: [0; 9],
        };
        parts.words[0] = 1;
        parts.words[1] = 200_000_000;
        parts.words[8] = u32::MAX;
        let value = Decimal::try_from_parts(parts).unwrap();
        assert_eq!(value.try_to_parts().unwrap(), parts);
        assert_eq!(value.to_string(), "1.200");

        parts.words[0] = 0;
        parts.words[1] = 0;
        parts.negative = true;
        let negative_zero = Decimal::try_from_parts(parts).unwrap();
        assert!(negative_zero.is_negative());
        assert!(negative_zero.is_zero());
        assert_eq!(negative_zero.try_to_parts().unwrap(), parts);
    }

    #[test]
    fn test_exact_parts_reject_invalid() {
        let zero = Decimal::zero().try_to_parts().unwrap();
        for (int_digits, frac_digits) in [
            (0, 0),
            (82, 0),
            (0, 82),
            (80, 1),
            (81, 1),
            (u8::MAX, 0),
            (0, u8::MAX),
            (u8::MAX, u8::MAX),
        ] {
            assert!(matches!(
                Decimal::try_from_parts(DecimalParts {
                    int_digits,
                    frac_digits,
                    ..zero
                }),
                Err(Error::InvalidDataType(_))
            ));
        }
        for result_frac_digits in [82, u8::MAX] {
            assert!(matches!(
                Decimal::try_from_parts(DecimalParts {
                    result_frac_digits,
                    ..zero
                }),
                Err(Error::InvalidDataType(_))
            ));
        }
        for word in [WORD_BASE, u32::MAX] {
            let mut parts = DecimalParts {
                int_digits: 9,
                ..zero
            };
            parts.words[0] = word;
            assert!(matches!(
                Decimal::try_from_parts(parts),
                Err(Error::InvalidDataType(_))
            ));
        }
        for digits in 1_u8..9 {
            let mut parts = DecimalParts {
                int_digits: digits,
                ..zero
            };
            parts.words[0] = TEN_POW[usize::from(digits)];
            assert!(matches!(
                Decimal::try_from_parts(parts),
                Err(Error::InvalidDataType(_))
            ));
            parts.int_digits = 0;
            parts.frac_digits = digits;
            parts.words[0] = 700_000_001;
            assert!(matches!(
                Decimal::try_from_parts(parts),
                Err(Error::InvalidDataType(_))
            ));
        }
        let mut empty = DecimalParts {
            int_digits: 0,
            ..zero
        };
        empty.words[0] = 1;
        assert!(matches!(
            Decimal::try_from_parts(empty),
            Err(Error::InvalidDataType(_))
        ));
    }

    #[test]
    fn test_exact_parts_preserve_independent_scales_and_hidden_digits() {
        let result = Decimal::from(8_i64).div(&Decimal::from(7_i64), 7).unwrap();
        assert!(result.is_ok());
        let parts = result.unwrap().try_to_parts().unwrap();
        assert_eq!(parts.frac_digits, 9);
        assert_eq!(parts.result_frac_digits, 7);
        assert_eq!(&parts.words[..2], &[1, 142_857_142]);
        let value = Decimal::try_from_parts(parts).unwrap();
        assert_eq!(value.try_to_parts().unwrap(), parts);
        assert_eq!(value.to_string(), "1.1428571");

        // This proves exact transport, not >30-scale formatting compatibility.
        for result_frac_digits in [0, 3, 30, 31, 81] {
            let parts = DecimalParts {
                result_frac_digits,
                ..parts
            };
            assert_eq!(
                Decimal::try_from_parts(parts)
                    .unwrap()
                    .try_to_parts()
                    .unwrap(),
                parts
            );
        }
    }

    #[test]
    fn test_exact_parts_preserve_status_payloads() {
        // Existing asymmetric truncation behavior is not changed by transport.
        let lhs = Decimal::from_str("999999999999999999999999999999999.9999").unwrap();
        let rhs = Decimal::from_str("766507373740683764182618847769240.9770").unwrap();
        let truncated = &lhs * &rhs;
        assert!(truncated.is_truncated());
        let large = Decimal::from_str(&format!("1{}", "0".repeat(60))).unwrap();
        let negative_large = -large.clone();
        let overflow = &negative_large * &large;
        assert!(overflow.is_overflow());
        assert!(overflow.is_negative());
        assert!(overflow.is_zero());
        assert_eq!(overflow.to_string(), "-0");
        for result in [Res::Ok(Decimal::from(123_i64)), truncated, overflow] {
            let parts = result.map(|d| d.try_to_parts().unwrap());
            let round_trip =
                parts.map(|p| Decimal::try_from_parts(p).unwrap().try_to_parts().unwrap());
            assert_eq!(round_trip, parts);
        }

        // The focused successful-zero fix leaves the existing Truncated-zero
        // payload convention intact; it must not accidentally become Ok.
        let zero = Decimal::try_from_parts(DecimalParts {
            int_digits: 1,
            frac_digits: 36,
            result_frac_digits: 30,
            negative: false,
            words: [0; 9],
        })
        .unwrap();
        assert_eq!(
            (&zero * &negative_large).map(|d| d.try_to_parts().unwrap()),
            Res::Truncated(Decimal::zero().try_to_parts().unwrap())
        );
    }

    #[test]
    fn test_exact_parts_reject_over_capacity_overflow_payload() {
        // Legacy multiplication returns before clamping fractional capacity.
        // Preserve its status/header through a safe logical view, not a clipped
        // fixed export. Malformed fixed import must still fail independently.
        let lhs = Decimal::from_str(&format!("-1{}.1", "0".repeat(60))).unwrap();
        let rhs = Decimal::from_str(&format!("1{}.1", "0".repeat(60))).unwrap();
        let result = &lhs * &rhs;
        assert!(result.is_overflow());
        let value = result.unwrap();
        let view = value.words();
        assert!(view.negative);
        assert_eq!(view.int_digits, 81);
        assert_eq!(view.storage_frac, 2);
        assert_eq!(view.result_frac, 2);
        assert_eq!(view.words.len(), 10);
        assert!(view.words.iter().all(|word| *word == 0));
        assert!(matches!(
            value.try_to_parts(),
            Err(Error::InvalidDataType(_))
        ));
        let mut words = [0; WORD_BUF_LEN];
        words.copy_from_slice(&view.words[..WORD_BUF_LEN]);
        let parts = DecimalParts {
            int_digits: 81,
            frac_digits: 2,
            result_frac_digits: 2,
            negative: true,
            words,
        };
        assert!(matches!(
            Decimal::try_from_parts(parts),
            Err(Error::InvalidDataType(_))
        ));
    }

    #[test]
    fn test_from_i64() {
        let cases = vec![
            (-12345i64, "-12345"),
            (-1, "-1"),
            (1, "1"),
            (-9223372036854775807, "-9223372036854775807"),
            (-9223372036854775808, "-9223372036854775808"),
        ];

        for (num, exp) in cases {
            let dec: Decimal = num.into();
            let dec_str = dec.to_string_value();
            assert_eq!(dec_str, exp);
        }
    }

    #[test]
    fn test_from_u64() {
        let cases = vec![
            (12345u64, "12345"),
            (0, "0"),
            (18446744073709551615, "18446744073709551615"),
        ];

        for (num, exp) in cases {
            let dec: Decimal = num.into();
            let dec_str = dec.to_string_value();
            assert_eq!(dec_str, exp);
        }
    }

    #[test]
    fn test_from_f64() {
        let cs = vec![
            (f64::INFINITY, Err(Error::InvalidDataType(String::new()))),
            (-f64::INFINITY, Err(Error::InvalidDataType(String::new()))),
            (10.123, Ok(Decimal::from_str("10.123").unwrap())),
            (-10.123, Ok(Decimal::from_str("-10.123").unwrap())),
            (10.111, Ok(Decimal::from_str("10.111").unwrap())),
            (-10.111, Ok(Decimal::from_str("-10.111").unwrap())),
            (
                18446744073709552000.0,
                Ok(Decimal::from_str("18446744073709552000").unwrap()),
            ),
            (
                -18446744073709552000.0,
                Ok(Decimal::from_str("-18446744073709552000").unwrap()),
            ),
            // FIXME: because of rust's bug,
            // (1<<64)(18446744073709551616), (1<<65)(36893488147419103232) can not be represent
            // by f64  so these cases can not pass
            // (18446744073709551616.0, Ok(Decimal::from_str("18446744073709551616").unwrap())),
            // (-18446744073709551616.0, Ok(Decimal::from_str("-18446744073709551616").unwrap())),
            // (36893488147419103000.0, Ok(Decimal::from_str("36893488147419103000.0").unwrap())),
            // (
            //    -36893488147419103000.0,
            //    Ok(Decimal::from_str("-36893488147419103000.0").unwrap())
            // ),
            (
                36893488147419103000.0,
                Ok(Decimal::from_str("36893488147419103000.0").unwrap()),
            ),
            (
                -36893488147419103000.0,
                Ok(Decimal::from_str("-36893488147419103000.0").unwrap()),
            ),
        ];
        for (input, expect) in cs {
            let r = Decimal::from_f64(input);
            let log = format!(
                "input: {}, expect: {:?}, output: {:?}",
                input,
                expect.as_ref().map(|x| x.to_string_value()),
                r.as_ref().map(|x| x.to_string_value())
            );
            match expect {
                Err(e) => {
                    assert!(r.is_err(), "{}", log.as_str());
                    match e {
                        Error::InvalidDataType(_) => (),
                        _ => panic!("{}", log.as_str()),
                    }
                }
                Ok(d) => {
                    assert!(r.is_ok(), "{}", log.as_str());
                    assert_eq!(r.unwrap(), d, "{}", log.as_str());
                }
            }
        }
    }

    #[test]
    fn test_to_i64() {
        let cases = vec![
            (
                "18446744073709551615",
                Res::Overflow(9223372036854775807i64),
            ),
            ("-1", Res::Ok(-1)),
            ("1", Res::Ok(1)),
            ("-1.23", Res::Truncated(-1)),
            ("-9223372036854775807", Res::Ok(-9223372036854775807)),
            ("-9223372036854775808", Res::Ok(-9223372036854775808)),
            ("9223372036854775808", Res::Overflow(9223372036854775807)),
            ("-9223372036854775809", Res::Overflow(-9223372036854775808)),
        ];

        for (dec_str, exp) in cases {
            let dec: Decimal = dec_str.parse().unwrap();
            let i = dec.as_i64();
            assert_eq!(i, exp);
        }
    }

    #[test]
    fn test_to_u64() {
        let cases = vec![
            ("12345", Res::Ok(12345u64)),
            ("0", Res::Ok(0)),
            // ULLONG_MAX = 18446744073709551615ULL
            ("18446744073709551615", Res::Ok(18446744073709551615)),
            ("18446744073709551616", Res::Overflow(18446744073709551615)),
            ("-1", Res::Overflow(0)),
            ("1.23", Res::Truncated(1)),
            (
                "9999999999999999999999999.000",
                Res::Overflow(18446744073709551615),
            ),
        ];

        for (dec_str, exp) in cases {
            let dec: Decimal = dec_str.parse().unwrap();
            let i = dec.as_u64();
            assert_eq!(i, exp);
        }
    }

    #[test]
    #[allow(clippy::approx_constant, clippy::excessive_precision)]
    fn test_to_f64() {
        let cases = vec![
            ("12345", 12345f64),
            ("123.45", 123.45),
            ("-123.45", -123.45),
            ("0.00012345000098765", 0.00012345000098765),
            ("1234500009876.5", 1234500009876.5),
            ("3.141592653589793", 3.141592653589793),
            ("3", 3f64),
            ("1234567890123456", 1234567890123456f64),
            ("1234567890123456000", 1234567890123456000f64),
            ("1234.567890123456", 1234.567890123456),
            ("0.1234567890123456", 0.1234567890123456),
            ("0", 0f64),
            ("0.111111111111111", 0.1111111111111110),
            ("0.1111111111111111", 0.1111111111111111),
            ("0.1111111111111119", 0.1111111111111119),
            ("0.000000000000000001", 0.000000000000000001),
            ("0.000000000000000002", 0.000000000000000002),
            ("0.000000000000000003", 0.000000000000000003),
            ("0.000000000000000005", 0.000000000000000005),
            ("0.000000000000000008", 0.000000000000000008),
            ("0.1000000000000001", 0.1000000000000001),
            ("0.1000000000000002", 0.1000000000000002),
            ("0.1000000000000003", 0.1000000000000003),
            ("0.1000000000000005", 0.1000000000000005),
            ("0.1000000000000008", 0.1000000000000008),
        ];

        let mut ctx = EvalContext::default();
        for (dec_str, exp) in cases {
            let dec = dec_str.parse::<Decimal>().unwrap();
            let res = dec.to_string_value();
            assert_eq!(res, dec_str);

            let f: f64 = dec.convert(&mut ctx).unwrap();
            assert!(
                (exp - f).abs() < f64::EPSILON,
                "expect: {}, got: {}",
                exp,
                f
            );
        }
    }

    #[test]
    fn test_shift() {
        let cases = vec![
            (
                WORD_BUF_LEN,
                b"123.123" as &'static [u8],
                1,
                Res::Ok("1231.23"),
            ),
            (
                WORD_BUF_LEN,
                b"123457189.123123456789000",
                1,
                Res::Ok("1234571891.23123456789"),
            ),
            (
                WORD_BUF_LEN,
                b"123457189.123123456789000",
                8,
                Res::Ok("12345718912312345.6789"),
            ),
            (
                WORD_BUF_LEN,
                b"123457189.123123456789000",
                9,
                Res::Ok("123457189123123456.789"),
            ),
            (
                WORD_BUF_LEN,
                b"123457189.123123456789000",
                10,
                Res::Ok("1234571891231234567.89"),
            ),
            (
                WORD_BUF_LEN,
                b"123457189.123123456789000",
                17,
                Res::Ok("12345718912312345678900000"),
            ),
            (
                WORD_BUF_LEN,
                b"123457189.123123456789000",
                18,
                Res::Ok("123457189123123456789000000"),
            ),
            (
                WORD_BUF_LEN,
                b"123457189.123123456789000",
                19,
                Res::Ok("1234571891231234567890000000"),
            ),
            (
                WORD_BUF_LEN,
                b"123457189.123123456789000",
                26,
                Res::Ok("12345718912312345678900000000000000"),
            ),
            (
                WORD_BUF_LEN,
                b"123457189.123123456789000",
                27,
                Res::Ok("123457189123123456789000000000000000"),
            ),
            (
                WORD_BUF_LEN,
                b"123457189.123123456789000",
                28,
                Res::Ok("1234571891231234567890000000000000000"),
            ),
            (
                WORD_BUF_LEN,
                b"000000000000000000000000123457189.123123456789000",
                26,
                Res::Ok("12345718912312345678900000000000000"),
            ),
            (
                WORD_BUF_LEN,
                b"00000000123457189.123123456789000",
                27,
                Res::Ok("123457189123123456789000000000000000"),
            ),
            (
                WORD_BUF_LEN,
                b"00000000000000000123457189.123123456789000",
                28,
                Res::Ok("1234571891231234567890000000000000000"),
            ),
            (WORD_BUF_LEN, b"123", 1, Res::Ok("1230")),
            (WORD_BUF_LEN, b"123", 10, Res::Ok("1230000000000")),
            (WORD_BUF_LEN, b".123", 1, Res::Ok("1.23")),
            (WORD_BUF_LEN, b".123", 10, Res::Ok("1230000000")),
            (WORD_BUF_LEN, b".123", 14, Res::Ok("12300000000000")),
            (WORD_BUF_LEN, b"000.000", 1000, Res::Ok("0")),
            (WORD_BUF_LEN, b"000.", 1000, Res::Ok("0")),
            (WORD_BUF_LEN, b".000", 1000, Res::Ok("0")),
            (WORD_BUF_LEN, b"1", 1000, Res::Overflow("1")),
            (WORD_BUF_LEN, b"123.123", -1, Res::Ok("12.3123")),
            (
                WORD_BUF_LEN,
                b"123987654321.123456789000",
                -1,
                Res::Ok("12398765432.1123456789"),
            ),
            (
                WORD_BUF_LEN,
                b"123987654321.123456789000",
                -2,
                Res::Ok("1239876543.21123456789"),
            ),
            (
                WORD_BUF_LEN,
                b"123987654321.123456789000",
                -3,
                Res::Ok("123987654.321123456789"),
            ),
            (
                WORD_BUF_LEN,
                b"123987654321.123456789000",
                -8,
                Res::Ok("1239.87654321123456789"),
            ),
            (
                WORD_BUF_LEN,
                b"123987654321.123456789000",
                -9,
                Res::Ok("123.987654321123456789"),
            ),
            (
                WORD_BUF_LEN,
                b"123987654321.123456789000",
                -10,
                Res::Ok("12.3987654321123456789"),
            ),
            (
                WORD_BUF_LEN,
                b"123987654321.123456789000",
                -11,
                Res::Ok("1.23987654321123456789"),
            ),
            (
                WORD_BUF_LEN,
                b"123987654321.123456789000",
                -12,
                Res::Ok("0.123987654321123456789"),
            ),
            (
                WORD_BUF_LEN,
                b"123987654321.123456789000",
                -13,
                Res::Ok("0.0123987654321123456789"),
            ),
            (
                WORD_BUF_LEN,
                b"123987654321.123456789000",
                -14,
                Res::Ok("0.00123987654321123456789"),
            ),
            (
                WORD_BUF_LEN,
                b"00000087654321.123456789000",
                -14,
                Res::Ok("0.00000087654321123456789"),
            ),
            (2, b"123.123", -2, Res::Ok("1.23123")),
            (2, b"123.123", -3, Res::Ok("0.123123")),
            (2, b"123.123", -6, Res::Ok("0.000123123")),
            (2, b"123.123", -7, Res::Ok("0.0000123123")),
            (2, b"123.123", -15, Res::Ok("0.000000000000123123")),
            (2, b"123.123", -16, Res::Truncated("0.000000000000012312")),
            (2, b"123.123", -17, Res::Truncated("0.000000000000001231")),
            (2, b"123.123", -18, Res::Truncated("0.000000000000000123")),
            (2, b"123.123", -19, Res::Truncated("0.000000000000000012")),
            (2, b"123.123", -20, Res::Truncated("0.000000000000000001")),
            (2, b"123.123", -21, Res::Truncated("0")),
            (2, b".000000000123", -1, Res::Ok("0.0000000000123")),
            (2, b".000000000123", -6, Res::Ok("0.000000000000000123")),
            (
                2,
                b".000000000123",
                -7,
                Res::Truncated("0.000000000000000012"),
            ),
            (
                2,
                b".000000000123",
                -8,
                Res::Truncated("0.000000000000000001"),
            ),
            (2, b".000000000123", -9, Res::Truncated("0")),
            (2, b".000000000123", 1, Res::Ok("0.00000000123")),
            (2, b".000000000123", 8, Res::Ok("0.0123")),
            (2, b".000000000123", 9, Res::Ok("0.123")),
            (2, b".000000000123", 10, Res::Ok("1.23")),
            (2, b".000000000123", 17, Res::Ok("12300000")),
            (2, b".000000000123", 18, Res::Ok("123000000")),
            (2, b".000000000123", 19, Res::Ok("1230000000")),
            (2, b".000000000123", 20, Res::Ok("12300000000")),
            (2, b".000000000123", 21, Res::Ok("123000000000")),
            (2, b".000000000123", 22, Res::Ok("1230000000000")),
            (2, b".000000000123", 23, Res::Ok("12300000000000")),
            (2, b".000000000123", 24, Res::Ok("123000000000000")),
            (2, b".000000000123", 25, Res::Ok("1230000000000000")),
            (2, b".000000000123", 26, Res::Ok("12300000000000000")),
            (2, b".000000000123", 27, Res::Ok("123000000000000000")),
            (2, b".000000000123", 28, Res::Overflow("0.000000000123")),
            (
                2,
                b"123456789.987654321",
                -1,
                Res::Truncated("12345678.998765432"),
            ),
            (
                2,
                b"123456789.987654321",
                -2,
                Res::Truncated("1234567.899876543"),
            ),
            (2, b"123456789.987654321", -8, Res::Truncated("1.234567900")),
            (
                2,
                b"123456789.987654321",
                -9,
                Res::Ok("0.123456789987654321"),
            ),
            (
                2,
                b"123456789.987654321",
                -10,
                Res::Truncated("0.012345678998765432"),
            ),
            (
                2,
                b"123456789.987654321",
                -17,
                Res::Truncated("0.000000001234567900"),
            ),
            (
                2,
                b"123456789.987654321",
                -18,
                Res::Truncated("0.000000000123456790"),
            ),
            (
                2,
                b"123456789.987654321",
                -19,
                Res::Truncated("0.000000000012345679"),
            ),
            (
                2,
                b"123456789.987654321",
                -26,
                Res::Truncated("0.000000000000000001"),
            ),
            (2, b"123456789.987654321", -27, Res::Truncated("0")),
            (2, b"123456789.987654321", 1, Res::Truncated("1234567900")),
            (2, b"123456789.987654321", 2, Res::Truncated("12345678999")),
            (
                2,
                b"123456789.987654321",
                4,
                Res::Truncated("1234567899877"),
            ),
            (
                2,
                b"123456789.987654321",
                8,
                Res::Truncated("12345678998765432"),
            ),
            (2, b"123456789.987654321", 9, Res::Ok("123456789987654321")),
            (
                2,
                b"123456789.987654321",
                10,
                Res::Overflow("123456789.987654321"),
            ),
            (2, b"123456789.987654321", 0, Res::Ok("123456789.987654321")),
            (
                WORD_BUF_LEN,
                b"0.0000000070415291131966574",
                -9223372036854775808,
                Res::Truncated("0"),
            ),
            (
                WORD_BUF_LEN,
                b"0.0000000070415291131966574",
                9223372036854775807,
                Res::Overflow("0.0000000070415291131966574"),
            ),
        ];

        for (word_buf_len, dec, shift, exp) in cases {
            let dec = Decimal::from_bytes_with_word_buf(dec, word_buf_len)
                .unwrap()
                .unwrap();
            let shifted = dec.shift_with_word_buf_len(shift, word_buf_len);
            let res = shifted.map(|d| d.to_string_value());
            assert_eq!(res, exp.map(ToOwned::to_owned));
        }
    }

    #[test]
    fn test_round() {
        let cases = vec![
            (
                "123456789.987654321",
                1,
                Res::Ok("123456790.0"),
                Res::Ok("123456789.9"),
                Res::Ok("123456790.0"),
            ),
            ("15.1", 0, Res::Ok("15"), Res::Ok("15"), Res::Ok("16")),
            ("15.5", 0, Res::Ok("16"), Res::Ok("15"), Res::Ok("16")),
            ("15.9", 0, Res::Ok("16"), Res::Ok("15"), Res::Ok("16")),
            ("-15.1", 0, Res::Ok("-15"), Res::Ok("-15"), Res::Ok("-16")),
            ("-15.5", 0, Res::Ok("-16"), Res::Ok("-15"), Res::Ok("-16")),
            ("-15.9", 0, Res::Ok("-16"), Res::Ok("-15"), Res::Ok("-16")),
            ("15.1", 1, Res::Ok("15.1"), Res::Ok("15.1"), Res::Ok("15.1")),
            (
                "-15.1",
                1,
                Res::Ok("-15.1"),
                Res::Ok("-15.1"),
                Res::Ok("-15.1"),
            ),
            (
                "15.17",
                1,
                Res::Ok("15.2"),
                Res::Ok("15.1"),
                Res::Ok("15.2"),
            ),
            ("15.4", -1, Res::Ok("20"), Res::Ok("10"), Res::Ok("20")),
            ("-15.4", -1, Res::Ok("-20"), Res::Ok("-10"), Res::Ok("-20")),
            ("5.4", -1, Res::Ok("10"), Res::Ok("0"), Res::Ok("10")),
            (".999", 0, Res::Ok("1"), Res::Ok("0"), Res::Ok("1")),
            (
                "999999999",
                -9,
                Res::Ok("1000000000"),
                Res::Ok("0"),
                Res::Ok("1000000000"),
            ),
        ];

        for (dec_str, scale, half_exp, trunc_exp, ceil_exp) in cases {
            let dec = dec_str.parse::<Decimal>().unwrap();
            let round_dec = dec.clone().round(scale, RoundMode::HalfEven);
            assert_eq!(round_dec.frac_cnt, round_dec.result_frac_cnt);
            let res = round_dec.map(|d| d.to_string_value());
            assert_eq!(res, half_exp.map(|s| s.to_owned()));
            let round_dec = dec.clone().round(scale, RoundMode::Truncate);
            assert_eq!(round_dec.frac_cnt, round_dec.result_frac_cnt);
            let res = round_dec.map(|d| d.to_string_value());
            assert_eq!(res, trunc_exp.map(|s| s.to_owned()));
            let round_dec = dec.round(scale, RoundMode::Ceiling);
            assert_eq!(round_dec.frac_cnt, round_dec.result_frac_cnt);
            let res = round_dec.map(|d| d.to_string_value());
            assert_eq!(res, ceil_exp.map(|s| s.to_owned()));
        }
    }

    #[test]
    #[rustfmt::skip]
    fn test_string() {
        let cases = vec![
            (WORD_BUF_LEN, b"12345" as &'static [u8], Res::Ok("12345")),
            (WORD_BUF_LEN, b"12345.", Res::Ok("12345")),
            (WORD_BUF_LEN, b"123.45.", Res::Truncated("123.45")),
            (WORD_BUF_LEN, b"-123.45.", Res::Truncated("-123.45")),
            (
                WORD_BUF_LEN,
                b".00012345000098765",
                Res::Ok("0.00012345000098765"),
            ),
            (
                WORD_BUF_LEN,
                b".12345000098765",
                Res::Ok("0.12345000098765"),
            ),
            (
                WORD_BUF_LEN,
                b"-.000000012345000098765",
                Res::Ok("-0.000000012345000098765"),
            ),
            (WORD_BUF_LEN, b"1234500009876.5", Res::Ok("1234500009876.5")),
            (WORD_BUF_LEN, b"123E5", Res::Ok("12300000")),
            (WORD_BUF_LEN, b"123E-2", Res::Ok("1.23")),
            (1, b"123450000098765", Res::Overflow("98765")),
            (1, b"123450.000098765", Res::Truncated("123450")),
            (WORD_BUF_LEN, b"123.123", Res::Ok("123.123")),
            (WORD_BUF_LEN, b"123.1230", Res::Ok("123.1230")),
            (WORD_BUF_LEN, b"00123.123", Res::Ok("123.123")),
            (WORD_BUF_LEN, b"1.21", Res::Ok("1.21")),
            (WORD_BUF_LEN, b".21", Res::Ok("0.21")),
            (WORD_BUF_LEN, b"1.00", Res::Ok("1.00")),
            (WORD_BUF_LEN, b"100", Res::Ok("100")),
            (WORD_BUF_LEN, b"-100", Res::Ok("-100")),
            (WORD_BUF_LEN, b"100.00", Res::Ok("100.00")),
            (WORD_BUF_LEN, b"00100.00", Res::Ok("100.00")),
            (WORD_BUF_LEN, b"-100.00", Res::Ok("-100.00")),
            (WORD_BUF_LEN, b"-0.00", Res::Ok("0.00")),
            (WORD_BUF_LEN, b"00.00", Res::Ok("0.00")),
            (WORD_BUF_LEN, b"0.00", Res::Ok("0.00")),
            (WORD_BUF_LEN, b"-2.010", Res::Ok("-2.010")),
            (WORD_BUF_LEN, b"12345", Res::Ok("12345")),
            (WORD_BUF_LEN, b"-12345", Res::Ok("-12345")),
            (WORD_BUF_LEN, b"-3.", Res::Ok("-3")),
            (WORD_BUF_LEN, b"1.456e3", Res::Ok("1456")),
            (WORD_BUF_LEN, b"3.", Res::Ok("3")),
            (WORD_BUF_LEN, b"314e-2", Res::Ok("3.14")),
            (WORD_BUF_LEN, b"1e2", Res::Ok("100")),
            (WORD_BUF_LEN, b"2E-1", Res::Ok("0.2")),
            (WORD_BUF_LEN, b"2E0", Res::Ok("2")),
            (WORD_BUF_LEN, b"2.2E-1", Res::Ok("0.22")),
            (WORD_BUF_LEN, b"2.23E2", Res::Ok("223")),
            (WORD_BUF_LEN, b"2.23E2abc", Res::Ok("223")),
            (WORD_BUF_LEN, b"2.23a2", Res::Truncated("2.23")),
            (WORD_BUF_LEN, b"223\xE0\x80\x80", Res::Truncated("223")),
            (WORD_BUF_LEN, b"223  ", Res::Ok("223")),
            (WORD_BUF_LEN, b"223.2  ", Res::Ok("223.2")),
            (WORD_BUF_LEN, b"223.2  .", Res::Truncated("223.2")),
            (WORD_BUF_LEN, b"1e -1", Res::Ok("0.1")),
            (WORD_BUF_LEN, b"1e001", Res::Ok("10")),
            (WORD_BUF_LEN, b"1e00", Res::Ok("1")),
            (WORD_BUF_LEN, b"1e1073741823",
             Res::Overflow("999999999999999999999999999999999999999999999999999999999999999999999999999999999")),
            (WORD_BUF_LEN, b"-1e1073741823",
             Res::Overflow("-999999999999999999999999999999999999999999999999999999999999999999999999999999999")),
            (WORD_BUF_LEN, b"135999696916777530000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
             Res::Overflow("0")),
            (WORD_BUF_LEN, b"-0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002932935661422768",
             Res::Truncated("0.000000000000000000000000000000000000000000000000000000000000000000000000")),
            // The following case return truncated in tidb, need to fix it in bytes_to_int_without_context
            (WORD_BUF_LEN, b"1eabc", Res::Ok("1")),
            (WORD_BUF_LEN, b"1e", Res::Ok("1")),
            (WORD_BUF_LEN, b"1e 1ddd", Res::Ok("10")),
            (WORD_BUF_LEN, b"1e - 1", Res::Ok("1")),
            // with word_buf_len 1
            (1, b"123450000098765", Res::Overflow("98765")),
            (1, b"123450.000098765", Res::Truncated("123450")),
        ];

        for (word_buf_len, dec, exp) in cases {
            let d = Decimal::from_bytes_with_word_buf(dec, word_buf_len).unwrap();
            let res = d.map(|d| d.to_string_value());
            assert_eq!(res, exp.map(|s| s.to_owned()));
        }

        // error cases
        let cases = vec![b"1e18446744073709551620"];
        for case in cases {
            Decimal::from_bytes(case).unwrap_err();
        }
    }

    #[test]
    fn test_codec() {
        let cases = vec![
            ("-10.55", 4, 2, Res::Ok("-10.55")),
            (
                "0.0123456789012345678912345",
                30,
                25,
                Res::Ok("0.0123456789012345678912345"),
            ),
            ("12345", 5, 0, Res::Ok("12345")),
            ("12345", 10, 3, Res::Ok("12345.000")),
            ("123.45", 10, 3, Res::Ok("123.450")),
            ("-123.45", 20, 10, Res::Ok("-123.4500000000")),
            (
                ".00012345000098765",
                15,
                14,
                Res::Truncated("0.00012345000098"),
            ),
            (
                ".00012345000098765",
                22,
                20,
                Res::Ok("0.00012345000098765000"),
            ),
            (".12345000098765", 30, 20, Res::Ok("0.12345000098765000000")),
            (
                "-.000000012345000098765",
                30,
                20,
                Res::Truncated("-0.00000001234500009876"),
            ),
            ("1234500009876.5", 30, 5, Res::Ok("1234500009876.50000")),
            ("111111111.11", 10, 2, Res::Overflow("11111111.11")),
            ("000000000.01", 7, 3, Res::Ok("0.010")),
            ("123.4", 10, 2, Res::Ok("123.40")),
            ("1000", 3, 0, Res::Overflow("0")),
            (
                "10000000000000000000.23",
                23,
                2,
                Res::Ok("10000000000000000000.23"),
            ),
        ];

        for (dec_str, prec, frac, exp) in cases {
            let dec = dec_str.parse::<Decimal>().unwrap();
            let mut buf = vec![];
            let res = buf.write_decimal(&dec, prec, frac).unwrap();
            let decoded = buf.as_slice().read_decimal().unwrap();
            let res = res.map(|_| decoded.to_string_value());
            assert_eq!(res, exp.map(|s| s.to_owned()));
        }
    }

    #[test]
    fn test_chunk_codec() {
        let cases = vec![
            "-10.55",
            "0.0123456789012345678912345",
            "12345",
            "12345",
            "123.45",
            ".00012345000098765",
            ".00012345000098765",
            ".12345000098765",
            "1234500009876.5",
            "111111111.11",
            "000000000.01",
            "123.4",
            "1000",
            "10000000000000000000.23",
        ];

        for dec_str in cases {
            let dec = dec_str.parse::<Decimal>().unwrap();
            let mut buf = vec![];
            buf.write_decimal_to_chunk(&dec).unwrap();
            buf.resize(DECIMAL_STRUCT_SIZE, 0);
            let decoded = buf.as_slice().read_decimal_from_chunk().unwrap();
            assert_eq!(decoded, dec);
        }
    }

    #[test]
    fn test_decode_chunk_from_tidb() {
        let src: Vec<u8> = vec![
            3, 3, 3, 0, 123, 0, 0, 0, 0, 2, 46, 27, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let decoded = src.as_slice().read_decimal_from_chunk().unwrap();
        assert_eq!(Decimal::from_f64(123.456).unwrap(), decoded);
    }

    #[test]
    fn test_cmp() {
        let cases = vec![
            ("12", "13", Ordering::Less),
            ("13", "12", Ordering::Greater),
            ("-10", "10", Ordering::Less),
            ("10", "-10", Ordering::Greater),
            ("-12", "-13", Ordering::Greater),
            ("0", "12", Ordering::Less),
            ("-10", "0", Ordering::Less),
            ("4", "4", Ordering::Equal),
            ("4", "4.00", Ordering::Equal),
            ("-1.1", "-1.2", Ordering::Greater),
            ("1.2", "1.1", Ordering::Greater),
            ("1.1", "1.2", Ordering::Less),
        ];

        for (lhs_str, rhs_str, exp) in cases {
            let lhs = lhs_str.parse::<Decimal>().unwrap();
            let rhs = rhs_str.parse::<Decimal>().unwrap();
            assert_eq!(lhs.cmp(&rhs), exp);
        }
    }

    #[test]
    fn test_hash() {
        let cases = vec![
            ("1.00", "1"),
            ("-1.11", "-1.11000000"),
            ("30.20", "30.2"),
            ("0", "-0"),
            ("0.001", "0.001000"),
        ];

        for (lhs_str, rhs_str) in cases {
            let lhs = lhs_str.parse::<Decimal>().unwrap();
            let rhs = rhs_str.parse::<Decimal>().unwrap();
            let mut lhasher = DefaultHasher::new();
            lhs.hash(&mut lhasher);
            let mut rhasher = DefaultHasher::new();
            rhs.hash(&mut rhasher);
            assert_eq!(lhasher.finish(), rhasher.finish());
        }
    }

    #[test]
    fn test_max_decimal() {
        let cases = vec![
            (1, 1, "0.9"),
            (1, 0, "9"),
            (2, 1, "9.9"),
            (4, 2, "99.99"),
            (6, 3, "999.999"),
            (8, 4, "9999.9999"),
            (10, 5, "99999.99999"),
            (12, 6, "999999.999999"),
            (14, 7, "9999999.9999999"),
            (16, 8, "99999999.99999999"),
            (18, 9, "999999999.999999999"),
            (20, 10, "9999999999.9999999999"),
            (20, 20, "0.99999999999999999999"),
            (20, 0, "99999999999999999999"),
            (40, 20, "99999999999999999999.99999999999999999999"),
        ];

        for (prec, frac, exp) in cases {
            let dec = super::max_decimal(prec, frac);
            let res = dec.to_string_value();
            assert_eq!(&res, exp);
        }
    }

    #[test]
    fn test_add() {
        let a = "2".to_owned() + &"1".repeat(71);
        let b: String = "8".repeat(81);
        let c = "8888888890".to_owned() + &"9".repeat(71);
        let cases = vec![
            (
                ".00012345000098765",
                "123.45",
                Res::Ok("123.45012345000098765"),
            ),
            (".1", ".45", Res::Ok("0.55")),
            (
                "1234500009876.5",
                ".00012345000098765",
                Res::Ok("1234500009876.50012345000098765"),
            ),
            ("9999909999999.5", ".555", Res::Ok("9999910000000.055")),
            ("99999999", "1", Res::Ok("100000000")),
            ("989999999", "1", Res::Ok("990000000")),
            ("999999999", "1", Res::Ok("1000000000")),
            ("12345", "123.45", Res::Ok("12468.45")),
            ("-12345", "-123.45", Res::Ok("-12468.45")),
            ("-12345", "123.45", Res::Ok("-12221.55")),
            ("12345", "-123.45", Res::Ok("12221.55")),
            ("123.45", "-12345", Res::Ok("-12221.55")),
            ("-123.45", "12345", Res::Ok("12221.55")),
            ("5", "-6.0", Res::Ok("-1.0")),
            ("2", "3", Res::Ok("5")),
            ("2454495034", "3451204593", Res::Ok("5905699627")),
            ("24544.95034", ".3451204593", Res::Ok("24545.2954604593")),
            (".1", ".1", Res::Ok("0.2")),
            (".1", "-.1", Res::Ok("0")),
            ("0", "1.001", Res::Ok("1.001")),
            (&a, &b, Res::Ok(&c)),
        ];

        for (lhs_str, rhs_str, exp) in cases {
            let lhs = lhs_str.parse::<Decimal>().unwrap();
            let rhs = rhs_str.parse::<Decimal>().unwrap();

            let res_dec = &lhs + &rhs;
            let res = res_dec.map(|s| s.to_string_value());
            let exp_str = exp.map(|s| s.to_owned());
            assert_eq!(res, exp_str);

            let res_dec = &rhs + &lhs;
            let res = res_dec.map(|s| s.to_string_value());
            assert_eq!(res, exp_str);
        }
    }

    #[test]
    fn test_sub() {
        let cases = vec![
            (
                ".00012345000098765",
                "123.45",
                Res::Ok("-123.44987654999901235"),
            ),
            (
                "1234500009876.5",
                ".00012345000098765",
                Res::Ok("1234500009876.49987654999901235"),
            ),
            ("9999900000000.5", ".555", Res::Ok("9999899999999.945")),
            ("1111.5551", "1111.555", Res::Ok("0.0001")),
            (".555", ".555", Res::Ok("0")),
            ("10000000", "1", Res::Ok("9999999")),
            ("1000001000", ".1", Res::Ok("1000000999.9")),
            ("1000000000", ".1", Res::Ok("999999999.9")),
            ("12345", "123.45", Res::Ok("12221.55")),
            ("-12345", "-123.45", Res::Ok("-12221.55")),
            ("123.45", "12345", Res::Ok("-12221.55")),
            ("-123.45", "-12345", Res::Ok("12221.55")),
            ("-12345", "123.45", Res::Ok("-12468.45")),
            ("12345", "-123.45", Res::Ok("12468.45")),
            ("3.10000000000", "2.00", Res::Ok("1.10000000000")),
            ("3.00", "2.0000000000000", Res::Ok("1.0000000000000")),
            (
                "-20048271934704078000000000000000000000000000000000000",
                "-20048271934734512000000000000000000000000000000000000",
                Res::Ok("30434000000000000000000000000000000000000"),
            ),
        ];

        for (lhs_str, rhs_str, exp) in cases {
            let lhs = lhs_str.parse::<Decimal>().unwrap();
            let rhs = rhs_str.parse::<Decimal>().unwrap();
            let res_dec = &lhs - &rhs;
            let res = res_dec.map(|s| s.to_string_value());
            assert_eq!(res, exp.map(|s| s.to_owned()));
        }
    }

    #[test]
    fn test_mul() {
        let a = "1".to_owned() + &"0".repeat(60);
        let b = "1".to_owned() + &"0".repeat(60);
        let cases = vec![
            ("12", "10", Res::Ok("120")),
            // Pinned TiDB Go oracle: successful 0 * -1.1 (both orders)
            // retains scale 1. This intentionally corrects legacy TiKV "0".
            ("0", "-1.1", Res::Ok("0.0")),
            ("-123.456", "98765.4321", Res::Ok("-12193185.1853376")),
            (
                "-123456000000",
                "98765432100000",
                Res::Ok("-12193185185337600000000000"),
            ),
            ("123456", "987654321", Res::Ok("121931851853376")),
            ("123456", "9876543210", Res::Ok("1219318518533760")),
            ("123", "0.01", Res::Ok("1.23")),
            ("123", "0", Res::Ok("0")),
            (&a, &b, Res::Overflow("0")),
            (
                "0.00000000000000",
                "0.000000000000000000000000000000000000000000000000000000000000000",
                Res::Truncated("0.0000000000000000000000000000000"),
            ),
        ];

        for (lhs_str, rhs_str, exp_str) in cases {
            let lhs: Decimal = lhs_str.parse().unwrap();
            let rhs: Decimal = rhs_str.parse().unwrap();
            let exp = exp_str.map(|s| s.to_owned());
            let res = (&lhs * &rhs).map(|d| d.to_string_value());
            assert_eq!(res, exp);

            let res = (&rhs * &lhs).map(|d| d.to_string_value());
            assert_eq!(res, exp);
        }
    }

    #[test]
    fn test_mul_truncated() {
        let cases = vec![(
            "999999999999999999999999999999999.9999",
            "766507373740683764182618847769240.9770",
            Res::Truncated(
                "766507373740683764182618847769239999923349262625931623581738115223.07600000",
            ),
            Res::Truncated(
                "766507373740683764182618847769240210492626259316235817381152230759.02300000",
            ),
        )];

        for (lhs_str, rhs_str, exp_str, rev_exp_str) in cases {
            let lhs: Decimal = lhs_str.parse().unwrap();
            let rhs: Decimal = rhs_str.parse().unwrap();
            let exp = exp_str.map(|s| s.to_owned());
            let res = (&lhs * &rhs).map(|d| d.to_string_value());
            assert_eq!(res, exp);

            let exp = rev_exp_str.map(|s| s.to_owned());
            let res = (&rhs * &lhs).map(|d| d.to_string_value());
            assert_eq!(res, exp);
        }
    }

    #[test]
    fn test_div_mod() {
        let cases = vec![
            (5, "120", "10", Some("12.000000000"), Some("0")),
            (5, "123", "0.01", Some("12300.000000000"), Some("0.00")),
            (
                5,
                "120",
                "100000000000.00000",
                Some("0.000000001200000000"),
                Some("120.00000"),
            ),
            (5, "123", "0", None, None),
            (5, "123", "0.0", None, None),
            (5, "123", "00.0000000000", None, None),
            (5, "0", "0", None, None),
            (5, "0.0", "0.0", None, None),
            (5, "0.0000000000", "00.0000000000", None, None),
            (
                5,
                "-12193185.1853376",
                "98765.4321",
                Some("-123.456000000000000000"),
                Some("-45037.0370376"),
            ),
            (
                5,
                "121931851853376",
                "987654321",
                Some("123456.000000000"),
                Some("0"),
            ),
            (5, "0", "987", Some("0"), Some("0")),
            (5, "0.0", "987", Some("0"), Some("0")),
            (5, "0.0000000000", "987", Some("0"), Some("0")),
            (5, "1", "3", Some("0.333333333"), Some("1")),
            (
                5,
                "1.000000000000",
                "3",
                Some("0.333333333333333333"),
                Some("1.000000000000"),
            ),
            (5, "1", "1", Some("1.000000000"), Some("0")),
            (
                5,
                "0.0123456789012345678912345",
                "9999999999",
                Some("0.000000000001234567890246913578148141"),
                Some("0.0123456789012345678912345"),
            ),
            (
                5,
                "10.333000000",
                "12.34500",
                Some("0.837019036046982584042122316"),
                Some("10.333000000"),
            ),
            (
                5,
                "10.000000000060",
                "2",
                Some("5.000000000030000000"),
                Some("0.000000000060"),
            ),
            (0, "234", "10", Some("23"), Some("4")),
            (
                0,
                "234.567",
                "10.555",
                Some("22.223306489815253434"),
                Some("2.357"),
            ),
            (
                0,
                "-234.567",
                "10.555",
                Some("-22.223306489815253434"),
                Some("-2.357"),
            ),
            (
                0,
                "234.567",
                "-10.555",
                Some("-22.223306489815253434"),
                Some("2.357"),
            ),
            (
                0,
                "99999999999999999999999999999999999999",
                "3",
                Some("33333333333333333333333333333333333333"),
                Some("0"),
            ),
            (
                DEFAULT_DIV_FRAC_INCR,
                "1",
                "1",
                Some("1.000000000"),
                Some("0"),
            ),
            (
                DEFAULT_DIV_FRAC_INCR,
                "1.00",
                "1",
                Some("1.000000000"),
                Some("0.00"),
            ),
            (
                DEFAULT_DIV_FRAC_INCR,
                "1",
                "1.000",
                Some("1.000000000"),
                Some("0.000"),
            ),
            (
                DEFAULT_DIV_FRAC_INCR,
                "0.0000000001",
                "1.0",
                Some("0.000000000100000000000000000"),
                Some("0.0000000001"),
            ),
            (
                DEFAULT_DIV_FRAC_INCR,
                "2",
                "3",
                Some("0.666666666"),
                Some("2"),
            ),
            (0, "1", "2.0", Some("0.500000000"), Some("1.0")),
            (0, "1.0", "2", Some("0.500000000"), Some("1.0")),
            (0, "2.23", "3", Some("0.743333333"), Some("2.23")),
            (
                DEFAULT_DIV_FRAC_INCR,
                "51",
                "0.003430",
                Some("14868.804664723032069970"),
                Some("0.002760"),
            ),
            (
                5,
                "51",
                "0.003430",
                Some("14868.804664723032069970"),
                Some("0.002760"),
            ),
            (
                0,
                "51",
                "0.003430",
                Some("14868.804664723"),
                Some("0.002760"),
            ),
            (
                5,
                "3428138243708624600000000000000000000000000000000000",
                "0.000000000000000000000000000000000000000000010962196522059515",
                Some(
                    "312723662343590746587750435944686855597018456899102054479447138416084646758822",
                ),
                Some("0.000000000000000000000000000000000003564345362392880000000000"),
            ),
            (
                0,
                "-0.000000000000000000000000000000000000000000004078816115216077",
                "770994069125765500000000000000000000000000000",
                Some("0.000000000000000000000000000000000000000000000000000000000000000"),
                Some("-0.000000000000000000000000000000000000000000004078816115216077"),
            ),
            (
                DEFAULT_DIV_FRAC_INCR,
                "-125",
                "489466941506",
                Some("0.000000000"),
                Some("-125"),
            ),
            (
                DEFAULT_DIV_FRAC_INCR,
                "-56",
                "489466941506",
                Some("0.000000000"),
                Some("-56"),
            ),
        ];

        for (frac_incr, lhs_str, rhs_str, div_exp, rem_exp) in cases {
            let lhs: Decimal = lhs_str.parse().unwrap();
            let rhs: Decimal = rhs_str.parse().unwrap();
            let res = super::do_div_mod(&lhs, &rhs, frac_incr, false)
                .map(|d| d.unwrap().to_string_value());
            assert_eq!(res, div_exp.map(|s| s.to_owned()));

            let res = super::do_div_mod(&lhs, &rhs, frac_incr, true)
                .map(|d| d.unwrap().to_string_value());
            assert_eq!(res, rem_exp.map(|s| s.to_owned()));
        }

        let div_cases = vec![
            (
                "-43791957044243810000000000000000000000000000000000000000000000000000000000000",
                "-0.0000000000000000000000000000000000000000000000000012867433602814482",
                Res::Overflow(
                    "34033171179267041433424155279291553259014210153022524070386565694757521640",
                ),
            ),
            ("0", "0.5", Res::Ok("0.0000")),
        ];
        for (lhs_str, rhs_str, div_exp) in div_cases {
            let lhs: Decimal = lhs_str.parse().unwrap();
            let rhs: Decimal = rhs_str.parse().unwrap();
            let res = (&lhs / &rhs).unwrap().map(|d| d.to_string_value());
            assert_eq!(res, div_exp.map(|s| s.to_owned()))
        }

        let rem_cases = vec![("0", "0.5", Res::Ok("0.0"))];
        for (lhs_str, rhs_str, rem_exp) in rem_cases {
            let lhs: Decimal = lhs_str.parse().unwrap();
            let rhs: Decimal = rhs_str.parse().unwrap();
            let res = (lhs % rhs).unwrap().map(|d| d.to_string_value());
            assert_eq!(res, rem_exp.map(|s| s.to_owned()))
        }
    }

    #[test]
    fn test_neg() {
        let cases = vec![
            ("123.45", "-123.45"),
            ("1", "-1"),
            ("1234500009876.5", "-1234500009876.5"),
            ("1111.5551", "-1111.5551"),
            ("0.555", "-0.555"),
            ("0", "0"),
            ("0.0", "0.0"),
            ("0.00", "0.00"),
        ];

        for (pos, neg) in cases {
            let pos_dec: Decimal = pos.parse().unwrap();
            let res = -pos_dec.clone();
            assert_eq!(res.to_string_value(), neg);
            assert!((&pos_dec + &res).is_zero());

            let neg_dec: Decimal = neg.parse().unwrap();
            let res = -neg_dec.clone();
            assert_eq!(res.to_string_value(), pos);
            assert!((&neg_dec + &res).is_zero());
        }

        let max_dec = super::max_or_min_dec(false, 40, 20);
        let min_dec = super::max_or_min_dec(true, 40, 20);
        assert_eq!(min_dec, -max_dec.clone());
        assert_eq!(max_dec, -min_dec);
    }

    #[test]
    fn test_max_or_min_decimal() {
        let cases = vec![
            (1, 1, "0.9"),
            (1, 0, "9"),
            (2, 1, "9.9"),
            (4, 2, "99.99"),
            (6, 3, "999.999"),
            (8, 4, "9999.9999"),
            (10, 5, "99999.99999"),
            (12, 6, "999999.999999"),
            (14, 7, "9999999.9999999"),
            (16, 8, "99999999.99999999"),
            (18, 9, "999999999.999999999"),
            (20, 10, "9999999999.9999999999"),
            (20, 20, "0.99999999999999999999"),
            (20, 0, "99999999999999999999"),
            (40, 20, "99999999999999999999.99999999999999999999"),
        ];

        for (prec, frac, exp) in cases {
            let positive = super::max_or_min_dec(false, prec, frac);
            let res = positive.to_string_value();
            assert_eq!(&res, exp);
            let negative = super::max_or_min_dec(true, prec, frac);
            let mut negative_exp = String::from("-");
            negative_exp.push_str(exp);
            let res = negative.to_string_value();
            assert_eq!(res, negative_exp);
        }
    }

    #[test]
    fn test_ceil() {
        let cases = vec![
            ("12345", "12345"),
            ("0.99999", "1"),
            ("-0.99999", "0"),
            ("18446744073709551615", "18446744073709551615"),
            ("18446744073709551616", "18446744073709551616"),
            ("-18446744073709551615", "-18446744073709551615"),
            ("-18446744073709551616", "-18446744073709551616"),
            ("-1", "-1"),
            ("1.23", "2"),
            ("-1.23", "-1"),
            ("1.00000", "1"),
            ("-1.00000", "-1"),
            (
                "9999999999999999999999999.001",
                "10000000000000000000000000",
            ),
        ];
        for (input, exp) in cases {
            let dec: Decimal = input.parse().unwrap();
            let exp: Decimal = exp.parse().unwrap();
            let got = dec.ceil().unwrap();
            assert_eq!(got, exp);
        }
    }

    #[test]
    fn test_floor() {
        let cases = vec![
            ("12345", "12345"),
            ("0.99999", "0"),
            ("-0.99999", "-1"),
            ("18446744073709551615", "18446744073709551615"),
            ("18446744073709551616", "18446744073709551616"),
            ("-18446744073709551615", "-18446744073709551615"),
            ("-18446744073709551616", "-18446744073709551616"),
            ("-1", "-1"),
            ("1.23", "1"),
            ("-1.23", "-2"),
            ("00001.00000", "1"),
            ("-00001.00000", "-1"),
            ("9999999999999999999999999.001", "9999999999999999999999999"),
        ];
        for (input, exp) in cases {
            let dec: Decimal = input.parse().unwrap();
            let exp: Decimal = exp.parse().unwrap();
            let got = dec.floor().unwrap();
            assert_eq!(got, exp);
        }
    }

    #[test]
    fn test_bytes_to_decimal() {
        let mut ctx = EvalContext::default();
        let cases: Vec<(&[u8], Decimal)> = vec![
            (
                b"123456.1",
                ConvertTo::<Decimal>::convert(&123456.1, &mut ctx).unwrap(),
            ),
            (
                b"-123456.1",
                ConvertTo::<Decimal>::convert(&-123456.1, &mut ctx).unwrap(),
            ),
            (b"123456", Decimal::from(123456)),
            (b"-123456", Decimal::from(-123456)),
            (b"1  ", Decimal::from(1)),
        ];
        for (s, expect) in cases {
            let got: Decimal = s.convert(&mut ctx).unwrap();
            assert_eq!(
                got, expect,
                "from {:?}, expect: {:?} got: {:?}",
                s, expect, got
            );
        }

        // OVERFLOWING
        let big = (0..85).map(|_| '9').collect::<String>();
        let val: Result<Decimal> = big.as_bytes().convert(&mut ctx);
        assert!(val.is_err(), "expected error, but got {:?}", val);
        assert_eq!(val.unwrap_err().code(), ERR_DATA_OUT_OF_RANGE);

        // OVERFLOW_AS_WARNING
        let mut ctx = EvalContext::new(Arc::new(EvalConfig::from_flag(Flag::OVERFLOW_AS_WARNING)));
        let val: Decimal = big.as_bytes().convert(&mut ctx).unwrap();
        let max = max_decimal((WORD_BUF_LEN * DIGITS_PER_WORD) as u8, 0);
        assert_eq!(val, max, "expect: {:?}, got: {:?}", val, max);
        assert_eq!(ctx.warnings.warning_cnt, 1);
        assert_eq!(ctx.warnings.warnings[0].get_code(), ERR_DATA_OUT_OF_RANGE);

        // Truncate cases
        let truncate_cases: Vec<(&[u8], Decimal)> = vec![
            (
                b"123.45.",
                ConvertTo::<Decimal>::convert(&123.45, &mut ctx).unwrap(),
            ),
            (
                b"-123.45.",
                ConvertTo::<Decimal>::convert(&-123.45, &mut ctx).unwrap(),
            ),
            (
                b"1.1.1.1.1",
                ConvertTo::<Decimal>::convert(&1.1, &mut ctx).unwrap(),
            ),
            (b"1asf", Decimal::from(1)),
            (b"1  1", Decimal::from(1)),
        ];
        for (s, expect) in truncate_cases {
            let val: Result<Decimal> = s.convert(&mut ctx);
            assert!(val.is_err(), "expected error, but got {:?}", val);
            assert_eq!(val.unwrap_err().code(), WARN_DATA_TRUNCATED);

            let mut truncate_as_warning_ctx = EvalContext::new(std::sync::Arc::new(
                EvalConfig::from_flag(Flag::TRUNCATE_AS_WARNING),
            ));
            let got: Decimal = s.convert(&mut truncate_as_warning_ctx).unwrap();
            assert_eq!(
                got, expect,
                "from {:?}, expect: {:?} got: {:?}",
                s, expect, got
            );
            assert_eq!(truncate_as_warning_ctx.warnings.warning_cnt, 1);
        }
    }

    #[test]
    fn test_private_eager_res_argument_baseline() {
        use std::cell::Cell;
        struct CountingDisplay<'a>(&'a Cell<usize>);
        impl fmt::Display for CountingDisplay<'_> {
            fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.set(self.0.get() + 1);
                out.write_str("bounded operand")
            }
        }
        let calls = Cell::new(0);
        let mut ctx = EvalContext::default();
        let payload = Res::Ok(vec![17])
            .into_result_with_overflow_err(
                &mut ctx,
                Error::overflow("DECIMAL", format!("{}", CountingDisplay(&calls))),
            )
            .unwrap();
        assert_eq!(payload, vec![17]);
        assert_eq!(calls.get(), 1); // Existing eager API stays eager.
        assert_eq!(ctx.warnings.warning_cnt, 0);
    }

    #[test]
    fn test_private_lazy_res_factory_compatibility() {
        use std::{cell::Cell, sync::Arc};
        fn result(value: Result<i32>) -> std::result::Result<i32, (i32, String)> {
            value.map_err(|error| (error.code(), error.to_string()))
        }
        fn details(ctx: &EvalContext) -> Vec<(i32, String)> {
            ctx.warnings
                .warnings
                .iter()
                .map(|error| (error.get_code(), error.get_msg().to_owned()))
                .collect()
        }
        for flags in [
            Flag::empty(),
            Flag::TRUNCATE_AS_WARNING,
            Flag::IGNORE_TRUNCATE,
            Flag::OVERFLOW_AS_WARNING | Flag::TRUNCATE_AS_WARNING,
        ] {
            for cap in [0, 1, 4] {
                for status in [Res::Ok(17), Res::Truncated(18), Res::Overflow(19)] {
                    let mut config = EvalConfig::from_flag(flags);
                    config.set_max_warning_cnt(cap);
                    let config = Arc::new(config);
                    let mut eager = EvalContext::new(config.clone());
                    let mut lazy = EvalContext::new(config);
                    for ctx in [&mut eager, &mut lazy] {
                        ctx.warnings
                            .append_warning(Error::truncated_wrong_val("prefix", "retained"));
                    }
                    let calls = Cell::new(0);
                    // Moving this String out makes the factory genuinely FnOnce.
                    let diagnostic = "(1 / 2)".to_owned();
                    let actual = status.into_result_with_overflow_err_lazy(&mut lazy, || {
                        calls.set(calls.get() + 1);
                        Error::overflow("DECIMAL", diagnostic)
                    });
                    let expected = status.into_result_with_overflow_err(
                        &mut eager,
                        Error::overflow("DECIMAL", "(1 / 2)"),
                    );
                    assert_eq!(result(actual), result(expected));
                    assert_eq!(calls.get(), usize::from(status.is_overflow()));
                    assert_eq!(lazy.warnings.warning_cnt, eager.warnings.warning_cnt);
                    assert_eq!(details(&lazy), details(&eager));
                }
            }
            // The existing private custom-truncation adapter remains compatible.
            let mut eager = EvalContext::new(Arc::new(EvalConfig::from_flag(flags)));
            let mut lazy = EvalContext::new(Arc::new(EvalConfig::from_flag(flags)));
            let calls = Cell::new(0);
            let actual = Res::Truncated(23).into_result_with_error_factory(
                &mut lazy,
                Some(Error::truncated_wrong_val("DECIMAL", "custom")),
                || {
                    calls.set(calls.get() + 1);
                    Error::overflow("DECIMAL", "unused")
                },
            );
            let expected = Res::Truncated(23).into_result_impl(
                &mut eager,
                Some(Error::truncated_wrong_val("DECIMAL", "custom")),
                None,
            );
            assert_eq!(result(actual), result(expected));
            assert_eq!(calls.get(), 0);
            assert_eq!(lazy.warnings.warning_cnt, eager.warnings.warning_cnt);
            assert_eq!(details(&lazy), details(&eager));
        }
    }

    #[test]
    fn test_into_result_impl() {
        // Truncated cases
        let mut ctx = EvalContext::default();
        let truncated_res = Res::Truncated(2333);
        let truncated_err_cases = vec![Error::truncated(), Error::truncated_wrong_val("", "")];

        for error in truncated_err_cases {
            assert_eq!(
                error.code(),
                truncated_res
                    .into_result_impl(&mut ctx, Some(error), None)
                    .unwrap_err()
                    .code()
            );
        }

        // TRUNCATE_AS_WARNING
        let mut ctx = EvalContext::new(std::sync::Arc::new(EvalConfig::from_flag(
            Flag::TRUNCATE_AS_WARNING,
        )));
        let truncated_res = Res::Truncated(2333);

        truncated_res
            .into_result_impl(&mut ctx, Some(Error::truncated()), None)
            .unwrap();

        // Overflow cases
        let mut ctx = EvalContext::default();
        let overflow_res = Res::Overflow(666);
        let error = Error::overflow("", "");
        assert_eq!(
            error.code(),
            overflow_res
                .into_result_impl(&mut ctx, None, Some(error))
                .unwrap_err()
                .code(),
        );

        // OVERFLOW_AS_WARNING
        let mut ctx = EvalContext::new(std::sync::Arc::new(EvalConfig::from_flag(
            Flag::OVERFLOW_AS_WARNING,
        )));
        let error = Error::overflow("", "");
        overflow_res
            .into_result_impl(&mut ctx, None, Some(error))
            .unwrap();
    }
}
