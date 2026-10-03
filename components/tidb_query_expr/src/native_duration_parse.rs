// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native expression duration parsing over the shared datetime value parser.
//! This preserves the microsecond/error policy, not the wire nanosecond parser.

use std::sync::OnceLock;

use tidb_query_datatype::codec::mysql::Time;

use crate::native_time_parse::{NativeDurationDateTime, parse_native_duration_datetime};

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

    /// Native Duration.Add/Sub: scale first, preserve the zero-value operand
    /// special case, then saturate the sum and retain the larger precision.
    pub fn combine(self, other: Self, sign: i64) -> Self {
        let scaled = Self {
            micros: other.micros * sign,
            fsp: other.fsp,
        };
        if other.micros == 0 && other.fsp == 0 {
            return self;
        }
        Self {
            micros: self.micros.saturating_add(scaled.micros),
            fsp: self.fsp.max(other.fsp),
        }
    }

    /// Native Duration.String over the shared microsecond formatter.
    pub fn format(self) -> String {
        crate::native_time_diff::native_format_time_diff(self.micros, self.fsp.max(0) as usize)
    }

    /// Original expression duration-shape predicate, before either value
    /// parser.
    pub fn is_duration(value: &str) -> bool {
        static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
        PATTERN
            .get_or_init(|| {
                regex::Regex::new(
                    r"^\s*[-]?(((\d{1,2}\s+)?0*\d{0,3}(:0*\d{1,2}){0,2})|(\d{1,7}))?(\.\d*)?\s*$",
                )
                .expect("the source durationPattern is a valid regex")
            })
            .is_match(value)
    }

    /// ADDTIME/SUBTIME use maximum precision when the written fraction contains
    /// any nonzero character; this is distinct from native_duration_fsp.
    pub fn fsp_for_time_add_sub(value: &str) -> i32 {
        match value.find('.') {
            None => MIN_FSP,
            Some(dot) => {
                if value[dot + 1..].chars().any(|c| c != '0') {
                    MAX_FSP
                } else {
                    MIN_FSP
                }
            }
        }
    }
}

impl NativeDurationDateTime {
    /// Native Time.IsZero ignores the precision metadata.
    pub fn is_zero(self) -> bool {
        self.year == 0
            && self.month == 0
            && self.day == 0
            && self.hour == 0
            && self.minute == 0
            && self.second == 0
            && self.micros == 0
    }

    /// Native Time.String truncates the raw microsecond field, without calendar
    /// validation, field narrowing or rounding to the requested precision.
    pub fn format(self) -> String {
        let stem = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        );
        let fsp = self.fsp.clamp(0, MAX_FSP);
        if fsp == 0 {
            return stem;
        }
        let divisor = 10u32.pow(6 - fsp as u32);
        format!(
            "{stem}.{:0width$}",
            self.micros / divisor,
            width = fsp as usize
        )
    }

    /// Native Time.Add keeps its checked total, signed absolute-value policy,
    /// zero-date inverse result and maximum precision. Validation is separate.
    pub fn add(self, delta: NativeGoDuration) -> Option<Self> {
        let total = Self::daynr(self.year, self.month, self.day)
            .checked_mul(86_400_000_000)?
            .checked_add(
                i64::from(self.hour) * 3_600_000_000
                    + i64::from(self.minute) * 60_000_000
                    + i64::from(self.second) * 1_000_000
                    + i64::from(self.micros),
            )?
            .checked_add(delta.micros)?;
        let total = total.abs();
        let seconds = total / 1_000_000;
        let micros = (total % 1_000_000) as u32;
        let (year, month, day) = Self::date_from_daynr(seconds / 86_400);
        let rest = seconds % 86_400;
        Some(Self {
            year,
            month,
            day,
            hour: (rest / 3600) as u32,
            minute: (rest / 60 % 60) as u32,
            second: (rest % 60) as u32,
            micros,
            fsp: self.fsp.max(delta.fsp),
        })
    }

    /// The family's original range predicate, not full civil-date validation.
    pub fn in_range(self) -> bool {
        (1..=9999).contains(&self.year) && self.month >= 1 && self.day >= 1
    }

    /// Wide native day-number arithmetic, sharing the existing original-width
    /// core.
    pub fn daynr(year: i64, month: u32, day: u32) -> i64 {
        Time::native_time_diff_daynr(year, month, day)
    }

    /// Original inverse day-number algorithm; outside the admitted interval it
    /// returns zero fields, rather than constructing or rejecting a civil date.
    pub fn date_from_daynr(daynr: i64) -> (i64, u32, u32) {
        if daynr <= 365 || daynr >= 3_652_500 {
            return (0, 0, 0);
        }
        let mut year = daynr * 100 / 36525;
        let temp = (((year - 1) / 100 + 1) * 3) / 4;
        let mut day_of_year = daynr - year * 365 - (year - 1) / 4 + temp;
        let mut in_year = days_in_year(year);
        while day_of_year > in_year {
            day_of_year -= in_year;
            year += 1;
            in_year = days_in_year(year);
        }
        let mut leap_day = 0;
        if in_year == 366 && day_of_year > 31 + 28 {
            day_of_year -= 1;
            if day_of_year == 31 + 28 {
                leap_day = 1;
            }
        }
        let mut month = 1;
        for length in [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31] {
            if day_of_year <= length {
                break;
            }
            day_of_year -= length;
            month += 1;
        }
        (year, month, (day_of_year + leap_day) as u32)
    }
}

fn days_in_year(year: i64) -> i64 {
    // Only the inverse's admitted day numbers reach this helper: its initial
    // year is positive and at most 9999, and the loop can advance to 10000.
    let year = i32::try_from(year).expect("bounded inverse day number year");
    i64::from(Time::native_calc_days_in_year_i32(year))
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
    fn duration_value_methods_keep_zero_precision_raw_fields_and_day_boundaries() {
        let duration = NativeGoDuration {
            micros: 42,
            fsp: -1,
        };
        assert_eq!(
            duration.combine(NativeGoDuration { micros: 0, fsp: 0 }, -1),
            duration
        );
        assert_eq!(
            NativeGoDuration {
                micros: i64::MAX,
                fsp: 2
            }
            .combine(NativeGoDuration { micros: 1, fsp: 6 }, 1),
            NativeGoDuration {
                micros: i64::MAX,
                fsp: 6
            }
        );
        assert_eq!(
            NativeGoDuration {
                micros: -123_456,
                fsp: 3
            }
            .format(),
            "-00:00:00.123"
        );
        let zero = NativeDurationDateTime {
            year: 0,
            month: 0,
            day: 0,
            hour: 0,
            minute: 0,
            second: 0,
            micros: 0,
            fsp: 3,
        };
        assert!(zero.is_zero());
        assert!(!zero.in_range());
        let reflected = zero.add(NativeGoDuration { micros: -1, fsp: 6 }).unwrap();
        assert_eq!(
            reflected,
            NativeDurationDateTime {
                micros: 1,
                fsp: 6,
                ..zero
            }
        );
        let first_day = NativeDurationDateTime {
            year: 1,
            month: 1,
            day: 1,
            ..zero
        };
        assert!(
            first_day
                .add(NativeGoDuration {
                    micros: i64::MAX,
                    fsp: 0
                })
                .is_none()
        );
        assert_eq!(NativeDurationDateTime::daynr(0, 0, u32::MAX), 0);
        assert_eq!(NativeDurationDateTime::daynr(1, 1, 1), 366);
        assert_eq!(NativeDurationDateTime::date_from_daynr(366), (1, 1, 1));
        assert_eq!(
            NativeDurationDateTime::date_from_daynr(3_652_499),
            (10_000, 3, 15)
        );
        for day in [i64::MIN, 365, 3_652_500, i64::MAX] {
            assert_eq!(NativeDurationDateTime::date_from_daynr(day), (0, 0, 0));
        }
        let raw = NativeDurationDateTime {
            year: 12_345,
            month: 99,
            day: 88,
            hour: 77,
            minute: 66,
            second: 55,
            micros: 1_234_567,
            fsp: 3,
        };
        assert_eq!(raw.format(), "12345-99-88 77:66:55.1234");
        assert!(!raw.in_range());
        assert!(NativeDurationDateTime { year: 1, ..raw }.in_range());
        assert!(NativeGoDuration::is_duration(""));
        assert!(NativeGoDuration::is_duration("1 01:00:00"));
        assert!(!NativeGoDuration::is_duration("aa:bb:cc"));
        assert!(!NativeGoDuration::is_duration("20171231235959.999999"));
        assert_eq!(NativeGoDuration::fsp_for_time_add_sub("1.000"), 0);
        assert_eq!(NativeGoDuration::fsp_for_time_add_sub("1.000 "), 6);
        #[cfg(debug_assertions)]
        assert!(
            std::panic::catch_unwind(|| {
                duration.combine(
                    NativeGoDuration {
                        micros: i64::MIN,
                        fsp: 0,
                    },
                    -1,
                )
            })
            .is_err()
        );
    }

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
