// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Original native TIMEDIFF text policy over the shared calendar primitives.
//! Coercion and its left-to-right demand remain outside this pure core.

use tidb_query_datatype::codec::mysql::Time;

enum TimeDiffValue {
    DateTime { micros: i64, fsp: usize },
    Duration { micros: i64, fsp: usize },
}

/// Determines only whether the original right operand's coercion is demanded.
/// The worker independently reparses actual text; no parsed answer is carried.
pub fn native_time_diff_needs_right(left: Option<&str>) -> bool {
    left.and_then(parse_time_diff_value).is_some()
}

/// Computes the matching datetime/duration difference from actual nullable
/// text.
pub fn native_time_diff(left: Option<&str>, right: Option<&str>) -> Option<String> {
    let left = parse_time_diff_value(left?)?;
    let right = parse_time_diff_value(right?)?;
    let (left_micros, right_micros, fsp) = match (left, right) {
        (
            TimeDiffValue::DateTime {
                micros: left,
                fsp: left_fsp,
            },
            TimeDiffValue::DateTime {
                micros: right,
                fsp: right_fsp,
            },
        )
        | (
            TimeDiffValue::Duration {
                micros: left,
                fsp: left_fsp,
            },
            TimeDiffValue::Duration {
                micros: right,
                fsp: right_fsp,
            },
        ) => (left, right, left_fsp.max(right_fsp)),
        _ => return None,
    };
    Some(native_format_time_diff(
        truncate_time_diff(left_micros.saturating_sub(right_micros)),
        fsp,
    ))
}

fn parse_time_diff_value(text: &str) -> Option<TimeDiffValue> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some((date, time)) = text.split_once(char::is_whitespace) {
        return parse_datetime_diff_value(date, time.trim());
    }
    if text.contains(':') {
        return parse_duration_diff_value(text);
    }
    // Date-only values are datetimes at midnight; colon durations were handled
    // first, including the original short fields such as 10:9:0.
    parse_datetime_diff_value(text, "00:00:00")
}

fn parse_datetime_diff_value(date: &str, time: &str) -> Option<TimeDiffValue> {
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
    let (hour, minute, second, fraction) = Time::parse_native_clock_with_fraction(time)?;
    let fsp = fraction.len();
    let microsecond = fraction.parse::<u32>().ok().unwrap_or(0) * 10u32.pow(6 - fsp as u32);
    let micros = Time::native_time_diff_daynr(year, month, day)
        .checked_mul(86_400_000_000)?
        .checked_add(i64::from(hour) * 3_600_000_000)?
        .checked_add(i64::from(minute) * 60_000_000)?
        .checked_add(i64::from(second) * 1_000_000)?
        .checked_add(i64::from(microsecond))?;
    Some(TimeDiffValue::DateTime { micros, fsp })
}

const MAX_TIME_DIFF_MICROS: i64 = (838 * 3_600 + 59 * 60 + 59) * 1_000_000;

fn truncate_time_diff(micros: i64) -> i64 {
    micros.clamp(-MAX_TIME_DIFF_MICROS, MAX_TIME_DIFF_MICROS)
}

fn parse_duration_diff_value(text: &str) -> Option<TimeDiffValue> {
    let (negative, text) = text
        .strip_prefix('-')
        .map_or((false, text), |text| (true, text));
    let mut fields = text.splitn(3, ':');
    let hour = fields.next()?.parse::<i64>().ok()?;
    let minute = fields.next()?.parse::<u32>().ok()?;
    let second_part = fields.next()?;
    let (second_part, fraction) = second_part.split_once('.').unwrap_or((second_part, ""));
    let second = second_part.parse::<u32>().ok()?;
    if minute > 59 || second > 59 || fraction.len() > 6 || !fraction.is_ascii() {
        return None;
    }
    let microsecond = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u32>().ok()? * 10u32.pow(6 - fraction.len() as u32)
    };
    let micros = hour
        .checked_mul(3_600_000_000)?
        .checked_add(i64::from(minute) * 60_000_000)?
        .checked_add(i64::from(second) * 1_000_000)?
        .checked_add(i64::from(microsecond))?;
    Some(TimeDiffValue::Duration {
        micros: if negative { -micros } else { micros },
        fsp: fraction.len(),
    })
}

/// Formats the original raw microsecond/FSP domain, without adding TIMEDIFF's
/// clamp or normalizing FSP. Other native duration consumers share this body.
pub fn native_format_time_diff(micros: i64, fsp: usize) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let absolute = micros.unsigned_abs();
    let hours = absolute / 3_600_000_000;
    let minutes = absolute / 60_000_000 % 60;
    let seconds = absolute / 1_000_000 % 60;
    if fsp == 0 {
        return format!("{sign}{hours:02}:{minutes:02}:{seconds:02}");
    }
    let divisor = 10u64.pow(6 - fsp as u32);
    let fraction = absolute / divisor % 10u64.pow(fsp as u32);
    format!(
        "{sign}{hours:02}:{minutes:02}:{seconds:02}.{fraction:0width$}",
        width = fsp
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_diff_keeps_parser_demand_fraction_clamps_and_raw_formatter() {
        assert!(!native_time_diff_needs_right(None));
        assert!(!native_time_diff_needs_right(Some(" \t")));
        assert!(!native_time_diff_needs_right(Some("2023-02-29")));
        assert!(native_time_diff_needs_right(Some("2024-00-00")));
        assert!(native_time_diff_needs_right(Some("10:9:0")));
        assert_eq!(native_time_diff(None, Some("10:00:00")), None);
        assert_eq!(native_time_diff(Some("10:00:00"), None), None);
        assert_eq!(native_time_diff(Some("2024-01-01"), Some("00:00:00")), None);
        assert_eq!(
            native_time_diff(Some("10:9:0.12"), Some("01:08:59.001")),
            Some("09:00:01.119".into())
        );
        assert_eq!(
            native_time_diff(Some("-10:00:00"), Some("01:00:00.0")),
            Some("-11:00:00.0".into())
        );
        assert_eq!(
            native_time_diff(Some("1000:00:00.123"), Some("00:00:00")),
            Some("838:59:59.000".into())
        );
        assert_eq!(
            native_time_diff(Some("00:00:00"), Some("1000:00:00.123")),
            Some("-838:59:59.000".into())
        );
        assert_eq!(
            native_time_diff(Some("2024-01-02 00:00:00.1234567"), Some("2024-01-01")),
            Some("24:00:00.123456".into())
        );
        assert!(!native_time_diff_needs_right(Some(
            "2024-01-02 00:00:00.123456x"
        )));
        assert!(!native_time_diff_needs_right(Some("00:00:00.1234567")));
        assert!(!native_time_diff_needs_right(Some(
            "9223372036854775807:00:00"
        )));
        assert!(!native_time_diff_needs_right(Some("4294967295-01-01")));
        assert_eq!(
            native_time_diff(Some("00:00:00.+1"), Some("00:00:00")),
            Some("00:00:00.01".into())
        );
        assert_eq!(
            native_time_diff(Some("2024-00-00"), Some("2024-00-00")),
            Some("00:00:00".into())
        );
        assert_eq!(
            native_time_diff(Some("24-01-01"), Some("2024-01-01")),
            Some("00:00:00".into())
        );
        assert_eq!(
            native_time_diff(Some("024-01-01"), Some("2024-01-01")),
            Some("-838:59:59".into())
        );
        assert_eq!(
            native_format_time_diff(i64::MIN, 6),
            "-2562047788:00:54.775808"
        );
        assert_eq!(native_format_time_diff(0, 6), "00:00:00.000000");
        assert_eq!(native_format_time_diff(-1, 0), "-00:00:00");
    }
}
