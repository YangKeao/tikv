// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Actual calendar operands and consuming week continuation. Wire adapters and
//! native sources share these kernels; no serialization or Ready invocation.
use tidb_query_datatype::codec::mysql::{Duration, Time};
/// Closed calendar policy, preserving the original distinction between core
/// fields, SQL text and legacy nanoseconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCalendarOperation {
    /// Native DATE policy.
    Date,
    /// Native YEAR policy.
    Year,
    /// Native MONTH policy.
    Month,
    /// Native DAY policy.
    Day,
    /// Native QUARTER policy.
    Quarter,
    /// Native HOUR policy.
    Hour,
    /// Native MINUTE policy.
    Minute,
    /// Native SECOND policy.
    Second,
    /// Native DAYOFWEEK policy.
    DayOfWeek,
    /// Native DAYOFYEAR policy.
    DayOfYear,
    /// Native WEEKDAY policy.
    Weekday,
    /// Native MONTHNAME policy.
    MonthName,
    /// Native DAYNAME policy.
    DayName,
    /// Native WEEKOFYEAR policy.
    WeekOfYear,
    /// Native WEEK policy.
    Week,
    /// Native YEARWEEK policy.
    YearWeek,
    /// Native LAST_DAY policy.
    LastDay,
    /// Native TO_DAYS policy.
    ToDays,
    /// Native TO_SECONDS policy.
    ToSeconds,
}
impl NativeCalendarOperation {
    /// Original spelling used by the return-family boundary.
    pub fn name(self) -> &'static str {
        match self {
            Self::Date => "date",
            Self::Year => "year",
            Self::Month => "month",
            Self::Day => "day",
            Self::Quarter => "quarter",
            Self::Hour => "hour",
            Self::Minute => "minute",
            Self::Second => "second",
            Self::DayOfWeek => "dayofweek",
            Self::DayOfYear => "dayofyear",
            Self::Weekday => "weekday",
            Self::MonthName => "monthname",
            Self::DayName => "dayname",
            Self::WeekOfYear => "weekofyear",
            Self::Week => "week",
            Self::YearWeek => "yearweek",
            Self::LastDay => "last_day",
            Self::ToDays => "to_days",
            Self::ToSeconds => "to_seconds",
        }
    }
    /// Accepted source argument counts.
    pub fn accepts_arity(self, n: usize) -> bool {
        if matches!(self, Self::Week | Self::YearWeek) {
            (1..=2).contains(&n)
        } else {
            n == 1
        }
    }
    /// Original ETDatetime argument boundary; HMS deliberately uses text.
    pub fn datetime_argument(self) -> bool {
        !matches!(self, Self::Hour | Self::Minute | Self::Second)
    }
}
/// Resource refusal is separate from a NULL parse or invalid API operand.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCalendarError {
    /// Finite work allowance exhausted.
    Resource,
    /// Operation and operand domains differ.
    Contract,
}
fn work(maximum: usize, n: usize) -> Result<(), NativeCalendarError> {
    if n > maximum {
        Err(NativeCalendarError::Resource)
    } else {
        Ok(())
    }
}
/// Actual scalar or owned text output of the calendar kernel.
#[derive(Debug, Eq, PartialEq)]
pub enum NativeCalendarValue {
    /// SQL NULL, including invalid SQL text.
    Null,
    /// Original signed integer.
    Int(i64),
    /// Original full month/day/date text.
    Text(String),
}
/// Full unvalidated stored core; no rebuild, formatting or zero-date rejection.
pub fn native_calendar_core(
    op: NativeCalendarOperation,
    raw: Option<u64>,
    maximum: usize,
) -> Result<Option<i64>, NativeCalendarError> {
    work(maximum, 0)?;
    use NativeCalendarOperation as O;
    let f: fn(u64) -> i64 = match op {
        O::Year => |v| Time::year_from_core_bits(v) as i64,
        O::Month => |v| Time::month_from_core_bits(v) as i64,
        O::Day => |v| Time::day_from_core_bits(v) as i64,
        O::Quarter => |v| Time::quarter_from_core_bits(v) as i64,
        O::Week => |v| Time::native_core_week(v, 0) as i64,
        _ => return Err(NativeCalendarError::Contract),
    };
    Ok(raw.map(f))
}
/// DATE uses the caller's captured modes even if warning callbacks change them.
pub fn native_calendar_date(
    raw: u64,
    no_zero_date: bool,
    no_zero_in_date: bool,
    maximum: usize,
) -> Result<Option<u64>, NativeCalendarError> {
    work(maximum, 0)?;
    Ok(
        (!Time::native_date_rejects_zero(raw, no_zero_date, no_zero_in_date))
            .then(|| Time::native_date_core(raw)),
    )
}
/// Legacy elapsed nanoseconds have no SQL text clamp or calendar cast.
pub fn native_calendar_nanos(
    op: NativeCalendarOperation,
    nanos: Option<i64>,
    maximum: usize,
) -> Result<Option<i64>, NativeCalendarError> {
    work(maximum, 0)?;
    use NativeCalendarOperation as O;
    let f: fn(i64) -> i64 = match op {
        O::Hour => |v| Duration::hours_from_nanos(v) as i64,
        O::Minute => |v| Duration::minutes_from_nanos(v) as i64,
        O::Second => |v| Duration::secs_from_nanos(v) as i64,
        _ => return Err(NativeCalendarError::Contract),
    };
    Ok(nanos.map(f))
}
fn last_day(value: &str) -> Option<String> {
    let value = value.trim();
    let (date, time) = value
        .split_once(char::is_whitespace)
        .map_or((value, None), |(d, t)| (d, Some(t.trim())));
    let (y, m, _) = Time::parse_native_date_ymd(date)?;
    if time.is_some_and(|t| Time::parse_native_clock_with_fraction(t).is_none()) {
        return None;
    }
    let next = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let (y, m, d) =
        Time::native_civil_from_days(Time::native_days_from_civil(next.0, next.1, 1) - 1);
    Some(format!("{y:04}-{m:02}-{d:02}"))
}
/// The actual parsed YMD retained between the original date-validity check and
/// optional mode conversion. Private fields and consuming finish prevent
/// replay.
#[derive(Debug)]
pub struct NativeCalendarWeek {
    year: i64,
    month: u32,
    day: u32,
    yearweek: bool,
}
impl NativeCalendarWeek {
    /// Consume actual optional mode; explicit NULL retains mode zero.
    pub fn finish(self, mode: Option<i64>, maximum: usize) -> Result<i64, NativeCalendarError> {
        work(maximum, 0)?;
        let (y, n) = Time::native_week_of_year(
            self.year,
            self.month,
            self.day,
            mode.unwrap_or(0),
            self.yearweek,
        );
        Ok(if self.yearweek {
            let v = y * 100 + n;
            if v < 0 { i64::from(u32::MAX) } else { v }
        } else {
            n
        })
    }
}
/// Parse the actual SQL date before demanding mode conversion.
pub fn native_calendar_week(
    text: Option<&str>,
    yearweek: bool,
    maximum: usize,
) -> Result<Option<NativeCalendarWeek>, NativeCalendarError> {
    text_work(text, maximum)?;
    Ok(text
        .and_then(Time::parse_native_date_ymd)
        .map(|(year, month, day)| NativeCalendarWeek {
            year,
            month,
            day,
            yearweek,
        }))
}
fn text_work(text: Option<&str>, maximum: usize) -> Result<(), NativeCalendarError> {
    let n = match text {
        None => 0,
        Some(v) => v
            .len()
            .checked_mul(16)
            .and_then(|n| n.checked_add(1024))
            .ok_or(NativeCalendarError::Resource)?,
    };
    work(maximum, n)
}
/// Execute the original parser and projection over actual text. Week operations
/// use the consuming API instead, so invalid dates suppress mode conversion.
pub fn native_calendar_text(
    op: NativeCalendarOperation,
    text: Option<&str>,
    maximum: usize,
) -> Result<NativeCalendarValue, NativeCalendarError> {
    text_work(text, maximum)?;
    use NativeCalendarOperation as O;
    use NativeCalendarValue as V;
    let Some(s) = text else {
        return Ok(V::Null);
    };
    let value = match op {
        O::Hour | O::Minute | O::Second => Time::parse_native_hms(s).map(|(h, m, s)| {
            V::Int(match op {
                O::Hour => h,
                O::Minute => m,
                _ => s,
            } as i64)
        }),
        O::LastDay => last_day(s).map(V::Text),
        O::ToDays | O::ToSeconds => {
            Time::parse_native_datetime_components(s).map(|(y, m, d, h, min, sec, _)| {
                V::Int(
                    Time::native_time_diff_daynr(y, m, d) * if op == O::ToDays { 1 } else { 86400 }
                        + if op == O::ToDays {
                            0
                        } else {
                            i64::from(h) * 3600 + i64::from(min) * 60 + i64::from(sec)
                        },
                )
            })
        }
        O::DayOfWeek | O::DayOfYear | O::Weekday | O::MonthName | O::DayName | O::WeekOfYear => {
            Time::parse_native_date_ymd(s).map(|(y, m, d)| match op {
                O::DayOfWeek => V::Int((Time::native_weekday_sunday_index(y, m, d) + 1) as i64),
                O::Weekday => V::Int(((Time::native_weekday_sunday_index(y, m, d) + 6) % 7) as i64),
                O::DayOfYear => V::Int(Time::native_day_of_year(y, m, d)),
                O::MonthName => V::Text(Time::month_name_from_month(m).to_string()),
                O::DayName => V::Text(
                    Time::weekday_name_from_sunday_index(Time::native_weekday_sunday_index(
                        y, m, d,
                    ))
                    .to_string(),
                ),
                _ => V::Int(Time::native_week_of_year(y, m, d, 3, false).1),
            })
        }
        _ => return Err(NativeCalendarError::Contract),
    };
    Ok(value.unwrap_or(V::Null))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calendar_actual_core_text_consuming_week_and_finite_work() {
        use NativeCalendarOperation as O;
        use NativeCalendarValue as V;
        for raw in [None, Some(0), Some(u64::MAX), Some(i64::MIN as u64)] {
            assert_eq!(
                native_calendar_core(O::Month, raw, 0),
                Ok(raw.map(|v| Time::month_from_core_bits(v) as i64))
            );
            assert_eq!(
                native_calendar_core(O::Week, raw, 0),
                Ok(raw.map(|v| Time::native_core_week(v, 0) as i64))
            );
            assert_eq!(
                native_calendar_nanos(O::Hour, raw.map(|v| v as i64), 0),
                Ok(raw.map(|v| Duration::hours_from_nanos(v as i64) as i64))
            );
        }
        assert_eq!(
            native_calendar_text(O::MonthName, Some("2000-02-29"), 8192),
            Ok(V::Text("February".into()))
        );
        assert_eq!(
            native_calendar_text(O::LastDay, Some("2000-02-01 99:00:00"), 8192),
            Ok(V::Null)
        );
        assert_eq!(
            native_calendar_text(O::LastDay, Some("2000-02-01 12:00:00"), 8192),
            Ok(V::Text("2000-02-29".into()))
        );
        assert_eq!(
            native_calendar_text(O::Hour, Some("999:00:00"), 8192),
            Ok(V::Int(838))
        );
        assert_eq!(native_calendar_text(O::Hour, None, 0), Ok(V::Null));
        assert!(
            native_calendar_week(Some("invalid"), false, 8192)
                .unwrap()
                .is_none()
        );
        let week = native_calendar_week(Some("2021-01-01"), false, 8192)
            .unwrap()
            .unwrap();
        assert_eq!(week.finish(Some(3), 0), Ok(53));
        let week = native_calendar_week(Some("2021-01-01"), true, 8192)
            .unwrap()
            .unwrap();
        assert_eq!(week.finish(Some(3), 0), Ok(202053));
        let week = native_calendar_week(Some("2021-01-01"), false, 8192)
            .unwrap()
            .unwrap();
        assert_eq!(week.finish(None, 0), Ok(0));
        assert_eq!(
            native_calendar_text(O::DayName, Some(&"1".repeat(100000)), 8192),
            Err(NativeCalendarError::Resource)
        );
        assert_eq!(
            native_calendar_text(O::Hour, None, usize::MAX),
            native_calendar_text(O::Hour, None, 1 << 30)
        );
        assert_eq!(
            native_calendar_core(O::Hour, None, 0),
            Err(NativeCalendarError::Contract)
        );
        assert_eq!(
            native_calendar_date(u64::MAX, true, true, 0),
            Ok(Some(Time::native_date_core(u64::MAX)))
        );
    }
}
