// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native public datatype EXTRACT helpers. These preserve raw field and signed
//! nanosecond projection, independently of the wire EXTRACT policies.

use super::{NativeTimeError, Time};
use crate::codec::mysql::Duration;

/// Returns whether the interval unit contains a clock component.
pub fn native_is_clock_unit(unit: &str) -> bool {
    matches!(
        unit.to_ascii_uppercase().as_str(),
        "MICROSECOND"
            | "SECOND"
            | "MINUTE"
            | "HOUR"
            | "SECOND_MICROSECOND"
            | "MINUTE_MICROSECOND"
            | "HOUR_MICROSECOND"
            | "DAY_MICROSECOND"
            | "MINUTE_SECOND"
            | "HOUR_SECOND"
            | "DAY_SECOND"
            | "HOUR_MINUTE"
            | "DAY_MINUTE"
            | "DAY_HOUR"
    )
}

/// Returns whether the interval unit contains a calendar component.
pub fn native_is_date_unit(unit: &str) -> bool {
    matches!(
        unit.to_ascii_uppercase().as_str(),
        "DAY"
            | "WEEK"
            | "MONTH"
            | "QUARTER"
            | "YEAR"
            | "DAY_MICROSECOND"
            | "DAY_SECOND"
            | "DAY_MINUTE"
            | "DAY_HOUR"
            | "YEAR_MONTH"
    )
}

/// Returns whether the interval unit contains microseconds.
pub fn native_is_microsecond_unit(unit: &str) -> bool {
    matches!(
        unit.to_ascii_uppercase().as_str(),
        "MICROSECOND"
            | "SECOND_MICROSECOND"
            | "MINUTE_MICROSECOND"
            | "HOUR_MICROSECOND"
            | "DAY_MICROSECOND"
    )
}

/// Extracts date and composite date/clock units from the actual raw core.
/// No calendar, clock or FSP validation is applied; clock-only units remain
/// invalid here, even though their components are present in the raw core.
pub fn native_extract_datetime_num(raw: u64, unit: &str) -> Result<i64, NativeTimeError> {
    let [year, month, day, hour, minute, second, microsecond] = Time::native_core_fields(raw);
    let hour = i64::from(hour);
    let minute = i64::from(minute);
    let second = i64::from(second);
    let day = i64::from(day);
    let value = match unit.to_ascii_uppercase().as_str() {
        "DAY" => day,
        "WEEK" => i64::from(Time::native_core_week(raw, 0)),
        "MONTH" => i64::from(month),
        "QUARTER" => (i64::from(month) + 2) / 3,
        "YEAR" => i64::from(year),
        "DAY_MICROSECOND" => {
            (day * 1_000_000 + hour * 10_000 + minute * 100 + second) * 1_000_000
                + i64::from(microsecond)
        }
        "DAY_SECOND" => day * 1_000_000 + hour * 10_000 + minute * 100 + second,
        "DAY_MINUTE" => day * 10_000 + hour * 100 + minute,
        "DAY_HOUR" => day * 100 + hour,
        "YEAR_MONTH" => i64::from(year) * 100 + i64::from(month),
        _ => return Err(NativeTimeError::InvalidUnit(unit.to_owned())),
    };
    Ok(value)
}

/// Extracts absolute duration components and then applies the whole-value sign.
/// The full i64 nanosecond domain is accepted without duration construction,
/// range checks or FSP rounding.
pub fn native_extract_duration_num(nanos: i64, unit: &str) -> Result<i64, NativeTimeError> {
    let hour = i64::from(Duration::hours_from_nanos(nanos));
    let minute = i64::from(Duration::minutes_from_nanos(nanos));
    let second = i64::from(Duration::secs_from_nanos(nanos));
    let microsecond = i64::from(Duration::micro_secs_from_nanos(nanos));
    let mut value = match unit.to_ascii_uppercase().as_str() {
        "MICROSECOND" => microsecond,
        "SECOND" => second,
        "MINUTE" => minute,
        "HOUR" => hour,
        "SECOND_MICROSECOND" => second * 1_000_000 + microsecond,
        "MINUTE_MICROSECOND" => minute * 100_000_000 + second * 1_000_000 + microsecond,
        "MINUTE_SECOND" => minute * 100 + second,
        "HOUR_MICROSECOND" => {
            hour * 10_000_000_000 + minute * 100_000_000 + second * 1_000_000 + microsecond
        }
        "HOUR_SECOND" | "DAY_SECOND" => hour * 10_000 + minute * 100 + second,
        "HOUR_MINUTE" | "DAY_MINUTE" => hour * 100 + minute,
        "DAY_MICROSECOND" => (hour * 10_000 + minute * 100 + second) * 1_000_000 + microsecond,
        "DAY_HOUR" => hour,
        _ => return Err(NativeTimeError::InvalidUnit(unit.to_owned())),
    };
    if nanos < 0 {
        value = -value;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_extract_keeps_original_vectors_raw_fields_sign_and_unit_domains() {
        let raw = Time::native_core_from_fields(2019, 4, 12, 14, 0, 0, 0);
        for (unit, expected) in [
            ("day", 12),
            ("week", 14),
            ("MONTH", 4),
            ("QUARTER", 2),
            ("YEAR", 2019),
            ("DAY_MICROSECOND", 12_140_000_000_000),
            ("DAY_SECOND", 12_140_000),
            ("DAY_MINUTE", 121_400),
            ("DAY_HOUR", 1_214),
            ("YEAR_MONTH", 201_904),
        ] {
            assert_eq!(
                native_extract_datetime_num(raw, unit),
                Ok(expected),
                "{unit}"
            );
        }
        for unit in ["day", "week", "MONTH", "QUARTER", "YEAR"] {
            assert_eq!(native_extract_datetime_num(0, unit), Ok(0), "{unit}");
        }
        // Hidden microseconds and all raw out-of-calendar fields survive; low
        // reserved type/FSP bits neither validate nor normalize the projection.
        let raw = Time::native_core_from_fields(16383, 15, 31, 31, 63, 63, 1_048_575) | 15;
        assert_eq!(
            native_extract_datetime_num(raw, "DAY_MICROSECOND"),
            Ok(31_316_364_048_575)
        );
        assert_eq!(
            native_extract_datetime_num(raw, "YEAR_MONTH"),
            Ok(1_638_315)
        );
        assert_eq!(native_extract_datetime_num(raw, "QUARTER"), Ok(5));
        for (unit, positive, negative) in [
            ("MICROSECOND", 31_536, 0),
            ("SECOND", 0, -1),
            ("MINUTE", 0, -59),
            ("HOUR", 0, -10),
            ("SECOND_MICROSECOND", 31_536, -1_000_000),
            ("MINUTE_MICROSECOND", 31_536, -5_901_000_000),
            ("MINUTE_SECOND", 0, -5_901),
            ("HOUR_MICROSECOND", 31_536, -105_901_000_000),
            ("HOUR_SECOND", 0, -105_901),
            ("HOUR_MINUTE", 0, -1_059),
            ("DAY_MICROSECOND", 31_536, -105_901_000_000),
            ("DAY_SECOND", 0, -105_901),
            ("DAY_MINUTE", 0, -1_059),
            ("DAY_HOUR", 0, -10),
        ] {
            assert_eq!(
                native_extract_duration_num(31_536_000, unit),
                Ok(positive),
                "{unit}"
            );
            assert_eq!(
                native_extract_duration_num(-39_541_000_000_000, unit),
                Ok(negative),
                "{unit}"
            );
        }
        assert_eq!(
            native_extract_duration_num(i64::MIN, "microsecond"),
            Ok(-854_775)
        );
        assert_eq!(
            native_extract_duration_num(i64::MIN, "DAY_HOUR"),
            Ok(-2_562_047)
        );
        for unit in ["HoUr", " day", "DAY ", "tEsT_eRrOr"] {
            assert_eq!(
                native_extract_datetime_num(raw, unit),
                Err(NativeTimeError::InvalidUnit(unit.to_owned()))
            );
        }
        for unit in ["DaY", "YEAR_MONTH", " hour", "HOUR ", "tEsT_eRrOr"] {
            assert_eq!(
                native_extract_duration_num(0, unit),
                Err(NativeTimeError::InvalidUnit(unit.to_owned()))
            );
        }
        for (unit, clock, date, microsecond) in [
            ("microsecond", true, false, true),
            ("SECOND", true, false, false),
            ("MINUTE", true, false, false),
            ("HOUR", true, false, false),
            ("SECOND_MICROSECOND", true, false, true),
            ("MINUTE_MICROSECOND", true, false, true),
            ("HOUR_MICROSECOND", true, false, true),
            ("DAY_MICROSECOND", true, true, true),
            ("MINUTE_SECOND", true, false, false),
            ("HOUR_SECOND", true, false, false),
            ("DAY_SECOND", true, true, false),
            ("HOUR_MINUTE", true, false, false),
            ("DAY_MINUTE", true, true, false),
            ("DAY_HOUR", true, true, false),
            ("DAY", false, true, false),
            ("WEEK", false, true, false),
            ("MONTH", false, true, false),
            ("QUARTER", false, true, false),
            ("YEAR", false, true, false),
            ("year_month", false, true, false),
            (" DAY", false, false, false),
            ("DAY ", false, false, false),
            ("MİCROSECOND", false, false, false),
            ("", false, false, false),
        ] {
            assert_eq!(native_is_clock_unit(unit), clock, "{unit}");
            assert_eq!(native_is_date_unit(unit), date, "{unit}");
            assert_eq!(native_is_microsecond_unit(unit), microsecond, "{unit}");
        }
    }
}
