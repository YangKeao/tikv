// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Pure clock operations for the native GetTimeValue SDK and TSO projection.
//!
//! In the existing GetTimeValue SDK only, UTC validation is deliberately
//! separate from local calendar projection: the caller validates UTC before
//! demanding its second session-zone getter. Its native Time constructor keeps
//! validating every clock field before date-only output.
//!
//! TSO has different source ordering: after positive-TSO admission the caller
//! reads its original time_zone getter first, then uses shared UTC only to
//! query the named/local offset. The worker recomputes UTC and local core from
//! actual TSO/offset operands, never from prepared host calendar fields.

use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};
use tidb_query_datatype::codec::mysql::Time;

/// Validates the actual statement-clock instant, without demanding a zone.
/// Neither the clock tuple's offset nor GetTimeValue's explicit parse zone is
/// used by this SDK path.
pub fn native_typed_clock_utc(seconds: i64, nanos: u32) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp(seconds, nanos)
}

/// Resolves the physical milliseconds of an actual positive TSO. Callers may
/// use this instant to discover a named/local zone's offset, not as worker
/// data.
pub fn native_tso_utc(tso: i64) -> Option<DateTime<Utc>> {
    if tso <= 0 {
        return None;
    }
    let physical_ms = tso >> 18;
    let seconds = physical_ms.div_euclid(1000);
    let micros = (physical_ms.rem_euclid(1000) * 1000) as u32;
    native_typed_clock_utc(seconds, micros * 1000)
}

/// Recomputes the local native core from the original TSO and raw zone offset.
/// The original fixed-zone path adds seconds directly, including offsets that
/// FixedOffset rejects. Positive i64 TSO milliseconds plus any i32 offset stay
/// within approximately years 1901..3153 and ordinary microseconds 0..999000:
/// every resulting field fits the original checked native constructor's width.
pub fn native_tso_core(tso: i64, raw_offset: i32) -> Option<u64> {
    let local =
        (native_tso_utc(tso)? + chrono::Duration::seconds(i64::from(raw_offset))).naive_utc();
    Some(Time::native_core_from_fields(
        local.year() as u16,
        local.month() as u8,
        local.day() as u8,
        local.hour() as u8,
        local.minute() as u8,
        local.second() as u8,
        local.and_utc().timestamp_subsec_micros(),
    ))
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
    Time::native_date_fields(raw_core)
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;

    use super::*;

    #[test]
    fn tso_keeps_physical_milliseconds_and_unrestricted_raw_offset() {
        assert!(native_tso_utc(0).is_none());
        assert!(native_tso_utc(i64::MIN).is_none());
        assert!(native_tso_core(-1, 0).is_none());
        assert_eq!(native_tso_utc(1).unwrap().timestamp(), 0);
        assert_eq!(native_tso_core(1, 0), Some(0x1ec8_4200_0000_0000));
        assert_eq!(native_tso_core((1 << 18) - 1, 0), native_tso_core(1, 0));
        assert_eq!(native_tso_core(1 << 18, 0), Some(0x1ec8_4200_0000_3e80));
        assert_eq!(
            Time::native_date_fields(native_tso_core(1, 86_400).unwrap()),
            [1970, 1, 2, 0, 0, 0, 0],
        );
        assert_eq!(
            Time::native_date_fields(native_tso_core(1, -86_400).unwrap()),
            [1969, 12, 31, 0, 0, 0, 0],
        );
        assert!(native_tso_core(1, i32::MIN).is_some());
        assert!(native_tso_core(i64::MAX, i32::MAX).is_some());
        let instant = native_tso_utc(424_930_234_047_906_595).unwrap();
        assert_eq!(instant.timestamp_subsec_micros(), 903_000);
    }

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
