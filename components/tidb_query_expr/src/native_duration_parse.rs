// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native expression duration parsing over the shared datetime value parser.
//! This preserves the microsecond/error policy, not the wire nanosecond parser.

use crate::native_time_parse::parse_native_duration_datetime;

const MAX_FSP: i32 = 6;
const MIN_FSP: i32 = 0;
const TIME_MAX_HOUR: i64 = 838;
const MAX_TIME_MICROS: i64 = (838 * 3600 + 59 * 60 + 59) * 1_000_000;

/// Native Go duration value, including the precision retained by its caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeGoDuration {
    pub micros: i64,
    pub fsp: i32,
}

impl NativeGoDuration {
    /// Fractional microseconds alone, with the original signed-abs behavior.
    pub fn micro_second(self) -> i64 {
        self.micros.abs() % 1_000_000
    }
}

/// The native expression parser's single truncation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeDurationTruncated;

/// Written fractional precision: byte count after the first dot, capped at six.
pub fn native_duration_fsp(value: &str) -> i32 {
    match value.find('.') {
        None => MIN_FSP,
        Some(dot) => (value.len() - dot - 1).min(MAX_FSP as usize) as i32,
    }
}

fn space0(value: &str) -> &str {
    value.trim_start_matches(|c: char| c.is_ascii_whitespace())
}

/// Go `parser.Number`: at least one leading decimal digit.
fn number(value: &str) -> Option<(i64, &str)> {
    let digits: &str = value
        .split_at(
            value
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(value.len()),
        )
        .0;
    if digits.is_empty() {
        return None;
    }
    Some((digits.parse::<i64>().ok()?, &value[digits.len()..]))
}

/// Go `matchColon`: optional spaces, a colon, optional spaces.
fn match_colon(value: &str) -> Option<&str> {
    Some(space0(space0(value).strip_prefix(':')?))
}

/// Go `matchHHMMSSDelimited`.
fn match_hhmmss_delimited(value: &str, require_colon: bool) -> Option<([i64; 3], &str)> {
    let (hour, mut rest) = number(value)?;
    let mut hhmmss = [hour, 0, 0];
    for (index, slot) in hhmmss.iter_mut().enumerate().skip(1) {
        let Some(after_colon) = match_colon(rest) else {
            if index == 1 && require_colon {
                return None;
            }
            break;
        };
        let (num, remain) = number(after_colon)?;
        *slot = num;
        rest = remain;
    }
    Some((hhmmss, rest))
}

/// Go `matchDayHHMMSS`: `D HH:MM:SS`, the day folded into the hours.
fn match_day_hhmmss(value: &str) -> Option<([i64; 3], &str)> {
    let (day, rest) = number(value)?;
    let after_space = space0(rest);
    if after_space.len() == rest.len() {
        return None;
    }
    let (mut hhmmss, rest) = match_hhmmss_delimited(after_space, false)?;
    hhmmss[0] += 24 * day;
    Some((hhmmss, rest))
}

/// Go `matchHHMMSSCompact`: one run of digits read right-aligned as `HHMMSS`.
fn match_hhmmss_compact(value: &str) -> Option<([i64; 3], &str)> {
    let (num, rest) = number(value)?;
    Some(([num / 10000, num / 100 % 100, num % 100], rest))
}

/// Go `types.ParseFrac`, returning `(microseconds, overflow)`.
fn parse_frac(digits: &str, fsp: i32) -> Result<(i64, bool), NativeDurationTruncated> {
    if digits.is_empty() {
        return Ok((0, false));
    }
    tidb_query_datatype::codec::mysql::Time::native_parse_fraction(
        digits.as_bytes(),
        i64::from(fsp.clamp(MIN_FSP, MAX_FSP)),
    )
    .map_err(|_| NativeDurationTruncated)
}

/// Go `matchFrac`, returning `(overflow, microseconds, rest)`.
fn match_frac(value: &str, fsp: i32) -> Result<(bool, i64, &str), NativeDurationTruncated> {
    let Some(after_dot) = value.strip_prefix('.') else {
        return Ok((false, 0, value));
    };
    let end = after_dot
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_dot.len());
    let (frac, overflow) = parse_frac(&after_dot[..end], fsp)?;
    Ok((overflow, frac, &after_dot[end..]))
}

/// Go `hhmmssAddOverflow`: carry one second into `HH:MM:SS`.
fn hhmmss_add_overflow(hms: &mut [i64; 3]) {
    let modulus = [-1, 60, 60];
    let mut overflow = true;
    for index in (0..3).rev() {
        if !overflow {
            break;
        }
        hms[index] += 1;
        if hms[index] == modulus[index] {
            hms[index] = 0;
        } else {
            overflow = false;
        }
    }
}

/// Go `matchDuration`.
fn match_duration(value: &str, fsp: i32) -> Result<NativeGoDuration, NativeDurationTruncated> {
    if value.is_empty() {
        return Err(NativeDurationTruncated);
    }
    let (negative, rest) = value
        .strip_prefix('-')
        .map_or((false, value), |rest| (true, rest));
    let rest = space0(rest);
    let chars_len = rest.len();
    let (mut hhmmss, rest) = match_day_hhmmss(rest)
        .or_else(|| match_hhmmss_delimited(rest, true))
        .or_else(|| match_hhmmss_compact(rest))
        .ok_or(NativeDurationTruncated)?;
    let rest = space0(rest);
    let (overflow, mut frac, rest) = match_frac(rest, fsp)?;
    if !rest.is_empty() && chars_len >= 12 {
        return Err(NativeDurationTruncated);
    }
    if overflow {
        hhmmss_add_overflow(&mut hhmmss);
        frac = 0;
    }
    if hhmmss[1] >= 60 || hhmmss[2] >= 60 {
        return Err(NativeDurationTruncated);
    }
    // The native expression callers reject the source's clamped/error result.
    if hhmmss[0] > TIME_MAX_HOUR {
        return Err(NativeDurationTruncated);
    }
    let mut micros = (hhmmss[0] * 3600 + hhmmss[1] * 60 + hhmmss[2]) * 1_000_000 + frac;
    if negative {
        micros = -micros;
    }
    if !(-MAX_TIME_MICROS..=MAX_TIME_MICROS).contains(&micros) || !rest.is_empty() {
        return Err(NativeDurationTruncated);
    }
    Ok(NativeGoDuration { micros, fsp })
}

/// Go `canFallbackToDateTime`.
fn can_fall_back_to_datetime(value: &str) -> bool {
    let Some((_, rest)) = number(value) else {
        return false;
    };
    let digits = value.len() - rest.len();
    if digits == 12 || digits == 14 {
        return true;
    }
    let Some(rest) = strip_punct(rest) else {
        return false;
    };
    let Some((_, rest)) = number(rest) else {
        return false;
    };
    let Some(rest) = strip_punct(rest) else {
        return false;
    };
    let Some((_, rest)) = number(rest) else {
        return false;
    };
    rest.starts_with(' ') || rest.starts_with('T')
}

/// Native `parser.AnyPunct`: retain its original single-byte ASCII test.
fn strip_punct(value: &str) -> Option<&str> {
    let first = *value.as_bytes().first()?;
    if (first as char).is_ascii_punctuation() {
        Some(&value[1..])
    } else {
        None
    }
}

/// Match a duration first, then try the admitted datetime fallback. The input
/// remains actual text through both shared parsers; no host-parsed value is
/// used.
pub fn parse_native_duration(
    value: &str,
    fsp: i32,
) -> Result<NativeGoDuration, NativeDurationTruncated> {
    let rest = value.trim();
    match match_duration(rest, fsp) {
        Ok(duration) => Ok(duration),
        Err(NativeDurationTruncated) => {
            if !can_fall_back_to_datetime(rest) {
                return Err(NativeDurationTruncated);
            }
            let datetime = parse_native_duration_datetime(rest).ok_or(NativeDurationTruncated)?;
            let micros = i64::from(datetime.hour) * 3_600_000_000
                + i64::from(datetime.minute) * 60_000_000
                + i64::from(datetime.second) * 1_000_000
                + i64::from(datetime.micros);
            Ok(native_duration_round_frac(micros, fsp))
        }
    }
}

/// Original microsecond half-up rounding, distinct from nanosecond SDK ties.
pub(crate) fn native_duration_round_frac(micros: i64, fsp: i32) -> NativeGoDuration {
    let fsp = fsp.clamp(MIN_FSP, MAX_FSP);
    let unit = 10i64.pow(MAX_FSP as u32 - fsp as u32);
    let sign = if micros < 0 { -1 } else { 1 };
    let magnitude = micros.abs();
    let rounded = (magnitude + unit / 2) / unit * unit;
    NativeGoDuration {
        micros: sign * rounded,
        fsp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_parser_keeps_rounding_compact_fallback_and_raw_fsp() {
        for (text, fsp, micros, result_fsp) in [
            ("-00:00:00.1234567", 6, -123_457, 6),
            ("00:59:59.9999999", 6, 3_600_000_000, 6),
            ("1 01:02:03.004", 3, 90_123_004_000, 3),
            ("1", -1, 1_000_000, -1),
            ("20170118123050.1234567", 6, 45_050_123_457, 6),
            ("2017-01-18 12:30:50.1234567", 6, 45_050_123_456, 6),
        ] {
            assert_eq!(
                parse_native_duration(text, fsp),
                Ok(NativeGoDuration {
                    micros,
                    fsp: result_fsp
                })
            );
        }
        for text in [
            "1x",
            "839:00:00",
            "838:59:59.000001",
            "2011-11-11 10:10:10.11.12",
        ] {
            assert_eq!(
                parse_native_duration(text, native_duration_fsp(text)),
                Err(NativeDurationTruncated)
            );
        }
        assert_eq!(native_duration_fsp("1.é"), 2);
        assert_eq!(native_duration_round_frac(-500_000, 0).micros, -1_000_000);
        assert_eq!(
            NativeGoDuration {
                micros: -123_457,
                fsp: 6
            }
            .micro_second(),
            123_457
        );
    }
}
