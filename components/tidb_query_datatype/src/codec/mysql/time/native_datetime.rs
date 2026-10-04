// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native CoreTime conversion through the caller's actual chrono timezone.

use std::fmt;

use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, LocalResult, NaiveDate, NaiveDateTime,
    TimeZone, Timelike,
};

use super::Time;

/// Failure converting a MySQL wall-clock value through an IANA timezone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeTimeConversionError {
    /// The calendar or clock fields are invalid.
    InvalidCalendar,
    /// The local time lies in a timezone transition gap.
    NonexistentLocalTime,
    /// No valid transition boundary exists within TiDB's four-hour limit.
    TransitionOutOfRange,
}

impl fmt::Display for NativeTimeConversionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidCalendar => "invalid calendar time",
            Self::NonexistentLocalTime => "nonexistent local time",
            Self::TransitionOutOfRange => "timezone transition exceeds four hours",
        })
    }
}

impl std::error::Error for NativeTimeConversionError {}

/// Validate the native raw calendar with chrono, without SQL DATE range checks.
pub fn native_core_naive_datetime(raw: u64) -> Result<NaiveDateTime, NativeTimeConversionError> {
    // The bit view reads every clock field without applying Time's kind/FSP.
    let core = Time(raw);
    let date = NaiveDate::from_ymd_opt(core.year() as i32, core.month(), core.day())
        .ok_or(NativeTimeConversionError::InvalidCalendar)?;
    date.and_hms_micro_opt(core.hour(), core.minute(), core.second(), core.micro())
        .ok_or(NativeTimeConversionError::InvalidCalendar)
}

/// Resolve the actual native wall clock, optionally adjusting a DST gap.
pub fn native_core_to_datetime<TZ: TimeZone>(
    raw: u64,
    timezone: &TZ,
    adjust_gap: bool,
) -> Result<DateTime<TZ>, NativeTimeConversionError> {
    resolve_local_datetime(timezone, native_core_naive_datetime(raw)?, adjust_gap)
}

/// Convert a timezone-aware value to native microsecond calendar bits.
pub fn native_core_from_datetime<TZ: TimeZone>(value: DateTime<TZ>) -> u64 {
    let value = value + ChronoDuration::nanoseconds(500);
    Time::native_core_from_fields(
        value.year() as u16,
        value.month() as u8,
        value.day() as u8,
        value.hour() as u8,
        value.minute() as u8,
        value.second() as u8,
        value.nanosecond() / 1_000,
    )
}

fn resolve_local_datetime<TZ: TimeZone>(
    timezone: &TZ,
    naive: NaiveDateTime,
    adjust_gap: bool,
) -> Result<DateTime<TZ>, NativeTimeConversionError> {
    match timezone.from_local_datetime(&naive) {
        LocalResult::Single(value) => Ok(value),
        // A wall-clock time the fall-back repeats has TWO instants, and which
        // one Go picks is neither "the earlier" nor "the later" -- see
        // [`resolve_repeated_local_datetime`].
        LocalResult::Ambiguous(_, later) => {
            Ok(resolve_repeated_local_datetime(timezone, naive).unwrap_or(later))
        }
        LocalResult::None if !adjust_gap => Err(NativeTimeConversionError::NonexistentLocalTime),
        LocalResult::None => {
            let transition_search = naive.with_nanosecond(0).expect("zero nanosecond is valid");
            for seconds in 1..=4 * 60 * 60 {
                let candidate = transition_search + ChronoDuration::seconds(seconds);
                match timezone.from_local_datetime(&candidate) {
                    LocalResult::Single(value) => return Ok(value),
                    LocalResult::Ambiguous(_, later) => {
                        return Ok(
                            resolve_repeated_local_datetime(timezone, candidate).unwrap_or(later)
                        );
                    }
                    LocalResult::None => {}
                }
            }
            Err(NativeTimeConversionError::TransitionOutOfRange)
        }
    }
}

/// The instant Go's `time.Date` picks for a wall-clock time the daylight-saving
/// fall-back REPEATS.
///
/// The two candidates are real instants an hour apart, and Go picks neither
/// "the earlier" nor "the later" as a rule -- captured from TiDB, the same
/// question answers differently in two zones:
///
/// ```text
/// SET time_zone='America/Los_Angeles'; INSERT ... '2021-11-07 01:30:00'
///   read at +00:00 -> 2021-11-07 08:30:00     the EARLIER instant (PDT, -7)
/// SET time_zone='Europe/London';       INSERT ... '2021-10-31 01:30:00'
///   read at +00:00 -> 2021-10-31 01:30:00     the LATER instant (GMT, +0)
/// ```
///
/// The rule that produces both is `time.Date` itself (Go `src/time/time.go`):
/// it reads the wall clock AS IF it were already UTC, looks up the offset in
/// force at that instant, subtracts it, and only re-looks-up when the result
/// lands outside the zone period it started in.
///
/// ```go
/// unix := ...                                  // the local clock read as UTC
/// _, offset, start, end, _ := loc.lookup(unix)
/// if offset != 0 {
///     switch utc := unix - int64(offset); {
///     case utc < start: _, offset, _, _, _ = loc.lookup(start - 1)
///     case utc >= end:  _, offset, _, _, _ = loc.lookup(end)
///     }
///     unix -= int64(offset)
/// }
/// ```
///
/// London takes the `offset != 0` guard's false branch -- the offset in force
/// at `2021-10-31 01:30 UTC` is GMT's zero -- and keeps the wall clock as the
/// instant, which is the later of the two. Los Angeles is eight hours behind,
/// so reading `2021-11-07 01:30` as UTC lands well before the 09:00 UTC
/// transition, and the still-in-force PDT offset gives the earlier instant.
///
/// `None` when the instant is outside the representable range; the caller
/// falls back to `chrono`'s later candidate rather than failing a conversion
/// over a value that has two valid answers.
fn resolve_repeated_local_datetime<TZ: TimeZone>(
    timezone: &TZ,
    naive: NaiveDateTime,
) -> Option<DateTime<TZ>> {
    let offset_at = |seconds: i64| -> Option<i64> {
        let instant = DateTime::from_timestamp(seconds, 0)?;
        Some(i64::from(
            chrono::Offset::fix(&timezone.offset_from_utc_datetime(&instant.naive_utc()))
                .local_minus_utc(),
        ))
    };
    let unix = naive.and_utc().timestamp();
    let mut offset = offset_at(unix)?;
    if offset != 0 {
        let utc = unix - offset;
        // Go compares `utc` against the bounds of the period `unix` sits in.
        // The two are at most one zone offset apart, so a bound can only fall
        // BETWEEN them -- and if none does, `utc` is inside the period and Go
        // keeps the offset it already has.
        let (low, high) = if utc < unix { (utc, unix) } else { (unix, utc) };
        if offset_at(low)? != offset_at(high)? {
            // The first instant of the later of the two periods, which is
            // Go's `start` when it lies above `utc` and its `end` when below.
            let (mut before, mut after) = (low, high);
            while after - before > 1 {
                let middle = before + (after - before) / 2;
                if offset_at(middle)? == offset_at(high)? {
                    after = middle;
                } else {
                    before = middle;
                }
            }
            offset = if utc < unix {
                offset_at(after - 1)?
            } else {
                offset_at(after)?
            };
        }
    }
    let instant = DateTime::from_timestamp(unix - offset, naive.nanosecond())?;
    Some(instant.with_timezone(timezone))
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, Utc};

    use super::*;

    #[test]
    fn native_core_conversion_keeps_zone_transitions_rounding_and_raw_domain() {
        let pack = Time::native_core_from_fields;
        let raw = pack(2021, 1, 2, 3, 4, 5, 123_456);
        let utc = native_core_to_datetime(raw | 15, &Utc, false).unwrap();
        assert_eq!(
            (utc.year(), utc.month(), utc.day(), utc.hour()),
            (2021, 1, 2, 3)
        );
        assert_eq!(
            (utc.minute(), utc.second(), utc.nanosecond()),
            (4, 5, 123_456_000)
        );
        assert_eq!(native_core_from_datetime(utc), raw);
        let fixed = FixedOffset::east_opt(8 * 3600).unwrap();
        let local = native_core_to_datetime(raw, &fixed, false).unwrap();
        assert_eq!(local.timestamp(), utc.timestamp() - 8 * 3600);
        assert_eq!(native_core_from_datetime(local), raw);

        let la = chrono_tz::America::Los_Angeles;
        let repeated = native_core_to_datetime(pack(2021, 11, 7, 1, 30, 0, 42), &la, false)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            (repeated.hour(), repeated.minute(), repeated.nanosecond()),
            (8, 30, 42_000)
        );
        let london = native_core_to_datetime(
            pack(2021, 10, 31, 1, 30, 0, 0),
            &chrono_tz::Europe::London,
            false,
        )
        .unwrap()
        .with_timezone(&Utc);
        assert_eq!((london.hour(), london.minute()), (1, 30));
        let gap = pack(2021, 3, 14, 2, 30, 0, 123_456);
        assert_eq!(
            native_core_to_datetime(gap, &la, false).unwrap_err(),
            NativeTimeConversionError::NonexistentLocalTime
        );
        let adjusted = native_core_to_datetime(gap, &la, true).unwrap();
        assert_eq!(
            (
                adjusted.hour(),
                adjusted.minute(),
                adjusted.second(),
                adjusted.nanosecond()
            ),
            (3, 0, 0, 0)
        );
        assert_eq!(
            native_core_to_datetime(
                pack(2011, 12, 30, 12, 0, 0, 0),
                &chrono_tz::Pacific::Apia,
                true,
            )
            .unwrap_err(),
            NativeTimeConversionError::TransitionOutOfRange
        );
        for raw in [
            0,
            pack(2021, 2, 29, 0, 0, 0, 0),
            pack(2021, 1, 1, 31, 0, 0, 0),
            pack(2021, 1, 1, 0, 0, 58, 1_000_000),
        ] {
            assert_eq!(
                native_core_naive_datetime(raw).unwrap_err(),
                NativeTimeConversionError::InvalidCalendar
            );
        }
        // Chrono admits wide years and the stored leap-second microsecond field.
        for year in [0, 16_383] {
            assert_eq!(
                native_core_naive_datetime(pack(year, 1, 1, 0, 0, 0, 0))
                    .unwrap()
                    .year(),
                i32::from(year)
            );
        }
        let leap = native_core_naive_datetime(pack(2021, 1, 1, 0, 0, 59, 1_000_000)).unwrap();
        assert_eq!((leap.second(), leap.nanosecond()), (59, 1_000_000_000));
        let midnight = Utc.with_ymd_and_hms(2021, 1, 1, 0, 0, 0).single().unwrap();
        for (nanos, micros) in [(123_456_499, 123_456), (123_456_500, 123_457)] {
            assert_eq!(
                native_core_from_datetime(midnight.with_nanosecond(nanos).unwrap()),
                pack(2021, 1, 1, 0, 0, 0, micros)
            );
        }
        let carry = Utc
            .with_ymd_and_hms(2020, 12, 31, 23, 59, 59)
            .single()
            .unwrap()
            .with_nanosecond(999_999_500)
            .unwrap();
        assert_eq!(
            native_core_from_datetime(carry),
            (2021u64 << 50) | (1 << 46) | (1 << 41)
        );
        let wide = Utc
            .with_ymd_and_hms(20_000, 1, 2, 0, 0, 0)
            .single()
            .unwrap();
        assert_eq!(
            native_core_from_datetime(wide),
            (3616u64 << 50) | (1 << 46) | (2 << 41)
        );
        let negative = Utc.with_ymd_and_hms(-1, 1, 2, 0, 0, 0).single().unwrap();
        assert_eq!(
            native_core_from_datetime(negative),
            (16_383u64 << 50) | (1 << 46) | (2 << 41)
        );
        assert!(
            std::panic::catch_unwind(|| native_core_from_datetime(DateTime::<Utc>::MAX_UTC))
                .is_err()
        );
    }
}
