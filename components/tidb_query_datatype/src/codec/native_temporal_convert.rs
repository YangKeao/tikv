// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native temporal calendar conversions, separate from wire admission and SQL
//! expression diagnostics. Raw metadata and the original early returns matter.

use chrono::{DateTime, Datelike, Duration as ChronoDuration, TimeZone};

use super::{
    mysql::{
        Duration, Time,
        time::{
            NativeFspError, NativeTemporalValue, NativeTimeConversionError, NativeTimeError,
            TimeType, native_core_from_datetime, native_core_to_datetime,
        },
    },
    native_duration_convert::{NativeDurationParts, native_round_duration_fsp},
};

/// YEAR conversion retains the original input subject beside its clamped value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeYearConverted {
    pub value: i64,
    pub overflow: Option<String>,
}

impl NativeYearConverted {
    /// The sole strict projection used by native AdjustYear and Duration YEAR.
    pub fn into_result(self) -> Result<i64, NativeTimeError> {
        if self.overflow.is_some() {
            return Err(NativeTimeError::OutOfRange("year"));
        }
        Ok(self.value)
    }
}

/// Convert kind without broadening validation of raw zero or same-kind values.
/// A TIMESTAMP gap adjustment returns the source's DATETIME/FSP-zero value.
pub fn native_time_convert_kind<TZ: TimeZone>(
    value: NativeTemporalValue,
    kind: TimeType,
    allow_zero_in_date: bool,
    allow_invalid_date: bool,
    timezone: &TZ,
) -> Result<(NativeTemporalValue, bool), NativeTimeError> {
    let mut converted = value;
    converted.set_kind(kind);
    if value.kind == kind || value.raw == 0 {
        return Ok((converted, false));
    }
    match converted.validate(allow_zero_in_date, allow_invalid_date, timezone) {
        Ok(()) => Ok((converted, false)),
        Err(NativeTimeError::Conversion(NativeTimeConversionError::NonexistentLocalTime))
            if kind == TimeType::Timestamp =>
        {
            converted.raw =
                native_core_from_datetime(native_core_to_datetime(converted.raw, timezone, true)?);
            converted.set_kind(TimeType::DateTime);
            converted.set_fsp(0)?;
            converted.validate(allow_zero_in_date, allow_invalid_date, timezone)?;
            Ok((converted, true))
        }
        Err(error) => Err(error),
    }
}

/// Native half-up microsecond rounding, with DATE/zero and same-FSP identity
/// paths. Chrono addition retains its original panic domain, not a new clamp.
pub fn native_time_round_frac<TZ: TimeZone>(
    value: NativeTemporalValue,
    fsp: i64,
    timezone: &TZ,
) -> Result<NativeTemporalValue, NativeTimeError> {
    if value.kind == TimeType::Date || value.raw == 0 {
        return Ok(value);
    }
    let fsp = Time::native_normalize_fsp(fsp)
        .ok_or(NativeTimeError::InvalidFsp(NativeFspError::InvalidFsp(fsp)))? as u8;
    if fsp == value.fsp {
        return Ok(value);
    }
    let [year, month, day, hour, minute, second, microsecond] = Time::native_core_fields(value.raw);
    let quantum = 10_i64.pow(u32::from(6 - fsp));
    let microsecond = i64::from(microsecond);
    let rounded = ((microsecond + quantum / 2) / quantum) * quantum;
    let raw = match native_core_to_datetime(value.raw, timezone, false) {
        Ok(datetime) => {
            let shifted = datetime + ChronoDuration::microseconds(rounded - microsecond);
            native_core_from_datetime(timezone.from_utc_datetime(&shifted.naive_utc()))
        }
        Err(_) => {
            let clock_micros =
                (i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second)) * 1_000_000
                    + rounded;
            if clock_micros >= 86_400 * 1_000_000 {
                return Err(NativeTimeError::OutOfRange("rounded value"));
            }
            let seconds = clock_micros / 1_000_000;
            Time::native_core_from_fields(
                year as u16,
                month as u8,
                day as u8,
                (seconds / 3_600) as u8,
                (seconds % 3_600 / 60) as u8,
                (seconds % 60) as u8,
                (clock_micros % 1_000_000) as u32,
            )
        }
    };
    NativeTemporalValue::new(raw, value.kind, i64::from(fsp))
}

/// Add native elapsed nanoseconds to the timestamp's civil midnight. Midnight
/// must be unambiguous; do not introduce extra timezone reprojection after add.
pub fn native_duration_convert_to_time<TZ: TimeZone>(
    value: NativeDurationParts,
    timestamp: DateTime<TZ>,
    kind: TimeType,
    allow_zero_in_date: bool,
    allow_invalid_date: bool,
) -> Result<NativeTemporalValue, NativeTimeError> {
    let timezone = timestamp.timezone();
    let midnight = timezone
        .with_ymd_and_hms(
            timestamp.year(),
            timestamp.month(),
            timestamp.day(),
            0,
            0,
            0,
        )
        .single()
        .ok_or(NativeTimeError::InvalidDate)?;
    let datetime = midnight
        .checked_add_signed(ChronoDuration::nanoseconds(value.nanoseconds))
        .ok_or(NativeTimeError::OutOfRange("time"))?;
    let datetime = NativeTemporalValue::new(
        native_core_from_datetime(datetime),
        TimeType::DateTime,
        value.fsp,
    )?;
    native_time_convert_kind(
        datetime,
        kind,
        allow_zero_in_date,
        allow_invalid_date,
        &timezone,
    )
    .map(|result| result.0)
}

/// Native Duration YEAR selection. The concat branch never inspects the clock
/// or its timezone, and folds every rounding failure to the original year
/// error.
pub fn native_duration_convert_to_year_with_event<TZ: TimeZone>(
    value: NativeDurationParts,
    now: DateTime<TZ>,
    through_concat: bool,
) -> Result<NativeYearConverted, NativeTimeError> {
    if through_concat {
        let rounded = native_round_duration_fsp(value.nanoseconds, value.fsp, 0)
            .map_err(|_| NativeTimeError::OutOfRange("year"))?;
        let numeric = i64::from(Duration::hours_from_nanos(rounded.nanoseconds)) * 10_000
            + i64::from(Duration::minutes_from_nanos(rounded.nanoseconds)) * 100
            + i64::from(Duration::secs_from_nanos(rounded.nanoseconds));
        let numeric = if rounded.nanoseconds < 0 {
            -numeric
        } else {
            numeric
        };
        return Ok(native_adjust_year_with_event(numeric, false));
    }
    let datetime = native_duration_convert_to_time(value, now, TimeType::DateTime, false, false)?;
    Ok(native_adjust_year_with_event(
        i64::from(Time::native_core_fields(datetime.raw)[0]),
        false,
    ))
}

/// Preserve the native YEAR window, zero exception and original overflow
/// subject.
pub fn native_adjust_year_with_event(year: i64, adjust_zero: bool) -> NativeYearConverted {
    if year == 0 && !adjust_zero {
        return NativeYearConverted {
            value: 0,
            overflow: None,
        };
    }
    let adjusted = match year {
        0..=69 => 2000 + year,
        70..=99 => 1900 + year,
        _ => year,
    };
    let value = if adjusted < 0 {
        0
    } else {
        adjusted.clamp(1901, 2155)
    };
    NativeYearConverted {
        value,
        overflow: (value != adjusted).then(|| year.to_string()),
    }
}

/// Parse YEAR storage, not a MySQL calendar-range validator. Nonzero input is
/// narrowed only to u16 before raw packing; do not substitute checked fields.
pub fn native_parse_time_from_year(year: i64) -> Result<NativeTemporalValue, NativeTimeError> {
    if year == 0 {
        return NativeTemporalValue::new(0, TimeType::Date, 0);
    }
    let year = u16::try_from(year).map_err(|_| NativeTimeError::OutOfRange("year"))?;
    NativeTemporalValue::new(
        Time::native_core_from_fields(year, 0, 0, 0, 0, 0, 0),
        TimeType::DateTime,
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_kind_and_round_preserve_raw_early_returns_dst_and_invalid_calendar() {
        let utc = chrono_tz::UTC;
        let raw_date = NativeTemporalValue {
            raw: u64::MAX,
            kind: TimeType::Date,
            fsp: 255,
        };
        assert_eq!(
            native_time_round_frac(raw_date, -2, &utc).unwrap(),
            raw_date
        );
        let zero = NativeTemporalValue {
            raw: 0,
            kind: TimeType::DateTime,
            fsp: 255,
        };
        assert_eq!(native_time_round_frac(zero, -2, &utc).unwrap(), zero);
        assert_eq!(
            native_time_convert_kind(zero, TimeType::Timestamp, false, false, &utc).unwrap(),
            (
                NativeTemporalValue {
                    kind: TimeType::Timestamp,
                    ..zero
                },
                false
            )
        );
        assert_eq!(
            native_time_convert_kind(raw_date, TimeType::Date, false, false, &utc).unwrap(),
            (NativeTemporalValue { fsp: 0, ..raw_date }, false)
        );
        let raw_same = NativeTemporalValue {
            raw: u64::MAX,
            kind: TimeType::DateTime,
            fsp: 6,
        };
        assert_eq!(native_time_round_frac(raw_same, 7, &utc).unwrap(), raw_same);
        assert_eq!(
            native_time_convert_kind(raw_same, TimeType::DateTime, false, false, &utc).unwrap(),
            (raw_same, false)
        );
        let invalid = NativeTemporalValue {
            raw: Time::native_core_from_fields(2020, 0, 0, 12, 34, 56, 999_999),
            kind: TimeType::DateTime,
            fsp: 6,
        };
        let rounded = native_time_round_frac(invalid, 0, &utc).unwrap();
        assert_eq!(
            Time::native_core_fields(rounded.raw),
            [2020, 0, 0, 12, 34, 57, 0]
        );
        assert_eq!(rounded.fsp, 0);
        assert_eq!(
            native_time_round_frac(invalid, -2, &utc),
            Err(NativeTimeError::InvalidFsp(NativeFspError::InvalidFsp(-2)))
        );
        let invalid_carry = NativeTemporalValue {
            raw: Time::native_core_from_fields(2020, 0, 0, 23, 59, 59, 999_999),
            ..invalid
        };
        assert_eq!(
            native_time_round_frac(invalid_carry, 0, &utc),
            Err(NativeTimeError::OutOfRange("rounded value"))
        );
        let la = chrono_tz::America::Los_Angeles;
        let before_gap = NativeTemporalValue {
            raw: Time::native_core_from_fields(2011, 3, 13, 1, 59, 59, 999_999),
            kind: TimeType::DateTime,
            fsp: 6,
        };
        let rounded = native_time_round_frac(before_gap, 0, &la).unwrap();
        assert_eq!(
            Time::native_core_fields(rounded.raw),
            [2011, 3, 13, 3, 0, 0, 0]
        );
        let gap = NativeTemporalValue {
            raw: Time::native_core_from_fields(2011, 3, 13, 2, 30, 0, 0),
            kind: TimeType::DateTime,
            fsp: 6,
        };
        let (adjusted, did_adjust) =
            native_time_convert_kind(gap, TimeType::Timestamp, false, false, &la).unwrap();
        assert!(did_adjust);
        assert_eq!(
            Time::native_core_fields(adjusted.raw),
            [2011, 3, 13, 3, 0, 0, 0]
        );
        assert_eq!(adjusted.kind, TimeType::DateTime);
        assert_eq!(adjusted.fsp, 0);
    }

    #[test]
    fn duration_calendar_and_year_keep_raw_clock_modes_subjects_and_storage_domain() {
        let utc = chrono_tz::UTC;
        let now = utc
            .with_ymd_and_hms(2020, 12, 31, 17, 0, 0)
            .single()
            .unwrap();
        let elapsed = NativeDurationParts {
            nanoseconds: 25 * 3_600_000_000_000,
            fsp: 6,
        };
        let time = native_duration_convert_to_time(elapsed, now, TimeType::DateTime, false, false)
            .unwrap();
        assert_eq!(Time::native_core_fields(time.raw), [2021, 1, 1, 1, 0, 0, 0]);
        assert_eq!(time.fsp, 6);
        let date =
            native_duration_convert_to_time(elapsed, now, TimeType::Date, false, false).unwrap();
        assert_eq!(Time::native_core_fields(date.raw), [2021, 1, 1, 1, 0, 0, 0]);
        assert_eq!(date.fsp, 0);
        assert_eq!(
            native_duration_convert_to_year_with_event(elapsed, now, false)
                .unwrap()
                .into_result()
                .unwrap(),
            2021
        );
        let concat = NativeDurationParts {
            nanoseconds: (20 * 60 + 12) * 1_000_000_000,
            fsp: 6,
        };
        assert_eq!(
            native_duration_convert_to_year_with_event(concat, now, true).unwrap(),
            NativeYearConverted {
                value: 2012,
                overflow: None
            }
        );
        assert_eq!(
            native_duration_convert_to_year_with_event(
                NativeDurationParts {
                    nanoseconds: i64::MAX,
                    fsp: 9
                },
                now,
                true
            ),
            Err(NativeTimeError::OutOfRange("year"))
        );
        for (year, adjust_zero, expected, subject) in [
            (0, false, 0, None),
            (0, true, 2000, None),
            (69, false, 2069, None),
            (70, false, 1970, None),
            (-1, false, 0, Some("-1")),
            (100, false, 1901, Some("100")),
            (2156, false, 2155, Some("2156")),
        ] {
            let converted = native_adjust_year_with_event(year, adjust_zero);
            assert_eq!(converted.value, expected);
            assert_eq!(converted.overflow.as_deref(), subject);
            if subject.is_some() {
                assert_eq!(
                    converted.into_result(),
                    Err(NativeTimeError::OutOfRange("year"))
                );
            } else {
                assert_eq!(converted.into_result().unwrap(), expected);
            }
        }
        let zero = native_parse_time_from_year(0).unwrap();
        assert_eq!(
            zero,
            NativeTemporalValue {
                raw: 0,
                kind: TimeType::Date,
                fsp: 0
            }
        );
        let wide = native_parse_time_from_year(65535).unwrap();
        assert_eq!(
            Time::native_core_fields(wide.raw),
            [16383, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(wide.kind, TimeType::DateTime);
        for year in [-1, 65536] {
            assert_eq!(
                native_parse_time_from_year(year),
                Err(NativeTimeError::OutOfRange("year"))
            );
        }
        let midnight_gap = chrono_tz::America::Sao_Paulo
            .with_ymd_and_hms(2018, 11, 4, 12, 0, 0)
            .single()
            .unwrap();
        assert_eq!(
            native_duration_convert_to_time(
                NativeDurationParts {
                    nanoseconds: 0,
                    fsp: 0
                },
                midnight_gap,
                TimeType::DateTime,
                false,
                false
            ),
            Err(NativeTimeError::InvalidDate)
        );
    }
}
