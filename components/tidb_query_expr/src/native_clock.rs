// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native statement-clock formatting policies. These consume the original
//! clock tuple, not a timezone-adjusted or already-rounded host answer.

use tidb_query_datatype::codec::mysql::Time;

/// The actual statement clock fields, without validation or normalization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeClockInput {
    pub utc_secs: i64,
    pub nanos: u32,
    pub tz_offset: i32,
}

/// Render an epoch second using the native wide Gregorian calendar policy.
pub fn native_format_clock_date(secs: i64) -> String {
    let (y, m, d) = Time::native_civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

fn format_clock_hms(secs: i64) -> String {
    let secs_of_day = secs.rem_euclid(86_400);
    let (hour, minute, second) = (
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60,
    );
    format!("{hour:02}:{minute:02}:{second:02}")
}

fn clock_fraction_suffix(nanos: u32, fsp: u32) -> String {
    if fsp == 0 {
        return String::new();
    }
    let fraction = nanos / 10u32.pow(9 - fsp);
    format!(".{fraction:0width$}", width = fsp as usize)
}

fn round_clock_nanos(nanos: u32, fsp: u32) -> (i64, u32) {
    let scale = 10u32.pow(9 - fsp);
    let half_up = nanos + scale / 2;
    if half_up >= 1_000_000_000 {
        (1, 0)
    } else {
        (0, (half_up / scale) * scale)
    }
}

/// The original native datetime formatter, also shared by callers that have
/// not migrated their evaluation boundary. FSP is checked by those callers;
/// arithmetic and raw nanosecond behavior remain deliberately unchanged.
pub fn native_format_clock_datetime(secs: i64, nanos: u32, fsp: u32, round: bool) -> String {
    let (carry, nanos) = if round {
        round_clock_nanos(nanos, fsp)
    } else {
        (0, nanos)
    };
    let secs = secs + carry;
    format!(
        "{} {}{}",
        native_format_clock_date(secs),
        format_clock_hms(secs),
        clock_fraction_suffix(nanos, fsp)
    )
}

fn format_clock_time_only(secs: i64, nanos: u32, fsp: u32, round: bool) -> String {
    let (carry, nanos) = if round {
        round_clock_nanos(nanos, fsp)
    } else {
        (0, nanos)
    };
    let secs = secs + carry;
    format!(
        "{}{}",
        format_clock_hms(secs),
        clock_fraction_suffix(nanos, fsp)
    )
}

/// UTC_DATE ignores both the fractional field and the session offset.
pub fn native_utc_date(clock: NativeClockInput) -> String {
    native_format_clock_date(clock.utc_secs)
}

/// UTC_TIMESTAMP rounds the original nanoseconds directly, at every arity.
pub fn native_utc_timestamp(clock: NativeClockInput, fsp: u32) -> String {
    native_format_clock_datetime(clock.utc_secs, clock.nanos, fsp, true)
}

/// Zero-argument CURTIME/CURRENT_TIME truncates in the statement's local zone.
pub fn native_current_time_without_fsp(clock: NativeClockInput) -> String {
    format_clock_time_only(
        clock.utc_secs + i64::from(clock.tz_offset),
        clock.nanos,
        0,
        false,
    )
}

/// Explicit CURTIME/CURRENT_TIME first truncates to microseconds, then rounds.
pub fn native_current_time_with_fsp(clock: NativeClockInput, fsp: u32) -> String {
    let nanos = clock.nanos / 1_000 * 1_000;
    format_clock_time_only(
        clock.utc_secs + i64::from(clock.tz_offset),
        nanos,
        fsp,
        true,
    )
}

/// Zero-argument UTC_TIME truncates without applying the session offset.
pub fn native_utc_time_without_fsp(clock: NativeClockInput) -> String {
    format_clock_time_only(clock.utc_secs, clock.nanos, 0, false)
}

/// Explicit UTC_TIME preserves the microsecond-truncation-before-rounding step.
pub fn native_utc_time_with_fsp(clock: NativeClockInput, fsp: u32) -> String {
    let nanos = clock.nanos / 1_000 * 1_000;
    format_clock_time_only(clock.utc_secs, nanos, fsp, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_clock_fixed_family_literal_policies() {
        let clock = NativeClockInput {
            utc_secs: 1_700_000_000,
            nanos: 654_320_955,
            tz_offset: 19_800,
        };
        assert_eq!(native_utc_date(clock), "2023-11-14");
        assert_eq!(native_utc_timestamp(clock, 0), "2023-11-14 22:13:21");
        assert_eq!(native_utc_timestamp(clock, 6), "2023-11-14 22:13:20.654321");
        assert_eq!(native_current_time_without_fsp(clock), "03:43:20");
        assert_eq!(native_current_time_with_fsp(clock, 0), "03:43:21");
        assert_eq!(native_current_time_with_fsp(clock, 6), "03:43:20.654320");
        assert_eq!(native_utc_time_without_fsp(clock), "22:13:20");
        assert_eq!(native_utc_time_with_fsp(clock, 0), "22:13:21");
        assert_eq!(native_utc_time_with_fsp(clock, 6), "22:13:20.654320");
        // Thin native NOW/CURDATE/SYSDATE aliases use these same pure bodies,
        // without claiming that those callers have migrated their roots.
        assert_eq!(native_format_clock_date(1_700_019_800), "2023-11-15");
        assert_eq!(
            native_format_clock_datetime(1_700_019_800, 654_320_955, 6, false),
            "2023-11-15 03:43:20.654320"
        );
        assert_eq!(
            native_format_clock_datetime(1_700_019_800, 654_320_955, 6, true),
            "2023-11-15 03:43:20.654321"
        );
    }

    #[test]
    fn native_clock_epoch_carry_and_raw_fields() {
        let clock = NativeClockInput {
            utc_secs: -1,
            nanos: 999_999_500,
            tz_offset: 1,
        };
        assert_eq!(native_utc_date(clock), "1969-12-31");
        assert_eq!(native_utc_timestamp(clock, 6), "1970-01-01 00:00:00.000000");
        assert_eq!(native_utc_time_without_fsp(clock), "23:59:59");
        assert_eq!(native_utc_time_with_fsp(clock, 0), "00:00:00");
        assert_eq!(native_utc_time_with_fsp(clock, 6), "23:59:59.999999");
        assert_eq!(native_current_time_without_fsp(clock), "00:00:00");
        assert_eq!(native_current_time_with_fsp(clock, 6), "00:00:00.999999");
        let raw = NativeClockInput {
            utc_secs: 0,
            nanos: u32::MAX,
            tz_offset: i32::MIN,
        };
        assert_eq!(native_utc_date(raw), "1970-01-01");
        assert_eq!(native_utc_time_without_fsp(raw), "00:00:00");
        assert_eq!(
            native_format_clock_datetime(0, 1_234_567_890, 6, false),
            "1970-01-01 00:00:00.1234567"
        );
        assert_eq!(
            native_utc_timestamp(
                NativeClockInput {
                    nanos: 1_500_000_000,
                    ..raw
                },
                0
            ),
            "1970-01-01 00:00:01"
        );
        assert_eq!(native_format_clock_date(-62_167_219_200), "0000-01-01");
        assert_eq!(native_format_clock_date(253_402_300_800), "10000-01-01");
    }
}
