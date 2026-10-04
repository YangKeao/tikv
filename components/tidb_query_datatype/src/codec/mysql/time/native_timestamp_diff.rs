// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! The distinct native text/civil and raw-core/Go-day-number policies. Existing
//! wire TIMESTAMPDIFF keeps its own unit validation and arithmetic unchanged.

use super::Time;

/// Absolute seconds/microseconds plus the sign of a temporal difference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeDifference {
    /// Absolute whole seconds.
    pub seconds: i64,
    /// Absolute remaining microseconds.
    pub microseconds: i32,
    /// Whether the source difference was negative.
    pub negative: bool,
}

/// Shared alias retaining the original `TimeDifference` derived Debug spelling.
pub type NativeTimeDifference = TimeDifference;

/// Units accepted by MySQL TIMESTAMPDIFF, in the original native enum order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeTimestampInterval {
    /// Calendar years.
    Year,
    /// Calendar quarters.
    Quarter,
    /// Calendar months.
    Month,
    /// Seven-day weeks.
    Week,
    /// Days.
    Day,
    /// Hours.
    Hour,
    /// Minutes.
    Minute,
    /// Seconds.
    Second,
    /// Microseconds.
    Microsecond,
}

impl NativeTimestampInterval {
    /// Exact legacy/unit lookup: no UTF-8 requirement, case folding or
    /// trimming.
    pub fn from_uppercase_bytes(unit: &[u8]) -> Option<Self> {
        Some(match unit {
            b"YEAR" => Self::Year,
            b"QUARTER" => Self::Quarter,
            b"MONTH" => Self::Month,
            b"WEEK" => Self::Week,
            b"DAY" => Self::Day,
            b"HOUR" => Self::Hour,
            b"MINUTE" => Self::Minute,
            b"SECOND" => Self::Second,
            b"MICROSECOND" => Self::Microsecond,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy)]
struct TimestampDiffDateTime {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    microsecond: u32,
}

fn parse_timestamp_diff_datetime(input: &str) -> Option<TimestampDiffDateTime> {
    let (year, month, day, hour, minute, second, microsecond) =
        Time::parse_native_datetime_components(input)?;
    Some(TimestampDiffDateTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
        microsecond,
    })
}

impl Time {
    /// Native CoreTime time difference against the original signed field
    /// domain. Deliberately retain i32 day arithmetic, ordinary i64
    /// microsecond arithmetic and unary negation, including their original
    /// overflow/panic behavior.
    #[allow(clippy::too_many_arguments)]
    pub fn native_core_time_diff(
        left: u64,
        year: i32,
        month: i32,
        day: i32,
        hour: i32,
        minute: i32,
        second: i32,
        microsecond: i32,
        sign: i32,
    ) -> NativeTimeDifference {
        let [ly, lm, ld, lh, lmi, ls, lus] = Self::native_core_fields(left);
        let days = Self::native_calc_daynr_i32(ly, lm, ld)
            - sign * Self::native_calc_daynr_i32(year, month, day);
        let mut micros = (i64::from(days) * 86_400
            + i64::from(lh) * 3_600
            + i64::from(lmi) * 60
            + i64::from(ls)
            - i64::from(sign)
                * (i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second)))
            * 1_000_000
            + i64::from(lus)
            - i64::from(sign) * i64::from(microsecond);
        let negative = micros < 0;
        if negative {
            micros = -micros;
        }
        NativeTimeDifference {
            seconds: micros / 1_000_000,
            microseconds: (micros % 1_000_000) as i32,
            negative,
        }
    }

    /// Native raw-core TIMESTAMPDIFF. This is not the wire i64-month worker:
    /// source u8 month subtraction and u32 years/months remain intentional.
    pub fn native_core_timestamp_diff(
        start: u64,
        end: u64,
        interval: NativeTimestampInterval,
    ) -> i64 {
        let start = Self::native_core_fields(start);
        let finish = Self::native_core_fields(end);
        let difference = Self::native_core_time_diff(
            end, start[0], start[1], start[2], start[3], start[4], start[5], start[6], 1,
        );
        let mut months = 0_u32;
        if matches!(
            interval,
            NativeTimestampInterval::Year
                | NativeTimestampInterval::Quarter
                | NativeTimestampInterval::Month
        ) {
            let (begin, finish) = if difference.negative {
                (finish, start)
            } else {
                (start, finish)
            };
            let mut years = (finish[0] - begin[0]) as u32;
            if finish[1] < begin[1] || (finish[1] == begin[1] && finish[2] < begin[2]) {
                years -= 1;
            }
            months = 12 * years;
            if finish[1] < begin[1] || (finish[1] == begin[1] && finish[2] < begin[2]) {
                months += 12 - u32::from(begin[1] as u8 - finish[1] as u8);
            } else {
                months += u32::from(finish[1] as u8 - begin[1] as u8);
            }
            let begin_seconds = u32::from(begin[3] as u8) * 3_600
                + u32::from(begin[4] as u8) * 60
                + u32::from(begin[5] as u8);
            let finish_seconds = u32::from(finish[3] as u8) * 3_600
                + u32::from(finish[4] as u8) * 60
                + u32::from(finish[5] as u8);
            if finish[2] < begin[2]
                || (finish[2] == begin[2]
                    && (finish_seconds < begin_seconds
                        || (finish_seconds == begin_seconds && finish[6] < begin[6])))
            {
                months -= 1;
            }
        }
        let sign = if difference.negative { -1 } else { 1 };
        let value = match interval {
            NativeTimestampInterval::Year => i64::from(months / 12),
            NativeTimestampInterval::Quarter => i64::from(months / 3),
            NativeTimestampInterval::Month => i64::from(months),
            NativeTimestampInterval::Week => difference.seconds / 86_400 / 7,
            NativeTimestampInterval::Day => difference.seconds / 86_400,
            NativeTimestampInterval::Hour => difference.seconds / 3_600,
            NativeTimestampInterval::Minute => difference.seconds / 60,
            NativeTimestampInterval::Second => difference.seconds,
            NativeTimestampInterval::Microsecond => {
                difference.seconds * 1_000_000 + i64::from(difference.microseconds)
            }
        };
        value * sign
    }

    /// Ordinary native text TIMESTAMPDIFF: strict wide-year parsing, civil
    /// days, signed month arithmetic, ASCII case folding without trimming
    /// the unit, and unknown-unit zero only AFTER the original delta
    /// computation.
    pub fn native_timestamp_diff_text(unit: &str, left: &str, right: &str) -> Option<i64> {
        let left = parse_timestamp_diff_datetime(left)?;
        let right = parse_timestamp_diff_datetime(right)?;
        let left_day = Self::native_days_from_civil(left.year, left.month, left.day);
        let right_day = Self::native_days_from_civil(right.year, right.month, right.day);
        let left_clock = i64::from(left.hour) * 3_600_000_000
            + i64::from(left.minute) * 60_000_000
            + i64::from(left.second) * 1_000_000
            + i64::from(left.microsecond);
        let right_clock = i64::from(right.hour) * 3_600_000_000
            + i64::from(right.minute) * 60_000_000
            + i64::from(right.second) * 1_000_000
            + i64::from(right.microsecond);
        let delta = (right_day - left_day) * 86_400_000_000 + right_clock - left_clock;
        let negative = delta < 0;
        let absolute = delta.unsigned_abs();
        let seconds = absolute / 1_000_000;
        let microseconds = absolute % 1_000_000;
        let sign = if negative { -1 } else { 1 };
        let (begin, end) = if negative {
            (right, left)
        } else {
            (left, right)
        };
        // Lookup stays after delta: unknown text units must not bypass its
        // original wide-year arithmetic (unlike the raw-core profile).
        let interval =
            NativeTimestampInterval::from_uppercase_bytes(unit.to_ascii_uppercase().as_bytes());
        let months = if matches!(
            interval,
            Some(
                NativeTimestampInterval::Year
                    | NativeTimestampInterval::Quarter
                    | NativeTimestampInterval::Month
            )
        ) {
            let mut years = end.year - begin.year;
            let date_before =
                end.month < begin.month || (end.month == begin.month && end.day < begin.day);
            if date_before {
                years -= 1;
            }
            let mut months = 12 * years;
            if date_before {
                months += 12 - (i64::from(begin.month) - i64::from(end.month));
            } else {
                months += i64::from(end.month) - i64::from(begin.month);
            }
            if end.day < begin.day
                || (end.day == begin.day
                    && (end.hour * 3_600 + end.minute * 60 + end.second
                        < begin.hour * 3_600 + begin.minute * 60 + begin.second
                        || (end.hour * 3_600 + end.minute * 60 + end.second
                            == begin.hour * 3_600 + begin.minute * 60 + begin.second
                            && end.microsecond < begin.microsecond)))
            {
                months -= 1;
            }
            months
        } else {
            0
        };
        Some(match interval {
            Some(NativeTimestampInterval::Year) => months / 12 * sign,
            Some(NativeTimestampInterval::Quarter) => months / 3 * sign,
            Some(NativeTimestampInterval::Month) => months * sign,
            Some(NativeTimestampInterval::Week) => (seconds / 86_400 / 7) as i64 * sign,
            Some(NativeTimestampInterval::Day) => (seconds / 86_400) as i64 * sign,
            Some(NativeTimestampInterval::Hour) => (seconds / 3_600) as i64 * sign,
            Some(NativeTimestampInterval::Minute) => (seconds / 60) as i64 * sign,
            Some(NativeTimestampInterval::Second) => seconds as i64 * sign,
            Some(NativeTimestampInterval::Microsecond) => {
                (seconds * 1_000_000 + microseconds) as i64 * sign
            }
            None => 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_timestamp_diff_retains_distinct_text_and_core_domains() {
        assert_eq!(
            format!(
                "{:?}",
                NativeTimeDifference {
                    seconds: 1,
                    microseconds: 200,
                    negative: true
                }
            ),
            "TimeDifference { seconds: 1, microseconds: 200, negative: true }"
        );
        let raw = Time::native_core_from_fields;
        let jan = raw(0, 1, 1, 0, 0, 0, 0);
        let mar = raw(0, 3, 1, 0, 0, 0, 0);
        assert_eq!(
            Time::native_timestamp_diff_text("day", "0000-01-01", "0000-03-01"),
            Some(60)
        );
        assert_eq!(
            Time::native_core_timestamp_diff(jan | 15, mar | 1, NativeTimestampInterval::Day),
            59
        );
        assert_eq!(
            Time::native_timestamp_diff_text("MONTH", "2000-01-31", "2000-02-29"),
            Some(0)
        );
        assert_eq!(
            Time::native_core_timestamp_diff(
                raw(2000, 1, 31, 0, 0, 0, 0),
                raw(2000, 2, 29, 0, 0, 0, 0),
                NativeTimestampInterval::Month
            ),
            0
        );
        assert_eq!(
            Time::native_timestamp_diff_text(
                "MICROSECOND",
                "2000-01-01 00:00:01.000500",
                "2000-01-01 00:00:00.000700"
            ),
            Some(-999_800)
        );
        assert_eq!(
            Time::native_timestamp_diff_text("DAY", "4294967295-01-01", "4294967295-01-02"),
            Some(1)
        );
        assert_eq!(
            Time::native_timestamp_diff_text(" DAY", "2000-01-01", "2000-01-02"),
            Some(0)
        );
        assert_eq!(
            Time::native_timestamp_diff_text("unknown", "bad", "2000-01-02"),
            None
        );
        assert_eq!(
            Time::native_core_time_diff(
                raw(2020, 1, 1, 0, 0, 1, 500) | 15,
                2020,
                1,
                1,
                0,
                0,
                2,
                700,
                1
            ),
            NativeTimeDifference {
                seconds: 1,
                microseconds: 200,
                negative: true
            }
        );
        #[cfg(debug_assertions)]
        {
            // Unknown text units do not bypass original wide delta overflow.
            assert!(
                std::panic::catch_unwind(|| Time::native_timestamp_diff_text(
                    "unknown",
                    "0000-01-01",
                    "4294967295-01-01"
                ))
                .is_err()
            );
            // Raw hour 31 can reverse day ordering within one year: retain the
            // native u32 year decrement panic, not a signed-month replacement.
            assert!(
                std::panic::catch_unwind(|| Time::native_core_timestamp_diff(
                    raw(2020, 1, 2, 0, 0, 0, 0),
                    raw(2020, 1, 1, 31, 0, 0, 0),
                    NativeTimestampInterval::Month,
                ))
                .is_err()
            );
        }
    }
}
