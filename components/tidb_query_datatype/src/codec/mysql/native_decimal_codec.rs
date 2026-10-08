// Copyright 2026 PingCAP, Inc. Licensed under Apache-2.0.

//! Native MyDecimal's fixed binary codec leaves. These preserve the original
//! signed word/count domain, warning precedence and caller-sized-buffer
//! contract; they do not import or normalize a Decimal.

use smallvec::SmallVec;

const DIGITS_PER_WORD: usize = 9;
const WORD_BUF_LEN: usize = 9;
const WORD_SIZE: usize = 4;
const WORD_MAX: i32 = 999_999_999;
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

/// Native word representation produced by [`decode_bin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeDecimalDecoded {
    pub negative: bool,
    pub digits_int: i32,
    pub digits_frac: i32,
    pub words: [i32; 9],
    pub consumed: usize,
    pub warning: Option<DecimalCodecWarning>,
}

/// Hard decode failure with the fixed payload cursor disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeDecimalDecodeFailure {
    pub consumed: usize,
    pub error: DecimalCodecError,
}

/// Decimal sign, coefficient digits, and scale reconstructed from native words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeDecimalParts {
    pub negative: bool,
    pub coefficient: SmallVec<[u8; 24]>,
    pub scale: u32,
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

/// Reconstruct coefficient digits from the native base-1e9 word view.
pub fn decimal_words_to_parts(
    negative: bool,
    digits_int: i32,
    digits_frac: i32,
    words: &[i32; 9],
) -> NativeDecimalParts {
    let (word_start_idx, digits_int) = remove_leading_zeros(digits_int, words);
    let int_len = digits_int.max(0) as usize;
    let fraction_len = digits_frac.max(0) as usize;
    let mut coefficient = SmallVec::<[u8; 24]>::new();
    coefficient.resize(int_len + fraction_len, b'0');

    if digits_int > 0 {
        let mut pos = int_len;
        let mut word_idx = word_start_idx + (digits_int as usize).div_ceil(DIGITS_PER_WORD);
        let mut remaining = digits_int;
        while remaining > 0 {
            word_idx -= 1;
            let mut word = words[word_idx];
            let take = remaining.min(DIGITS_PER_WORD as i32);
            for _ in 0..take {
                let next = word / 10;
                pos -= 1;
                coefficient[pos] = b'0' + (word - next * 10) as u8;
                word = next;
            }
            remaining -= DIGITS_PER_WORD as i32;
        }
    }

    if digits_frac > 0 {
        let digit_mask = POWERS10[DIGITS_PER_WORD - 1];
        let mut word_idx = word_start_idx + (digits_int.max(0) as usize).div_ceil(DIGITS_PER_WORD);
        let mut remaining = digits_frac;
        let mut offset = int_len;
        while remaining > 0 {
            let mut word = words[word_idx];
            word_idx += 1;
            let take = remaining.min(DIGITS_PER_WORD as i32);
            for _ in 0..take {
                let next = word / digit_mask;
                coefficient[offset] = b'0' + next as u8;
                offset += 1;
                word -= next * digit_mask;
                word *= 10;
            }
            remaining -= DIGITS_PER_WORD as i32;
        }
    }

    if coefficient.is_empty() {
        coefficient.push(b'0');
    }
    NativeDecimalParts {
        negative,
        coefficient,
        scale: digits_frac.max(0) as u32,
    }
}

fn fix_word_cnt_error(
    words_int: usize,
    words_frac: usize,
) -> (usize, usize, Option<DecimalCodecWarning>) {
    if words_int + words_frac > WORD_BUF_LEN {
        if words_int > WORD_BUF_LEN {
            return (WORD_BUF_LEN, 0, Some(DecimalCodecWarning::Overflow));
        }
        return (
            words_int,
            WORD_BUF_LEN - words_int,
            Some(DecimalCodecWarning::Truncated),
        );
    }
    (words_int, words_frac, None)
}

/// Sign-extending big-endian load of a `size`-byte word.
fn read_word(b: &[u8], size: usize) -> i32 {
    match size {
        1 => i32::from(b[0] as i8),
        2 => (i32::from(b[0] as i8) << 8) + i32::from(b[1]),
        3 => {
            if b[0] & 128 > 0 {
                (0xFF00_0000u32
                    | (u32::from(b[0]) << 16)
                    | (u32::from(b[1]) << 8)
                    | u32::from(b[2])) as i32
            } else {
                ((u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2])) as i32
            }
        }
        4 => {
            i32::from(b[3])
                + (i32::from(b[2]) << 8)
                + (i32::from(b[1]) << 16)
                + (i32::from(b[0] as i8) << 24)
        }
        _ => 0,
    }
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

/// Decode a fixed-length MyDecimal binary payload into its native word view.
///
/// The returned value preserves the source sign and word/count representation;
/// it does not construct or normalize a Decimal. Empty input and invalid shapes
/// fail without consuming bytes. Corrupt words consume the legal fixed payload
/// size, matching MyDecimal's cursor disposition.
pub fn decode_bin(
    bin: &[u8],
    precision: i32,
    frac: i32,
) -> Result<NativeDecimalDecoded, NativeDecimalDecodeFailure> {
    let zero_failure = || NativeDecimalDecodeFailure {
        consumed: 0,
        error: DecimalCodecError::BadNumber,
    };
    if bin.is_empty() {
        return Err(zero_failure());
    }
    let digits_int = precision - frac;
    let words_int = digits_int / DIGITS_PER_WORD as i32;
    let leading_digits = digits_int - words_int * DIGITS_PER_WORD as i32;
    let mut words_frac = frac / DIGITS_PER_WORD as i32;
    let mut trailing_digits = frac - words_frac * DIGITS_PER_WORD as i32;
    let mut words_int_to = words_int;
    if leading_digits > 0 {
        words_int_to += 1;
    }
    let mut words_frac_to = words_frac;
    if trailing_digits > 0 {
        words_frac_to += 1;
    }

    let mask: i32 = if bin[0] & 0x80 > 0 { 0 } else { -1 };
    let bin_size =
        decimal_bin_size(precision, frac).map_err(|error| NativeDecimalDecodeFailure {
            error,
            ..zero_failure()
        })?;
    if bin_size > 40 {
        return Err(zero_failure());
    }

    let mut buf = [0u8; 40];
    let n = bin.len().min(bin_size);
    buf[..n].copy_from_slice(&bin[..n]);
    buf[0] ^= 0x80;

    let mut bin_idx = 0usize;
    let mut warning = None;
    let old_words_int_to = words_int_to;
    let (fixed_int, fixed_frac, warn) =
        fix_word_cnt_error(words_int_to as usize, words_frac_to as usize);
    words_int_to = fixed_int as i32;
    words_frac_to = fixed_frac as i32;
    if warn.is_some() {
        warning = warn;
        if words_int_to < old_words_int_to {
            bin_idx += DIG2BYTES[leading_digits as usize]
                + (words_int - words_int_to) as usize * WORD_SIZE;
        } else {
            trailing_digits = 0;
            words_frac = words_frac_to;
        }
    }

    let mut decoded = NativeDecimalDecoded {
        negative: mask != 0,
        digits_int: words_int * DIGITS_PER_WORD as i32 + leading_digits,
        digits_frac: words_frac * DIGITS_PER_WORD as i32 + trailing_digits,
        words: [0; WORD_BUF_LEN],
        consumed: bin_size,
        warning,
    };

    let mut word_idx = 0usize;
    if leading_digits > 0 {
        let i = DIG2BYTES[leading_digits as usize];
        let x = read_word(&buf[bin_idx..], i);
        bin_idx += i;
        decoded.words[word_idx] = x ^ mask;
        if u64::from(decoded.words[word_idx] as u32)
            >= u64::from(POWERS10[leading_digits as usize + 1] as u32)
        {
            return Err(NativeDecimalDecodeFailure {
                consumed: bin_size,
                error: DecimalCodecError::BadNumber,
            });
        }
        if word_idx > 0 || decoded.words[word_idx] != 0 {
            word_idx += 1;
        } else {
            decoded.digits_int -= leading_digits;
        }
    }

    let stop = bin_idx + words_int as usize * WORD_SIZE;
    while bin_idx < stop {
        decoded.words[word_idx] = read_word(&buf[bin_idx..], WORD_SIZE) ^ mask;
        if decoded.words[word_idx] as u32 > WORD_MAX as u32 {
            return Err(NativeDecimalDecodeFailure {
                consumed: bin_size,
                error: DecimalCodecError::BadNumber,
            });
        }
        if word_idx > 0 || decoded.words[word_idx] != 0 {
            word_idx += 1;
        } else {
            decoded.digits_int -= DIGITS_PER_WORD as i32;
        }
        bin_idx += WORD_SIZE;
    }

    let stop = bin_idx + words_frac as usize * WORD_SIZE;
    while bin_idx < stop {
        decoded.words[word_idx] = read_word(&buf[bin_idx..], WORD_SIZE) ^ mask;
        if decoded.words[word_idx] as u32 > WORD_MAX as u32 {
            return Err(NativeDecimalDecodeFailure {
                consumed: bin_size,
                error: DecimalCodecError::BadNumber,
            });
        }
        word_idx += 1;
        bin_idx += WORD_SIZE;
    }

    if trailing_digits > 0 {
        let i = DIG2BYTES[trailing_digits as usize];
        let x = read_word(&buf[bin_idx..], i);
        decoded.words[word_idx] = (x ^ mask) * POWERS10[DIGITS_PER_WORD - trailing_digits as usize];
        if decoded.words[word_idx] as u32 > WORD_MAX as u32 {
            return Err(NativeDecimalDecodeFailure {
                consumed: bin_size,
                error: DecimalCodecError::BadNumber,
            });
        }
    }

    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_words_to_parts_preserves_coefficient_shape() {
        let mut fraction = [0; 9];
        fraction[0] = 456_000_000;
        assert_eq!(
            decimal_words_to_parts(false, 0, 3, &fraction),
            NativeDecimalParts {
                negative: false,
                coefficient: SmallVec::from_slice(b"456"),
                scale: 3,
            }
        );

        assert_eq!(
            decimal_words_to_parts(true, 0, 0, &[0; 9]),
            NativeDecimalParts {
                negative: true,
                coefficient: SmallVec::from_slice(b"0"),
                scale: 0,
            }
        );

        let mut leading_zero_word = [0; 9];
        leading_zero_word[1] = 123;
        assert_eq!(
            decimal_words_to_parts(false, 12, 0, &leading_zero_word),
            NativeDecimalParts {
                negative: false,
                coefficient: SmallVec::from_slice(b"123"),
                scale: 0,
            }
        );
    }

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

    #[test]
    fn native_binary_decode_keeps_words_failures_and_negative_zero() {
        let decoded = decode_bin(&[0x80, 0x7b, 0x01, 0xc8], 6, 3).expect("decode 123.456");
        assert_eq!(
            decoded,
            NativeDecimalDecoded {
                negative: false,
                digits_int: 3,
                digits_frac: 3,
                words: [123, 456_000_000, 0, 0, 0, 0, 0, 0, 0],
                consumed: 4,
                warning: None,
            }
        );

        let corrupt = decode_bin(&[0x80, 0x3b, 0x9a, 0xca, 0x00], 10, 0)
            .expect_err("a word above 999999999 must fail");
        assert_eq!(
            corrupt,
            NativeDecimalDecodeFailure {
                consumed: 5,
                error: DecimalCodecError::BadNumber,
            }
        );
        assert_eq!(
            decode_bin(&[], 6, 3),
            Err(NativeDecimalDecodeFailure {
                consumed: 0,
                error: DecimalCodecError::BadNumber,
            })
        );
        assert_eq!(
            decode_bin(&[0x80], -1, 1),
            Err(NativeDecimalDecodeFailure {
                consumed: 0,
                error: DecimalCodecError::BadNumber,
            })
        );
        assert_eq!(
            decode_bin(&[0x80], 100, 0),
            Err(NativeDecimalDecodeFailure {
                consumed: 0,
                error: DecimalCodecError::BadNumber,
            })
        );

        let negative_zero = decode_bin(&[0x7f, 0x00], 2, 2).expect("decode negative zero");
        assert!(negative_zero.negative);
        assert_eq!(negative_zero.digits_int, 0);
        assert_eq!(negative_zero.digits_frac, 2);
        assert_eq!(negative_zero.words, [0; 9]);
        assert_eq!(negative_zero.consumed, 1);
        assert_eq!(negative_zero.warning, None);

        let clamped = decode_bin(&[0x80], 82, 1).expect("decode clamped shape");
        assert_eq!(clamped.consumed, 37);
        assert_eq!(clamped.warning, Some(DecimalCodecWarning::Truncated));
        assert_eq!(clamped.digits_frac, 0);
    }
}
