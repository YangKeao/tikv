// Copyright 2026 PingCAP, Inc. Licensed under Apache-2.0.

//! Native MyDecimal's binary write leaves. These preserve the original signed
//! word/count domain, warning precedence and caller-sized-buffer contract; they
//! are not the wire Decimal encoder and do not import/normalize a Decimal.
const DIGITS_PER_WORD: usize = 9;
const WORD_BUF_LEN: usize = 9;
const WORD_SIZE: usize = 4;
const MAX_SCALE: i32 = 30;
const DIG2BYTES: [usize; 10] = [0, 1, 1, 2, 2, 3, 3, 4, 4, 4];
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

/// Hard native codec failure, distinct from a value-plus-warning disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecimalCodecError {
    BadNumber,
}
/// Original soft write disposition; later fraction loss can replace overflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecimalCodecWarning {
    Truncated,
    Overflow,
}

/// Original unchecked-shape DecimalBinSize arithmetic, including signed widths.
pub fn decimal_bin_size(precision: i32, frac: i32) -> Result<usize, DecimalCodecError> {
    let digits_int = precision - frac;
    let words_int = digits_int / DIGITS_PER_WORD as i32;
    let words_frac = frac / DIGITS_PER_WORD as i32;
    let x_int = digits_int - words_int * DIGITS_PER_WORD as i32;
    let x_frac = frac - words_frac * DIGITS_PER_WORD as i32;
    if x_int < 0
        || x_int >= DIG2BYTES.len() as i32
        || x_frac < 0
        || x_frac >= DIG2BYTES.len() as i32
    {
        return Err(DecimalCodecError::BadNumber);
    }
    let size = words_int * WORD_SIZE as i32
        + DIG2BYTES[x_int as usize] as i32
        + words_frac * WORD_SIZE as i32
        + DIG2BYTES[x_frac as usize] as i32;
    usize::try_from(size).map_err(|_| DecimalCodecError::BadNumber)
}

/// Original ToBin shape checks. Deliberately does not add `frac <= precision`.
pub fn checked_bin_size(precision: i32, frac: i32) -> Result<usize, DecimalCodecError> {
    if !(0..=(DIGITS_PER_WORD * WORD_BUF_LEN) as i32).contains(&precision)
        || !(0..=MAX_SCALE).contains(&frac)
    {
        return Err(DecimalCodecError::BadNumber);
    }
    decimal_bin_size(precision, frac)
}
fn count_leading_zeroes(mut i: usize, word: i32) -> usize {
    let mut leading = 0;
    while word < POWERS10[i] {
        i -= 1;
        leading += 1;
    }
    leading
}

/// Exact codec word-prefix scan, shared with the native value-layer word view.
/// Inactive words are not validated, and original indexing panics are retained.
pub fn remove_leading_zeros(source_digits_int: i32, words: &[i32; 9]) -> (usize, i32) {
    let mut digits_int = source_digits_int;
    let mut word_idx = 0usize;
    let mut i = ((digits_int - 1) % DIGITS_PER_WORD as i32) + 1;
    while digits_int > 0 && words[word_idx] == 0 {
        digits_int -= i;
        i = DIGITS_PER_WORD as i32;
        word_idx += 1;
    }
    if digits_int > 0 {
        let start = ((digits_int - 1) % DIGITS_PER_WORD as i32) as usize;
        digits_int -= count_leading_zeroes(start, words[word_idx]) as i32;
    } else {
        digits_int = 0;
    }
    (word_idx, digits_int)
}
fn write_word(b: &mut [u8], word: i32, size: usize) {
    let v = word as u32;
    match size {
        1 => b[0] = word as u8,
        2 => {
            b[0] = (v >> 8) as u8;
            b[1] = v as u8;
        }
        3 => {
            b[0] = (v >> 16) as u8;
            b[1] = (v >> 8) as u8;
            b[2] = v as u8;
        }
        4 => {
            b[0] = (v >> 24) as u8;
            b[1] = (v >> 16) as u8;
            b[2] = (v >> 8) as u8;
            b[3] = v as u8;
        }
        _ => {}
    }
}

/// Original MyDecimal.WriteBin. The caller owns shape checks and buffer size;
/// preserve its debug assertion, partial writes and final `bin[0]` access.
pub fn write_bin(
    negative: bool,
    source_digits_int: i32,
    source_digits_frac: i32,
    words: &[i32; 9],
    precision: i32,
    frac: i32,
    bin: &mut [u8],
) -> Result<Option<DecimalCodecWarning>, DecimalCodecError> {
    let mut warning: Option<DecimalCodecWarning> = None;
    let mut mask: i32 = if negative { -1 } else { 0 };
    let mut digits_int: i32 = precision - frac;
    let words_int = (digits_int / DIGITS_PER_WORD as i32) as usize;
    let leading_digits = (digits_int - words_int as i32 * DIGITS_PER_WORD as i32) as usize;
    let words_frac = (frac / DIGITS_PER_WORD as i32) as usize;
    let trailing_digits = (frac - words_frac as i32 * DIGITS_PER_WORD as i32) as usize;
    let words_frac_from0 = (source_digits_frac / DIGITS_PER_WORD as i32) as usize;
    let trailing_digits_from0 =
        (source_digits_frac - words_frac_from0 as i32 * DIGITS_PER_WORD as i32) as usize;
    let mut int_size = words_int * WORD_SIZE + DIG2BYTES[leading_digits];
    let mut frac_size = words_frac * WORD_SIZE + DIG2BYTES[trailing_digits];
    let frac_size_from = words_frac_from0 * WORD_SIZE + DIG2BYTES[trailing_digits_from0];
    let origin_int_size = int_size;
    let origin_frac_size = frac_size;
    debug_assert_eq!(bin.len(), int_size + frac_size);
    let mut bin_idx = 0usize;
    let (word_idx_from0, digits_int_from) = remove_leading_zeros(source_digits_int, words);
    let mut word_idx_from: i64 = word_idx_from0 as i64;
    if digits_int_from + frac_size_from as i32 == 0 {
        mask = 0;
        digits_int = 1;
    }
    let mut words_int_from: i64 = (digits_int_from / DIGITS_PER_WORD as i32) as i64;
    let mut leading_digits_from =
        (digits_int_from - words_int_from as i32 * DIGITS_PER_WORD as i32) as usize;
    let i_size_from = words_int_from as usize * WORD_SIZE + DIG2BYTES[leading_digits_from];
    let mut words_frac_from = words_frac_from0;
    let mut trailing_digits_from = trailing_digits_from0;
    if digits_int < digits_int_from {
        word_idx_from += words_int_from - words_int as i64;
        if leading_digits_from > 0 {
            word_idx_from += 1;
        }
        if leading_digits > 0 {
            word_idx_from -= 1;
        }
        words_int_from = words_int as i64;
        leading_digits_from = leading_digits;
        warning = Some(DecimalCodecWarning::Overflow);
    } else if int_size > i_size_from {
        while int_size > i_size_from {
            int_size -= 1;
            bin[bin_idx] = mask as u8;
            bin_idx += 1;
        }
    }
    if frac_size < frac_size_from
        || (frac_size == frac_size_from
            && (trailing_digits <= trailing_digits_from || words_frac <= words_frac_from))
    {
        if frac_size < frac_size_from
            || (frac_size == frac_size_from && trailing_digits < trailing_digits_from)
            || (frac_size == frac_size_from && words_frac < words_frac_from)
        {
            warning = Some(DecimalCodecWarning::Truncated);
        }
        words_frac_from = words_frac;
        trailing_digits_from = trailing_digits;
    } else if frac_size > frac_size_from && trailing_digits_from > 0 {
        if words_frac == words_frac_from {
            trailing_digits_from = trailing_digits;
            frac_size = frac_size_from;
        } else {
            words_frac_from += 1;
            trailing_digits_from = 0;
        }
    }
    if leading_digits_from > 0 {
        let i = DIG2BYTES[leading_digits_from];
        let x = (words[word_idx_from as usize] % POWERS10[leading_digits_from]) ^ mask;
        word_idx_from += 1;
        write_word(&mut bin[bin_idx..], x, i);
        bin_idx += i;
    }
    let stop = word_idx_from + words_int_from + words_frac_from as i64;
    while word_idx_from < stop {
        let x = words[word_idx_from as usize] ^ mask;
        word_idx_from += 1;
        write_word(&mut bin[bin_idx..], x, WORD_SIZE);
        bin_idx += WORD_SIZE;
    }
    if trailing_digits_from > 0 {
        let i = DIG2BYTES[trailing_digits_from];
        let mut lim = trailing_digits;
        if words_frac_from < words_frac {
            lim = DIGITS_PER_WORD;
        }
        let mut tdf = trailing_digits_from;
        while tdf < lim && DIG2BYTES[tdf] == i {
            tdf += 1;
        }
        let x = (words[word_idx_from as usize] / POWERS10[DIGITS_PER_WORD - tdf]) ^ mask;
        write_word(&mut bin[bin_idx..], x, i);
        bin_idx += i;
    }
    if frac_size > frac_size_from {
        let bin_idx_end = origin_int_size + origin_frac_size;
        while frac_size > frac_size_from && bin_idx < bin_idx_end {
            frac_size -= 1;
            bin[bin_idx] = mask as u8;
            bin_idx += 1;
        }
    }
    bin[0] ^= 0x80;
    Ok(warning)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_binary_write_keeps_shape_status_bytes_and_caller_contract() {
        let mut words = [0; 9];
        words[0] = 123;
        words[1] = 456_000_000;
        words[8] = i32::MIN;
        // 123.456: partial integer/fraction groups are big-endian; an inactive
        // signed word is untouched rather than rejected by a logical import.
        let mut out = [0; 4];
        assert_eq!(write_bin(false, 3, 3, &words, 6, 3, &mut out), Ok(None));
        assert_eq!(out, [0x80, 0x7b, 0x01, 0xc8]);
        assert_eq!(write_bin(true, 3, 3, &words, 6, 3, &mut out), Ok(None));
        assert_eq!(out, [0x7f, 0x84, 0xfe, 0x37]);
        let mut short = [0; 2];
        assert_eq!(
            write_bin(false, 3, 3, &words, 3, 2, &mut short),
            Ok(Some(DecimalCodecWarning::Truncated))
        );
        assert_eq!(short, [0x83, 0x2d]); // fraction loss overwrites integer overflow
        let mut overflow = [0; 3];
        assert_eq!(
            write_bin(false, 3, 3, &words, 5, 3, &mut overflow),
            Ok(Some(DecimalCodecWarning::Overflow))
        );
        assert_eq!(overflow, [0x97, 0x01, 0xc8]);
        assert_eq!(checked_bin_size(6, 3), Ok(4));
        assert_eq!(checked_bin_size(82, 0), Err(DecimalCodecError::BadNumber));
        assert_eq!(checked_bin_size(1, 31), Err(DecimalCodecError::BadNumber));
        assert_eq!(checked_bin_size(0, 0), Ok(0));
        assert_eq!(checked_bin_size(0, 9), Ok(0));
        assert_eq!(checked_bin_size(1, 10), Ok(1));
        assert_eq!(decimal_bin_size(9, -9), Ok(4));
        assert_eq!(checked_bin_size(9, -9), Err(DecimalCodecError::BadNumber));
        let mut zero = [0xff];
        assert_eq!(write_bin(true, 0, 0, &[0; 9], 1, 0, &mut zero), Ok(None));
        assert_eq!(zero, [0x80]);
        assert_eq!(
            remove_leading_zeros(12, &[0, 123, 0, 0, 0, 0, 0, 0, 0]),
            (1, 3)
        );
        assert!(
            std::panic::catch_unwind(|| write_bin(false, 0, 0, &[0; 9], 0, 0, &mut [])).is_err()
        );
    }
}
