// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native string and numeric temporal parsing with the caller's actual
//! timezone.

use chrono::{FixedOffset, TimeZone};

use super::{
    NativeCompactDateTimeError, NativeFspError, NativeTemporalValue, NativeTimeConversionError,
    NativeTimeError, NativeTimezoneSuffix, Time, TimeType, native_core_from_datetime,
    native_core_to_datetime, native_get_frac_index, native_get_timezone, native_parse_date_format,
    native_time_is_ascii_punctuation,
};

/// Parsed temporal value and the two original independent diagnostic bits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeParsedTime {
    pub time: NativeTemporalValue,
    pub truncated: bool,
    pub dst_adjusted: bool,
}

/// The source value retained beside a numeric conversion error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeTimeParseOutcome<T> {
    pub value: T,
    pub error: Option<NativeTimeError>,
}

impl<T> NativeTimeParseOutcome<T> {
    fn from_result(result: Result<T, NativeTimeError>, fallback: T) -> Self {
        match result {
            Ok(value) => Self { value, error: None },
            Err(error) => Self {
                value: fallback,
                error: Some(error),
            },
        }
    }

    /// The result-only projection used by callers that do not retain error
    /// values.
    pub fn into_result(self) -> Result<T, NativeTimeError> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.value),
        }
    }
}

/// Parse a string using the native conversion flags, including float-string
/// policy.
#[allow(clippy::too_many_arguments)]
pub fn native_parse_time<TZ: TimeZone>(
    input: &str,
    kind: TimeType,
    fsp: i64,
    is_float: bool,
    allow_zero_in_date: bool,
    allow_invalid_date: bool,
    ignore_zero_date_err: bool,
    timezone: &TZ,
) -> Result<NativeParsedTime, NativeTimeError> {
    if is_float && input.starts_with("0.0") {
        return Ok(NativeParsedTime {
            time: NativeTemporalValue::new(0, kind, 0)?,
            truncated: false,
            dst_adjusted: false,
        });
    }
    let fsp = Time::native_normalize_fsp(fsp)
        .ok_or(NativeTimeError::InvalidFsp(NativeFspError::InvalidFsp(fsp)))?;
    let (raw, truncated) = parse_datetime_core(
        input,
        fsp,
        is_float,
        allow_zero_in_date,
        allow_invalid_date,
        ignore_zero_date_err,
        timezone,
    )?;
    let mut time = NativeTemporalValue::new(raw, kind, fsp)?;
    let mut dst_adjusted = false;
    match time.validate(allow_zero_in_date, allow_invalid_date, timezone) {
        Ok(()) => {}
        Err(NativeTimeError::Conversion(NativeTimeConversionError::NonexistentLocalTime))
            if kind == TimeType::Timestamp =>
        {
            // Preserve the adjusted value beside the source warning disposition.
            let adjusted = native_core_to_datetime(time.raw, timezone, true)
                .map_err(NativeTimeError::Conversion)?;
            time.raw = native_core_from_datetime(adjusted);
            time.validate(allow_zero_in_date, allow_invalid_date, timezone)?;
            dst_adjusted = true;
        }
        Err(error) => return Err(error),
    }
    Ok(NativeParsedTime {
        time,
        truncated,
        dst_adjusted,
    })
}

fn parse_datetime_core<TZ: TimeZone>(
    input: &str,
    fsp: i64,
    is_float: bool,
    allow_zero_in_date: bool,
    allow_invalid_date: bool,
    ignore_zero_date_err: bool,
    timezone: &TZ,
) -> Result<(u64, bool), NativeTimeError> {
    let (mut parts, mut fraction, mut timezone_suffix, mut truncated) = split_datetime(input);
    let no_absorb = |parts: &[String]| parts.len() > 5 || (parts.len() == 1 && parts[0].len() > 4);
    if !fraction.is_empty() && !is_float && !no_absorb(&parts) {
        parts.push(std::mem::take(&mut fraction));
    }
    if let Some(suffix) = &timezone_suffix {
        if suffix.sign.is_some()
            && !no_absorb(&parts)
            && !(suffix.minute.is_some() && !suffix.has_colon)
        {
            if let Some(hour) = &suffix.hour {
                parts.push(hour.clone());
            }
            if let Some(minute) = &suffix.minute {
                parts.push(minute.clone());
            }
            timezone_suffix = None;
        }
    }

    let mut fields = [0_i32; 6];
    let hhmmss;
    let mut compact_fraction = None;
    match parts.len() {
        0 => return Err(NativeTimeError::InvalidDate),
        1 if is_float => {
            let number = parts[0]
                .parse::<i64>()
                .map_err(|_| NativeTimeError::InvalidDate)?;
            let numeric = native_parse_time_from_num(
                number,
                TimeType::DateTime,
                0,
                allow_zero_in_date,
                allow_invalid_date,
                ignore_zero_date_err,
                timezone,
            )
            .into_result()?;
            let core = Time(numeric.time.raw);
            fields = [
                core.year() as i32,
                core.month() as i32,
                core.day() as i32,
                core.hour() as i32,
                core.minute() as i32,
                core.second() as i32,
            ];
            let length = parts[0].len();
            hhmmss = parts[0] == "0" || (9..=14).contains(&length);
        }
        1 => {
            let compact =
                Time::native_compact_datetime_parts(&parts[0], fraction.as_bytes(), fsp as u8)
                    .map_err(|error| match error {
                        NativeCompactDateTimeError::InvalidDate => NativeTimeError::InvalidDate,
                        NativeCompactDateTimeError::InvalidFsp(error) => {
                            NativeTimeError::InvalidFsp(error)
                        }
                    })?;
            fields = compact.fields;
            hhmmss = compact.has_clock;
            truncated |= compact.truncated;
            compact_fraction = Some((compact.microsecond, compact.carry));
        }
        2 => return Err(NativeTimeError::InvalidDate),
        3..=6 => {
            for (field, part) in fields.iter_mut().zip(&parts) {
                *field = part.parse().map_err(|_| NativeTimeError::InvalidDate)?;
            }
            hhmmss = parts.len() == 6;
        }
        _ => {
            truncated = true;
            for (field, part) in fields.iter_mut().zip(parts.iter().take(6)) {
                *field = part.parse().map_err(|_| NativeTimeError::InvalidDate)?;
            }
            hhmmss = true;
        }
    }
    if !is_float && parts[0].len() <= 2 {
        let all_zero = fields.iter().all(|field| *field == 0) && fraction.is_empty();
        if !all_zero {
            fields[0] = adjust_two_digit_year(fields[0]);
        }
    }
    let (microsecond, overflow) = if let Some(fraction) = compact_fraction {
        fraction
    } else if hhmmss {
        Time::native_parse_fraction(fraction.as_bytes(), fsp)
            .map_err(NativeTimeError::InvalidFsp)?
    } else {
        (0, false)
    };
    let mut raw = checked_core(fields, microsecond)?;
    if overflow {
        // Carry is an INSTANT addition in the session zone, before suffix handling.
        let carried = native_core_to_datetime(raw, timezone, false)? + chrono::Duration::seconds(1);
        raw = native_core_from_datetime(timezone.from_utc_datetime(&carried.naive_utc()));
    }
    if let Some(suffix) = timezone_suffix {
        if !hhmmss {
            return Err(NativeTimeError::InvalidDate);
        }
        let hour = suffix
            .hour
            .as_deref()
            .unwrap_or("0")
            .parse::<i32>()
            .map_err(|_| NativeTimeError::InvalidDate)?;
        let minute = suffix
            .minute
            .as_deref()
            .unwrap_or("0")
            .parse::<i32>()
            .map_err(|_| NativeTimeError::InvalidDate)?;
        if hour > 14
            || minute > 59
            || (hour == 14 && minute != 0)
            || (suffix.sign == Some('-') && hour == 0 && minute == 0)
        {
            return Err(NativeTimeError::InvalidDate);
        }
        let mut offset = hour * 3_600 + minute * 60;
        if suffix.sign == Some('-') {
            offset = -offset;
        }
        let fixed = FixedOffset::east_opt(offset).ok_or(NativeTimeError::InvalidDate)?;
        let source = native_core_to_datetime(raw, &fixed, false)?;
        raw = native_core_from_datetime(source.with_timezone(timezone));
    }
    Ok((raw, truncated))
}

fn split_datetime(input: &str) -> (Vec<String>, String, Option<NativeTimezoneSuffix>, bool) {
    let mut value = input;
    let mut suffix = native_get_timezone(value);
    if let Some(timezone) = &mut suffix {
        if timezone.index > 0 {
            let mut index = timezone.index;
            while index > 0 && native_time_is_ascii_punctuation(value.as_bytes()[index - 1]) {
                index -= 1;
            }
            value = &value[..index];
        } else {
            suffix = None;
        }
    }
    let mut fraction = String::new();
    let mut truncated = false;
    let fraction_index = native_get_frac_index(value);
    if fraction_index > 0 {
        let mut end = fraction_index as usize + 1;
        while end < value.len() && value.as_bytes()[end].is_ascii_digit() {
            end += 1;
        }
        truncated = end != value.len();
        fraction.push_str(&value[fraction_index as usize + 1..end]);
        let mut start = fraction_index as usize;
        while start > 0 && native_time_is_ascii_punctuation(value.as_bytes()[start - 1]) {
            start -= 1;
        }
        value = &value[..start];
    }
    (
        native_parse_date_format(value).unwrap_or_default(),
        fraction,
        suffix,
        truncated,
    )
}

fn checked_core(fields: [i32; 6], microsecond: i64) -> Result<u64, NativeTimeError> {
    NativeTemporalValue::from_date_checked(
        fields[0],
        fields[1],
        fields[2],
        fields[3],
        fields[4],
        fields[5],
        microsecond as i32,
        TimeType::DateTime,
        0,
    )
    .map(|time| time.raw)
}

const fn adjust_two_digit_year(year: i32) -> i32 {
    match year {
        0..=99 => Time::native_expand_date_year(year as u32, 2) as i32,
        _ => year,
    }
}

/// Parse a numeric datetime into the requested kind, preserving its error-side
/// zero.
#[allow(clippy::too_many_arguments)]
pub fn native_parse_time_from_num<TZ: TimeZone>(
    number: i64,
    kind: TimeType,
    fsp: i64,
    allow_zero_in_date: bool,
    allow_invalid_date: bool,
    ignore_zero_date_err: bool,
    timezone: &TZ,
) -> NativeTimeParseOutcome<NativeParsedTime> {
    let fallback = NativeParsedTime {
        time: NativeTemporalValue::new(0, kind, 0)
            .expect("zero target time is a valid MySQL error-side value"),
        truncated: false,
        dst_adjusted: false,
    };
    let result = (|| {
        if number == 0 {
            if !ignore_zero_date_err {
                return Err(NativeTimeError::ZeroDate);
            }
            return Ok(fallback);
        }
        let (normalized, _) = normalize_numeric_datetime(number)?;
        let fields = numeric_fields(normalized);
        let mut time = NativeTemporalValue::from_date_checked(
            fields[0], fields[1], fields[2], fields[3], fields[4], fields[5], 0, kind, fsp,
        )?;
        let mut dst_adjusted = false;
        match time.validate(allow_zero_in_date, allow_invalid_date, timezone) {
            Ok(()) => {}
            Err(NativeTimeError::Conversion(NativeTimeConversionError::NonexistentLocalTime))
                if kind == TimeType::Timestamp =>
            {
                let adjusted = native_core_to_datetime(time.raw, timezone, true)
                    .map_err(NativeTimeError::Conversion)?;
                time.raw = native_core_from_datetime(adjusted);
                time.validate(allow_zero_in_date, allow_invalid_date, timezone)?;
                dst_adjusted = true;
            }
            Err(error) => return Err(error),
        }
        Ok(NativeParsedTime {
            time,
            truncated: false,
            dst_adjusted,
        })
    })();
    NativeTimeParseOutcome::from_result(result, fallback)
}

/// Parse an integer with the native DATE-versus-DATETIME classification.
pub fn native_parse_time_from_int64<TZ: TimeZone>(
    number: i64,
    allow_zero_in_date: bool,
    allow_invalid_date: bool,
    timezone: &TZ,
) -> Result<NativeTemporalValue, NativeTimeError> {
    if number == 0 {
        return NativeTemporalValue::new(0, TimeType::Date, 0);
    }
    let (normalized, kind) = normalize_numeric_datetime(number)?;
    let fields = numeric_fields(normalized);
    let time = NativeTemporalValue::from_date_checked(
        fields[0], fields[1], fields[2], fields[3], fields[4], fields[5], 0, kind, 0,
    )?;
    time.validate(allow_zero_in_date, allow_invalid_date, timezone)?;
    Ok(time)
}

/// Parse a float using native casts and fractional rounding, without a carry
/// pass.
pub fn native_parse_time_from_float64<TZ: TimeZone>(
    value: f64,
    allow_zero_in_date: bool,
    allow_invalid_date: bool,
    timezone: &TZ,
) -> NativeTimeParseOutcome<NativeTemporalValue> {
    let result = (|| {
        let integer = value as i64;
        let mut time = native_parse_time_from_int64(
            integer,
            allow_zero_in_date,
            allow_invalid_date,
            timezone,
        )?;
        if time.kind == TimeType::DateTime {
            let microsecond = ((value - integer as f64) * 1_000_000.0).round() as u32;
            let core = Time(time.raw);
            time.raw = Time::native_core_from_fields(
                core.year() as u16,
                core.month() as u8,
                core.day() as u8,
                core.hour() as u8,
                core.minute() as u8,
                core.second() as u8,
                microsecond,
            );
        }
        Ok(time)
    })();
    NativeTimeParseOutcome::from_result(result, zero_datetime())
}

/// Parse the original native Decimal display text, truncating its written
/// fraction.
pub fn native_parse_time_from_decimal_text<TZ: TimeZone>(
    text: &str,
    allow_zero_in_date: bool,
    allow_invalid_date: bool,
    timezone: &TZ,
) -> NativeTimeParseOutcome<NativeTemporalValue> {
    let result = (|| {
        let (integer_text, fraction) = text.split_once('.').unwrap_or((text, ""));
        let integer = integer_text
            .parse::<i64>()
            .map_err(|_| NativeTimeError::InvalidDate)?;
        let mut time = native_parse_time_from_int64(
            integer,
            allow_zero_in_date,
            allow_invalid_date,
            timezone,
        )?;
        let fsp = fraction.len().min(6) as i64;
        time.set_fsp(fsp)?;
        if fsp > 0 && time.kind == TimeType::DateTime {
            let mut microsecond_text = fraction[..fsp as usize].to_owned();
            while microsecond_text.len() < 6 {
                microsecond_text.push('0');
            }
            let microsecond = microsecond_text
                .parse::<u32>()
                .map_err(|_| NativeTimeError::InvalidDate)?;
            let core = Time(time.raw);
            time.raw = Time::native_core_from_fields(
                core.year() as u16,
                core.month() as u8,
                core.day() as u8,
                core.hour() as u8,
                core.minute() as u8,
                core.second() as u8,
                microsecond,
            );
        }
        Ok(time)
    })();
    NativeTimeParseOutcome::from_result(result, zero_datetime())
}

fn zero_datetime() -> NativeTemporalValue {
    NativeTemporalValue::new(0, TimeType::DateTime, 0)
        .expect("zero DATETIME is a valid MySQL error-side value")
}

fn normalize_numeric_datetime(mut number: i64) -> Result<(i64, TimeType), NativeTimeError> {
    if !(0..=99_999_999_999_999).contains(&number) {
        return Err(NativeTimeError::InvalidDate);
    }
    if number >= 10_000_101_000_000 {
        return Ok((number, TimeType::DateTime));
    }
    if number < 101 {
        return Err(NativeTimeError::InvalidDate);
    }
    if number <= 691_231 {
        return Ok(((number + 20_000_000) * 1_000_000, TimeType::Date));
    }
    if number < 700_101 {
        return Err(NativeTimeError::InvalidDate);
    }
    if number <= 991_231 {
        return Ok(((number + 19_000_000) * 1_000_000, TimeType::Date));
    }
    if number <= 99_991_231 {
        return Ok((number * 1_000_000, TimeType::Date));
    }
    if number < 101_000_000 {
        return Err(NativeTimeError::InvalidDate);
    }
    if number <= 691_231_235_959 {
        number += 20_000_000_000_000;
    } else if number < 700_101_000_000 {
        return Err(NativeTimeError::InvalidDate);
    } else if number <= 991_231_235_959 {
        number += 19_000_000_000_000;
    }
    Ok((number, TimeType::DateTime))
}

fn numeric_fields(number: i64) -> [i32; 6] {
    let mut remainder = number;
    let second = (remainder % 100) as i32;
    remainder /= 100;
    let minute = (remainder % 100) as i32;
    remainder /= 100;
    let hour = (remainder % 100) as i32;
    remainder /= 100;
    let day = (remainder % 100) as i32;
    remainder /= 100;
    let month = (remainder % 100) as i32;
    let year = (remainder / 100) as i32;
    [year, month, day, hour, minute, second]
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    #[test]
    fn string_parser_preserves_early_zero_hidden_clock_carry_and_suffix_order() {
        let zero = native_parse_time(
            "0.0junk",
            TimeType::DateTime,
            -2,
            true,
            false,
            false,
            false,
            &Utc,
        )
        .unwrap();
        assert_eq!(
            (
                zero.time.raw,
                zero.time.kind,
                zero.time.fsp,
                zero.truncated,
                zero.dst_adjusted
            ),
            (0, TimeType::DateTime, 0, false, false)
        );
        for (input, is_float) in [(" 0.0junk", true), ("0.0junk", false)] {
            assert_eq!(
                native_parse_time(
                    input,
                    TimeType::Date,
                    -2,
                    is_float,
                    false,
                    false,
                    true,
                    &Utc
                ),
                Err(NativeTimeError::InvalidFsp(NativeFspError::InvalidFsp(-2)))
            );
        }
        let date = native_parse_time(
            "2020-01-02 03:04:05.123456",
            TimeType::Date,
            6,
            false,
            false,
            false,
            true,
            &Utc,
        )
        .unwrap();
        assert_eq!(
            (date.time.raw, date.time.fsp),
            (
                Time::native_core_from_fields(2020, 1, 2, 3, 4, 5, 123_456),
                0
            )
        );
        let la = chrono_tz::America::Los_Angeles;
        let carry = native_parse_time(
            "20110313015959.999999",
            TimeType::DateTime,
            0,
            false,
            false,
            false,
            true,
            &la,
        )
        .unwrap();
        assert_eq!(
            carry.time.raw,
            Time::native_core_from_fields(2011, 3, 13, 3, 0, 0, 0)
        );
        for kind in [TimeType::DateTime, TimeType::Timestamp] {
            let gap = native_parse_time(
                "2021-03-14 02:30:00.123456",
                kind,
                6,
                false,
                false,
                false,
                true,
                &la,
            )
            .unwrap();
            assert_eq!(gap.dst_adjusted, kind == TimeType::Timestamp);
            assert_eq!(
                gap.time.raw,
                if kind == TimeType::Timestamp {
                    Time::native_core_from_fields(2021, 3, 14, 3, 0, 0, 0)
                } else {
                    Time::native_core_from_fields(2021, 3, 14, 2, 30, 0, 123_456)
                }
            );
        }
        for (input, expected) in [
            (
                "2020-01-01 08:00:00+08:00",
                Time::native_core_from_fields(2020, 1, 1, 0, 0, 0, 0),
            ),
            (
                "2020-01-01+12:34",
                Time::native_core_from_fields(2020, 1, 1, 12, 34, 0, 0),
            ),
        ] {
            assert_eq!(
                native_parse_time(
                    input,
                    TimeType::DateTime,
                    0,
                    false,
                    false,
                    false,
                    true,
                    &Utc
                )
                .unwrap()
                .time
                .raw,
                expected
            );
        }
        let truncated = native_parse_time(
            "2020-01-01 01:02:03.123x",
            TimeType::DateTime,
            3,
            false,
            false,
            false,
            true,
            &Utc,
        )
        .unwrap();
        assert!(truncated.truncated);
        assert_eq!(
            truncated.time.raw,
            Time::native_core_from_fields(2020, 1, 1, 1, 2, 3, 123_000)
        );
    }

    #[test]
    fn numeric_parser_preserves_error_values_nan_fraction_and_decimal_representation() {
        for kind in [TimeType::Date, TimeType::DateTime, TimeType::Timestamp] {
            for allow_zero in [false, true] {
                let outcome =
                    native_parse_time_from_num(0, kind, -2, false, false, allow_zero, &Utc);
                assert_eq!(
                    (
                        outcome.value.time.raw,
                        outcome.value.time.kind,
                        outcome.value.time.fsp
                    ),
                    (0, kind, 0)
                );
                assert_eq!(
                    outcome.error,
                    if allow_zero {
                        None
                    } else {
                        Some(NativeTimeError::ZeroDate)
                    }
                );
                assert!(!outcome.value.truncated && !outcome.value.dst_adjusted);
            }
        }
        let invalid = native_parse_time_from_num(
            99_999_999_999_999,
            TimeType::DateTime,
            -2,
            false,
            false,
            true,
            &Utc,
        );
        assert_eq!(invalid.error, Some(NativeTimeError::OutOfRange("month")));
        assert_eq!(
            (
                invalid.value.time.raw,
                invalid.value.time.kind,
                invalid.value.time.fsp
            ),
            (0, TimeType::DateTime, 0)
        );
        let nan = native_parse_time_from_float64(f64::NAN, false, false, &Utc)
            .into_result()
            .unwrap();
        assert_eq!((nan.raw, nan.kind, nan.fsp), (0, TimeType::Date, 0));
        let invalid_float = native_parse_time_from_float64(f64::INFINITY, false, false, &Utc);
        assert_eq!(invalid_float.error, Some(NativeTimeError::InvalidDate));
        assert_eq!(invalid_float.value, zero_datetime());
        let rounded = native_parse_time_from_float64(101_000_000.999_999_9, false, false, &Utc)
            .into_result()
            .unwrap();
        assert_eq!(
            (rounded.raw, rounded.fsp),
            (
                Time::native_core_from_fields(2000, 1, 1, 0, 0, 0, 1_000_000),
                0
            )
        );
        let decimal =
            native_parse_time_from_decimal_text("20200101000000.1234567", false, false, &Utc)
                .into_result()
                .unwrap();
        assert_eq!(
            (decimal.raw, decimal.kind, decimal.fsp),
            (
                Time::native_core_from_fields(2020, 1, 1, 0, 0, 0, 123_456),
                TimeType::DateTime,
                6
            )
        );
        let date = native_parse_time_from_decimal_text("20200101.123", false, false, &Utc)
            .into_result()
            .unwrap();
        assert_eq!(
            (date.raw, date.kind, date.fsp),
            (
                Time::native_core_from_fields(2020, 1, 1, 0, 0, 0, 0),
                TimeType::Date,
                0
            )
        );
        let bad_fraction =
            native_parse_time_from_decimal_text("20200101000000.abc", false, false, &Utc);
        assert_eq!(bad_fraction.error, Some(NativeTimeError::InvalidDate));
        assert_eq!(bad_fraction.value, zero_datetime());
    }
}
