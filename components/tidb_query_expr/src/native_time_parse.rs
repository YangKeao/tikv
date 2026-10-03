// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! The native duration family's datetime fallback. Delimited dates keep their
//! wide year and zero-date domain; compact literals share the generic native
//! datatype parser's decomposition and non-TIMESTAMP validator. UTC below is a
//! literal policy of this fallback, never a session timezone getter.

use chrono::{Datelike, NaiveDate, Timelike};
use tidb_query_datatype::codec::mysql::Time;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeDurationDateTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub micros: u32,
    pub fsp: i32,
}

/// Original native duration-family datetime grammar, not the stricter stored
/// Time parser. In particular, ordinary delimited years are not narrowed to
/// the wire Time range, and only compact literals round their seventh digit.
pub fn parse_native_duration_datetime(value: &str) -> Option<NativeDurationDateTime> {
    let value = value.trim();
    let (date, time) = match value.split_once(|c: char| c.is_whitespace() || c == 'T') {
        Some((date, time)) => (date, time.trim()),
        None => (value, ""),
    };
    if time.is_empty() {
        if let Some(compact) = parse_compact_datetime_utc(date) {
            return Some(compact);
        }
    }
    let parts = Time::native_split_date_components(date)?;
    let year = Time::native_expand_date_year(parts[0].0, parts[0].1);
    let month = parts[1].0;
    let day = parts[2].0;
    if month > 12 || day > 31 {
        return None;
    }
    if month != 0 && day > Time::native_days_in_month(year, month) {
        return None;
    }
    let (hour, minute, second, fraction) = if time.is_empty() {
        (0, 0, 0, String::new())
    } else {
        Time::parse_native_clock_with_fraction(time)?
    };
    let fsp = fraction.len() as i32;
    let micros = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u32>().ok()? * 10u32.pow(6 - fsp as u32)
    };
    Some(NativeDurationDateTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
        micros,
        fsp: fsp.min(6),
    })
}

fn parse_compact_datetime_utc(value: &str) -> Option<NativeDurationDateTime> {
    let (digits, fraction) = value.split_once('.').unwrap_or((value, ""));
    if !matches!(digits.len(), 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 14) {
        return None;
    }
    // This family's existing guard deliberately excludes date-only compact
    // fraction-as-clock spellings, even though the generic SDK supports them.
    if !fraction.is_empty() && digits.len() != 14 {
        return None;
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let fsp = fraction.len().min(6) as u8;
    let parts = Time::native_compact_datetime_parts(digits, fraction.as_bytes(), fsp).ok()?;
    let mut fields = parts.fields;
    let micros = u32::try_from(parts.microsecond).ok()?;
    validate_compact(fields, micros)?;
    if parts.carry {
        // The original generic SDK adds one second to the UTC instant here.
        // Invalid/partial civil dates cannot be carried; no saturation or
        // made-up day-number normalization is introduced for those inputs.
        let instant = NaiveDate::from_ymd_opt(fields[0], fields[1] as u32, fields[2] as u32)?
            .and_hms_micro_opt(fields[3] as u32, fields[4] as u32, fields[5] as u32, micros)?
            .and_utc();
        let carried = instant + chrono::Duration::seconds(1);
        fields = [
            carried.year(),
            carried.month() as i32,
            carried.day() as i32,
            carried.hour() as i32,
            carried.minute() as i32,
            carried.second() as i32,
        ];
        validate_compact(fields, micros)?;
    }
    Some(NativeDurationDateTime {
        year: i64::from(fields[0]),
        month: fields[1] as u32,
        day: fields[2] as u32,
        hour: fields[3] as u32,
        minute: fields[4] as u32,
        second: fields[5] as u32,
        micros,
        fsp: i32::from(fsp),
    })
}

fn validate_compact(fields: [i32; 6], micros: u32) -> Option<()> {
    Time::validate_native_datetime_fields(
        fields[0],
        u8::try_from(fields[1]).ok()?,
        u8::try_from(fields[2]).ok()?,
        u8::try_from(fields[3]).ok()?,
        u8::try_from(fields[4]).ok()?,
        u8::try_from(fields[5]).ok()?,
        micros,
        true,
        false,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_duration_datetime_keeps_wide_dates_compact_carry_and_clock_rejections() {
        let parse = parse_native_duration_datetime;
        let fields = |input| {
            parse(input).map(|value| {
                (
                    value.year,
                    value.month,
                    value.day,
                    value.hour,
                    value.minute,
                    value.second,
                    value.micros,
                    value.fsp,
                )
            })
        };
        assert_eq!(
            fields(" 4294967295-12-31 01:02:03.1234567 "),
            Some((4_294_967_295, 12, 31, 1, 2, 3, 123_456, 6))
        );
        assert_eq!(fields("0000-02-29"), Some((0, 2, 29, 0, 0, 0, 0, 0)));
        assert_eq!(
            fields("2020-00-31T01:02:03"),
            Some((2020, 0, 31, 1, 2, 3, 0, 0))
        );
        assert_eq!(fields("0000-00-00"), Some((0, 0, 0, 0, 0, 0, 0, 0)));
        assert_eq!(fields("00-00-00"), Some((2000, 0, 0, 0, 0, 0, 0, 0)));
        for (input, date) in [
            ("20121", (2020, 12, 1, 0, 0, 0, 0, 0)),
            ("201231", (2020, 12, 31, 0, 0, 0, 0, 0)),
            ("2012311", (2020, 12, 31, 1, 0, 0, 0, 0)),
            ("20201231", (2020, 12, 31, 0, 0, 0, 0, 0)),
            ("201231121", (2020, 12, 31, 12, 1, 0, 0, 0)),
            ("2012311259", (2020, 12, 31, 12, 59, 0, 0, 0)),
            ("20123112591", (2020, 12, 31, 12, 59, 1, 0, 0)),
            ("201231125959", (2020, 12, 31, 12, 59, 59, 0, 0)),
            ("20201231125959", (2020, 12, 31, 12, 59, 59, 0, 0)),
            ("00000000", (0, 0, 0, 0, 0, 0, 0, 0)),
            ("20200031", (2020, 0, 31, 0, 0, 0, 0, 0)),
        ] {
            assert_eq!(fields(input), Some(date), "{input}");
        }
        assert_eq!(
            fields("20201231235959.9999995"),
            Some((2021, 1, 1, 0, 0, 0, 0, 6))
        );
        assert_eq!(
            fields("00000228235959.9999995"),
            Some((0, 2, 29, 0, 0, 0, 0, 6))
        );
        assert_eq!(
            fields("20200101010203.0123456"),
            Some((2020, 1, 1, 1, 2, 3, 12_346, 6))
        );
        for input in [
            "99991231235959.9999995",
            "20200031235959.9999995",
            "20200230235959.9999995",
            "20201231.5",
            "201231125959.1",
            "20201231240000",
            "20201231235960",
            "2020-12-31 24:00:00",
            "2020-12-31 00:60:00",
            "2020-12-31 00:00:60",
            "1900-02-29",
            "2020-13-01",
            "2020-00-32",
            "2020-01-01 01:02:03.123456x",
            "20200101010203.123456x",
            "4294967296-01-01",
        ] {
            assert!(parse(input).is_none(), "{input}");
        }
    }
}
