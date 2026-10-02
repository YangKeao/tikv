// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Pure clock operations for the native public GetTimeValue SDK.
//!
//! UTC validation is deliberately separate from local calendar projection:
//! the caller validates UTC before demanding its second session-zone getter.
//! The native Time constructor remains the unchanged representation/bit-width
//! codec, including its validation of all clock fields before date-only output.

use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};
use tidb_query_datatype::codec::mysql::Time;

/// Validates the actual statement-clock instant, without demanding a zone.
/// Neither the clock tuple's offset nor GetTimeValue's explicit parse zone is
/// used by this SDK path.
pub fn native_typed_clock_utc(seconds: i64, nanos: u32) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp(seconds, nanos)
}

/// Computes the actual local fields using the caller's session zone, and
/// truncates fractional seconds exactly as GetTimeValue does.
///
/// Returns `[year, month, day, hour, minute, second, microsecond]`. The caller
/// passes these fields to its original checked Time representation constructor.
/// In particular, a Chrono leap second can produce microseconds exceeding that
/// constructor's bit width; do not normalize, clamp, or clear them here.
///
/// `normalized_fsp` is the result of the caller's existing `check_fsp`, hence
/// is in 0..=6. That check must remain before the statement-clock getter.
/// Generic TimeZone accepts the real native session-zone implementation,
/// without translating named zones across different chrono-tz versions.
pub fn native_typed_clock_fields<TZ: TimeZone>(
    instant: DateTime<Utc>,
    zone: &TZ,
    normalized_fsp: i64,
) -> [i32; 7] {
    let instant = instant.with_timezone(zone);
    let quantum = 10_u32.pow((9 - normalized_fsp) as u32);
    let nanos = (instant.nanosecond() / quantum) * quantum;
    [
        instant.year(),
        instant.month() as i32,
        instant.day() as i32,
        instant.hour() as i32,
        instant.minute() as i32,
        instant.second() as i32,
        (nanos / 1_000) as i32,
    ]
}

/// Produces date-only fields from a fully constructed/validated current Time's
/// stored CoreTime bits, using the existing shared field projections. The
/// caller must first complete current-time construction so clearing the clock
/// cannot erase an earlier representation error, then construct the date result
/// using its original kind and original FSP.
pub fn native_typed_date_fields(raw_core: u64) -> [i32; 7] {
    [
        Time::year_from_core_bits(raw_core) as i32,
        Time::month_from_core_bits(raw_core) as i32,
        Time::day_from_core_bits(raw_core) as i32,
        0,
        0,
        0,
        0,
    ]
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;

    use super::*;

    #[test]
    fn typed_clock_uses_actual_zone_and_truncates_without_rounding() {
        let instant = native_typed_clock_utc(0, 999_999_999).unwrap();
        let east = FixedOffset::east_opt(5 * 3600 + 45 * 60).unwrap();
        for (fsp, micros) in [(0, 0), (3, 999_000), (6, 999_999)] {
            assert_eq!(
                native_typed_clock_fields(instant, &east, fsp),
                [1970, 1, 1, 5, 45, 0, micros]
            );
        }
        let west = FixedOffset::west_opt(3600).unwrap();
        assert_eq!(
            native_typed_clock_fields(instant, &west, 6),
            [1969, 12, 31, 23, 0, 0, 999_999]
        );
        assert_eq!(
            native_typed_date_fields(u64::MAX),
            [16383, 15, 31, 0, 0, 0, 0]
        );
        assert_eq!(native_typed_date_fields(0), [0; 7]);
    }

    #[test]
    fn utc_admission_and_leap_microseconds_precede_date_projection() {
        assert!(native_typed_clock_utc(i64::MAX, 0).is_none());
        assert!(native_typed_clock_utc(0, 1_000_000_000).is_none());
        let leap = native_typed_clock_utc(59, 1_500_123_456).unwrap();
        let fields = native_typed_clock_fields(leap, &Utc, 6);
        assert_eq!(fields, [1970, 1, 1, 0, 0, 59, 1_500_123]);
        assert!(fields[6] >= 1 << 20);
        let wide_year = Utc.with_ymd_and_hms(20000, 1, 1, 0, 0, 0).single().unwrap();
        assert_eq!(
            native_typed_clock_fields(wide_year, &Utc, 6),
            [20000, 1, 1, 0, 0, 0, 0]
        );
        // This helper intentionally retains the oversized field for the
        // unchanged native Time codec to reject. Its rejection, and the
        // getter ordering around it, belong to the caller's integration tests.
    }
}
