// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Public-native nanosecond duration parser, distinct from the wire parser and
//! the expression microsecond/regex parser. Events are values, not SQL
//! warnings.

use std::{error::Error, fmt};

use chrono::TimeZone;

use super::MAX_NANOS;
use crate::codec::mysql::{
    Time, TimeType,
    time::{NativeFspError, NativeTimeError, native_is_go_punctuation, native_parse_time},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeParsedDuration {
    pub nanos: i64,
    pub fsp: i64,
    pub overflow: Option<NativeDurationOverflow>,
    pub truncated: bool,
}

impl NativeParsedDuration {
    pub const fn nanoseconds(self) -> i64 {
        self.nanos
    }
    pub const fn fsp(self) -> i64 {
        self.fsp
    }
    pub const fn overflow(self) -> Option<NativeDurationOverflow> {
        self.overflow
    }
    pub const fn truncated(self) -> bool {
        self.truncated
    }
    pub const fn event(self) -> Option<NativeDurationParseEvent> {
        match self.overflow {
            Some(direction) => Some(NativeDurationParseEvent::Overflow(direction)),
            None if self.truncated => Some(NativeDurationParseEvent::Truncated),
            None => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDurationOverflow {
    Positive,
    Negative,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDurationDateTimeFallbackKind {
    Compact12,
    Compact14,
    Separated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDurationParseEvent {
    Overflow(NativeDurationOverflow),
    DateTimeFallback(NativeDurationDateTimeFallbackKind),
    Truncated,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeDurationParseError {
    InvalidFsp(NativeFspError),
    DateTimeFallback(NativeDurationDateTimeFallbackKind),
    InvalidFormat,
    NumericOverflow,
    Fraction(NativeFspError),
}

impl NativeDurationParseError {
    pub const fn event(&self) -> Option<NativeDurationParseEvent> {
        match self {
            Self::InvalidFsp(_) => None,
            Self::DateTimeFallback(kind) => Some(NativeDurationParseEvent::DateTimeFallback(*kind)),
            Self::InvalidFormat | Self::NumericOverflow | Self::Fraction(_) => {
                Some(NativeDurationParseEvent::Truncated)
            }
        }
    }
}

impl fmt::Display for NativeDurationParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFsp(error) | Self::Fraction(error) => error.fmt(formatter),
            Self::DateTimeFallback(_) => {
                formatter.write_str("duration literal requires datetime fallback")
            }
            Self::InvalidFormat => formatter.write_str("invalid duration format"),
            Self::NumericOverflow => formatter.write_str("duration component is out of range"),
        }
    }
}

impl Error for NativeDurationParseError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeDurationValueError {
    Duration(NativeDurationParseError),
    Time(NativeTimeError),
}

impl fmt::Display for NativeDurationValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duration(error) => error.fmt(formatter),
            Self::Time(error) => error.fmt(formatter),
        }
    }
}

impl Error for NativeDurationValueError {}

/// The original clamp and its direction, without statement error policy.
pub const fn native_truncate_overflow_mysql_time(
    value: i64,
) -> (i64, Option<NativeDurationOverflow>) {
    if value > MAX_NANOS {
        (MAX_NANOS, Some(NativeDurationOverflow::Positive))
    } else if value < -MAX_NANOS {
        (-MAX_NANOS, Some(NativeDurationOverflow::Negative))
    } else {
        (value, None)
    }
}

/// Byte grammar plus FSP rounding and explicit range/truncation events.
/// Date-shaped input is only a typed fallback signal at this layer.
pub fn native_parse_duration(
    input: &[u8],
    target_fsp: i64,
) -> Result<NativeParsedDuration, NativeDurationParseError> {
    let fsp = Time::native_normalize_fsp(target_fsp).ok_or(
        NativeDurationParseError::InvalidFsp(NativeFspError::InvalidFsp(target_fsp)),
    )?;
    let input = trim_ascii_space(input);
    if input.is_empty() {
        return Err(NativeDurationParseError::InvalidFormat);
    }
    if let Some(kind) = native_classify_duration_datetime_fallback(input) {
        return Err(NativeDurationParseError::DateTimeFallback(kind));
    }
    let mut index = 0;
    let negative = input.first() == Some(&b'-');
    if negative {
        index += 1;
        skip_ascii_space(input, &mut index);
    }
    // The long-leftover verdict counts bytes AFTER sign and its padding.
    let chars_len = input.len() - index;
    let first = parse_duration_number(input, &mut index)?;
    let before_space = index;
    skip_ascii_space(input, &mut index);
    let day_form =
        index != before_space && input.get(index).is_some_and(|byte| byte.is_ascii_digit());
    let (mut hours, mut minutes, mut seconds) = if day_form {
        let hour = parse_duration_number(input, &mut index)?;
        let hours = first
            .checked_mul(24)
            .and_then(|value| value.checked_add(hour))
            .ok_or(NativeDurationParseError::NumericOverflow)?;
        (hours, 0, 0)
    } else {
        (first, 0, 0)
    };
    if consume_duration_colon(input, &mut index) {
        minutes = parse_duration_number(input, &mut index)?;
        if consume_duration_colon(input, &mut index) {
            seconds = parse_duration_number(input, &mut index)?;
        }
    } else if !day_form {
        hours = first / 10_000;
        minutes = (first / 100) % 100;
        seconds = first % 100;
    }
    // Space0 precedes the optional fraction, never follows it.
    skip_ascii_space(input, &mut index);
    let mut microseconds = 0_i64;
    if input.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while input.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        let (fraction, overflow) = Time::native_parse_fraction(&input[start..index], fsp)
            .map_err(NativeDurationParseError::Fraction)?;
        microseconds = fraction;
        if overflow {
            seconds = seconds
                .checked_add(1)
                .ok_or(NativeDurationParseError::NumericOverflow)?;
            if seconds == 60 {
                seconds = 0;
                minutes = minutes
                    .checked_add(1)
                    .ok_or(NativeDurationParseError::NumericOverflow)?;
                if minutes == 60 {
                    minutes = 0;
                    hours = hours
                        .checked_add(1)
                        .ok_or(NativeDurationParseError::NumericOverflow)?;
                }
            }
        }
    }
    // Leftover classification deliberately precedes the field-range check.
    let leftover = index != input.len();
    if leftover && chars_len >= 12 {
        return Err(NativeDurationParseError::InvalidFormat);
    }
    if minutes > 59 || seconds > 59 {
        return Err(NativeDurationParseError::InvalidFormat);
    }
    let mut parsed =
        parsed_duration_from_parts(negative, hours, minutes, seconds, microseconds, fsp);
    parsed.truncated |= leftover;
    Ok(parsed)
}

/// Full native duration policy, using the existing native datetime parser only
/// after its exact byte-level fallback signal. A successful datetime fallback
/// deliberately drops the datetime parser's truncated/dst-adjusted flags.
pub fn native_parse_mysql_duration<TZ: TimeZone>(
    input: &str,
    target_fsp: i64,
    timezone: &TZ,
    allow_zero_in_date: bool,
    allow_invalid_date: bool,
) -> Result<NativeParsedDuration, NativeDurationValueError> {
    match native_parse_duration(input.as_bytes(), target_fsp) {
        Ok(parsed) => Ok(parsed),
        Err(NativeDurationParseError::DateTimeFallback(_)) => {
            let time = native_parse_time(
                input,
                TimeType::DateTime,
                target_fsp,
                false,
                allow_zero_in_date,
                allow_invalid_date,
                true,
                timezone,
            )
            .map_err(NativeDurationValueError::Time)?
            .time;
            let (nanos, fsp) = native_duration_from_time(time.raw, i64::from(time.fsp))
                .map_err(NativeDurationValueError::Time)?;
            Ok(NativeParsedDuration {
                nanos,
                fsp,
                overflow: None,
                truncated: false,
            })
        }
        Err(error) => Err(NativeDurationValueError::Duration(error)),
    }
}

/// Original Time.ToDuration: exact raw zero ignores input FSP; all other raw
/// values normalize FSP before computing from clock fields. No range gate or
/// fractional rounding is introduced, including for malformed raw fields.
pub fn native_duration_from_time(raw: u64, fsp: i64) -> Result<(i64, i64), NativeTimeError> {
    if raw == 0 {
        return Ok((0, 0));
    }
    let fsp = Time::native_normalize_fsp(fsp)
        .ok_or(NativeTimeError::InvalidFsp(NativeFspError::InvalidFsp(fsp)))?;
    let [_, _, _, hour, minute, second, microsecond] = Time::native_core_fields(raw);
    let nanos = (i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second))
        * 1_000_000_000
        + i64::from(microsecond) * 1_000;
    Ok((nanos, fsp))
}

/// Input is already outer-ASCII-trimmed at the parser call site; this public
/// classifier itself must not trim, skip signs, or decode UTF-8 punctuation.
pub fn native_classify_duration_datetime_fallback(
    input: &[u8],
) -> Option<NativeDurationDateTimeFallbackKind> {
    let first_len = source_digit_prefix(input);
    if first_len == 0 {
        return None;
    }
    match first_len {
        12 => return Some(NativeDurationDateTimeFallbackKind::Compact12),
        14 => return Some(NativeDurationDateTimeFallbackKind::Compact14),
        _ => {}
    }
    let mut index = first_len;
    if !consume_source_punctuation(input, &mut index) {
        return None;
    }
    let second_len = source_digit_prefix(&input[index..]);
    if second_len == 0 {
        return None;
    }
    index += second_len;
    if !consume_source_punctuation(input, &mut index) {
        return None;
    }
    let third_len = source_digit_prefix(&input[index..]);
    if third_len == 0 {
        return None;
    }
    index += third_len;
    match input.get(index) {
        Some(b' ' | b'T') => Some(NativeDurationDateTimeFallbackKind::Separated),
        _ => None,
    }
}

pub fn native_can_fallback_to_datetime(input: &[u8]) -> bool {
    native_classify_duration_datetime_fallback(input).is_some()
}

fn source_digit_prefix(input: &[u8]) -> usize {
    input
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count()
}

fn consume_source_punctuation(input: &[u8], index: &mut usize) -> bool {
    // Go promotes EACH byte to a rune. Reuse the shared Go punctuation service
    // over 0..=255, not Unicode decoding or ASCII's broader symbol predicate.
    if input
        .get(*index)
        .is_some_and(|byte| native_is_go_punctuation(char::from(*byte)))
    {
        *index += 1;
        true
    } else {
        false
    }
}

fn parsed_duration_from_parts(
    negative: bool,
    hours: u64,
    minutes: u64,
    seconds: u64,
    microseconds: i64,
    fsp: i64,
) -> NativeParsedDuration {
    let magnitude = i128::from(hours) * 3_600 * 1_000_000_000
        + i128::from(minutes) * 60 * 1_000_000_000
        + i128::from(seconds) * 1_000_000_000
        + i128::from(microseconds) * 1_000;
    let signed = if negative { -magnitude } else { magnitude };
    let (nanos, overflow) = if signed > i128::from(i64::MAX) {
        (MAX_NANOS, Some(NativeDurationOverflow::Positive))
    } else if signed < i128::from(i64::MIN) {
        (-MAX_NANOS, Some(NativeDurationOverflow::Negative))
    } else {
        native_truncate_overflow_mysql_time(signed as i64)
    };
    NativeParsedDuration {
        nanos,
        fsp,
        overflow,
        truncated: false,
    }
}

fn parse_duration_number(input: &[u8], index: &mut usize) -> Result<u64, NativeDurationParseError> {
    let start = *index;
    let mut value = 0_u64;
    while let Some(byte) = input.get(*index).copied() {
        if !byte.is_ascii_digit() {
            break;
        }
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(byte - b'0')))
            .ok_or(NativeDurationParseError::NumericOverflow)?;
        *index += 1;
    }
    if *index == start {
        return Err(NativeDurationParseError::InvalidFormat);
    }
    Ok(value)
}

fn consume_duration_colon(input: &[u8], index: &mut usize) -> bool {
    skip_ascii_space(input, index);
    if input.get(*index) != Some(&b':') {
        return false;
    }
    *index += 1;
    skip_ascii_space(input, index);
    true
}

fn skip_ascii_space(input: &[u8], index: &mut usize) {
    while input.get(*index).is_some_and(u8::is_ascii_whitespace) {
        *index += 1;
    }
}

fn trim_ascii_space(input: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = input.len();
    while input.get(start).is_some_and(u8::is_ascii_whitespace) {
        start += 1;
    }
    while end > start && input.get(end - 1).is_some_and(u8::is_ascii_whitespace) {
        end -= 1;
    }
    &input[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_parser_preserves_bytes_events_fallback_flags_and_raw_zero_fsp() {
        use chrono::{FixedOffset, Utc};
        for (input, fsp, nanos) in [
            (b"- 1 02:03:04.5".as_slice(), 1, -93_784_500_000_000),
            (b"112", -1, 72_000_000_000),
            (b"12:34", 0, 45_240_000_000_000),
            (b"00:59:59.9999995", 7, 3_600_000_000_000),
            (b"1.", 6, 1_000_000_000),
        ] {
            let value = native_parse_duration(input, fsp).unwrap();
            assert_eq!(value.nanos, nanos);
            assert!(value.overflow.is_none() && !value.truncated);
        }
        assert_eq!(MAX_NANOS, 3_020_399_000_000_000); // Exactly 838:59:59.0, not .999999.
        let positive = native_parse_duration(b"838:59:59.1", 1).unwrap();
        assert_eq!(
            (positive.nanos, positive.overflow),
            (MAX_NANOS, Some(NativeDurationOverflow::Positive))
        );
        let clamped = native_parse_duration(b"-839:00:00", 0).unwrap();
        assert_eq!(
            (clamped.nanos, clamped.overflow),
            (-MAX_NANOS, Some(NativeDurationOverflow::Negative))
        );
        assert!(native_parse_duration(b"1x", 0).unwrap().truncated);
        assert!(matches!(
            native_parse_duration(b"00:00:01.0 x", 0),
            Err(NativeDurationParseError::InvalidFormat)
        ));
        assert!(matches!(
            native_parse_duration(b"18446744073709551616", 0),
            Err(NativeDurationParseError::NumericOverflow)
        ));
        assert_eq!(native_parse_duration(b"", -2).unwrap_err().event(), None);
        assert_eq!(
            NativeDurationParseError::InvalidFormat.event(),
            Some(NativeDurationParseEvent::Truncated)
        );
        assert!(matches!(
            native_parse_duration(b"20200102030405", 0),
            Err(NativeDurationParseError::DateTimeFallback(
                NativeDurationDateTimeFallbackKind::Compact14
            ))
        ));
        // The frozen byte classifier table is a full-domain oracle for reuse
        // of the shared Unicode service with byte-to-rune, not UTF-8 decoding.
        for byte in 0_u8..=255 {
            let expected = matches!(
                byte,
                b'!' | b'"'
                    | b'#'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b','
                    | b'-'
                    | b'.'
                    | b'/'
                    | b':'
                    | b';'
                    | b'?'
                    | b'@'
                    | b'['
                    | b'\\'
                    | b']'
                    | b'_'
                    | b'{'
                    | b'}'
                    | 0xA1
                    | 0xA7
                    | 0xAB
                    | 0xB6
                    | 0xB7
                    | 0xBB
                    | 0xBF
            );
            let input = [
                b'2', b'0', b'2', b'0', byte, b'0', b'1', byte, b'0', b'2', b' ',
            ];
            assert_eq!(
                native_can_fallback_to_datetime(&input),
                expected,
                "byte {byte}"
            );
        }
        assert!(!native_can_fallback_to_datetime(
            "2020¡01¡02 03:04:05".as_bytes()
        ));
        assert!(!native_can_fallback_to_datetime(b" 20200102030405"));
        for (offset, shifted_nanos) in [(0, 11_045_000_000_000), (8 * 3600, 39_845_000_000_000)] {
            let timezone = FixedOffset::east_opt(offset).unwrap();
            let value =
                native_parse_mysql_duration("2020-01-02 03:04:05.25", 2, &timezone, true, false)
                    .unwrap();
            assert_eq!(
                (value.nanos, value.fsp, value.overflow, value.truncated),
                (11_045_250_000_000, 2, None, false)
            );
            let shifted =
                native_parse_mysql_duration("2020-01-02 03:04:05+00:00", 0, &timezone, true, false)
                    .unwrap();
            assert_eq!(shifted.nanos, shifted_nanos);
        }
        let trailing = "2020-01-02 03:04:05.25x";
        assert!(
            native_parse_time(
                trailing,
                TimeType::DateTime,
                2,
                false,
                true,
                false,
                true,
                &Utc
            )
            .unwrap()
            .truncated
        );
        let fallback = native_parse_mysql_duration(trailing, 2, &Utc, true, false).unwrap();
        assert_eq!(
            (fallback.nanos, fallback.fsp, fallback.truncated),
            (11_045_250_000_000, 2, false)
        );
        let both_events = NativeParsedDuration {
            truncated: true,
            ..positive
        };
        assert_eq!(
            both_events.event(),
            Some(NativeDurationParseEvent::Overflow(
                NativeDurationOverflow::Positive
            ))
        );
        assert!(native_parse_mysql_duration("2020-02-31 03:04:05", 0, &Utc, true, false).is_err());
        assert!(native_parse_mysql_duration("2020-02-31 03:04:05", 0, &Utc, true, true).is_ok());
        assert!(native_parse_mysql_duration("2020-00-01 03:04:05", 0, &Utc, false, false).is_err());
        assert!(native_parse_mysql_duration("2020-00-01 03:04:05", 0, &Utc, true, false).is_ok());
        let zero =
            native_parse_mysql_duration("0000-00-00 00:00:00", 6, &Utc, true, false).unwrap();
        assert_eq!((zero.nanos, zero.fsp), (0, 0));
        assert_eq!(native_duration_from_time(0, -2).unwrap(), (0, 0));
        assert_eq!(native_duration_from_time(1, 6).unwrap(), (0, 6));
        assert!(native_duration_from_time(1, -2).is_err());
        assert_eq!(
            native_truncate_overflow_mysql_time(MAX_NANOS),
            (MAX_NANOS, None)
        );
        assert_eq!(
            native_truncate_overflow_mysql_time(i64::MAX),
            (MAX_NANOS, Some(NativeDurationOverflow::Positive))
        );
        assert_eq!(
            native_truncate_overflow_mysql_time(i64::MIN),
            (-MAX_NANOS, Some(NativeDurationOverflow::Negative))
        );
    }
}
