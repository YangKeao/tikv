// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Pure ordinary DATE_ADD/DATE_SUB arithmetic. Input coercion, statement
//! getters and warning delivery belong to the controller; these are not wire
//! policies.

use tidb_query_datatype::codec::mysql::{Time, time::native_extract_duration_value};

use crate::native_extract::time_parts_with_micros;

pub(crate) enum PreparedAmount<'a> {
    Whole(i64),
    SecondMicros(i64),
    Composite(&'a str),
}

pub(crate) enum CalendarOutcome {
    Text(String),
    Null,
    Overflow,
    UnsupportedUnit,
}

pub(crate) fn composite_spec(unit: &str) -> Option<(usize, usize)> {
    const MONTH: usize = 1;
    const HOUR: usize = 3;
    const MINUTE: usize = 4;
    const SECOND: usize = 5;
    const MICROSECOND: usize = 6;
    Some(match unit.to_ascii_uppercase().as_str() {
        "YEAR_MONTH" => (MONTH, 2),
        "DAY_HOUR" => (HOUR, 2),
        "DAY_MINUTE" => (MINUTE, 3),
        "DAY_SECOND" => (SECOND, 4),
        "DAY_MICROSECOND" => (MICROSECOND, 5),
        "HOUR_MINUTE" => (MINUTE, 2),
        "HOUR_SECOND" => (SECOND, 3),
        "HOUR_MICROSECOND" => (MICROSECOND, 4),
        "MINUTE_SECOND" => (SECOND, 2),
        "MINUTE_MICROSECOND" => (MICROSECOND, 3),
        "SECOND_MICROSECOND" => (MICROSECOND, 2),
        _ => return None,
    })
}

/// The ordinary composite parser deliberately differs from the datatype parser:
/// excess groups yield zero and the last group is not fractionally padded.
pub(crate) fn parse_composite_value(
    index: usize,
    cnt: usize,
    format: &str,
) -> (i64, i64, i64, i64) {
    let trimmed = format.trim();
    let (neg, body) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    };
    let mut matches: Vec<i64> = Vec::new();
    let mut digits = String::new();
    for c in body.chars().chain(std::iter::once('\0')) {
        if c.is_ascii_digit() {
            digits.push(c);
        } else if !digits.is_empty() {
            matches.push(digits.parse().unwrap_or(i64::MAX));
            digits.clear();
        }
    }
    if matches.len() > cnt {
        return (0, 0, 0, 0);
    }
    let mut fields = [0i64; 7];
    let mut idx = index as i64;
    for i in 0..matches.len() {
        let value = matches[matches.len() - 1 - i];
        if idx >= 0 {
            fields[idx as usize] = if neg { -value } else { value };
        }
        idx -= 1;
    }
    let years = fields[0];
    let months = fields[1];
    let mut days = fields[2];
    let mut seconds = fields[3] * 3600 + fields[4] * 60 + fields[5];
    days += seconds / 86_400;
    seconds %= 86_400;
    let nanos = seconds * 1_000_000_000 + fields[6] * 1000;
    (years, months, days, nanos)
}

pub(crate) fn format_decimal_composite_text(unit: &str, visible_decimal: &str) -> String {
    let (negative, magnitude) = visible_decimal
        .strip_prefix('-')
        .map_or((false, visible_decimal), |value| (true, value));
    let formatted = match unit.to_ascii_uppercase().as_str() {
        "HOUR_MINUTE" | "MINUTE_SECOND" => magnitude.replace('.', ":"),
        "YEAR_MONTH" => magnitude.replace('.', "-"),
        "DAY_HOUR" => magnitude.replace('.', " "),
        "DAY_MINUTE" => format!("0 {}", magnitude.replace('.', ":")),
        "DAY_SECOND" => format!("0 00:{}", magnitude.replace('.', ":")),
        "DAY_MICROSECOND" => format!("0 00:00:{magnitude}"),
        "HOUR_MICROSECOND" => format!("00:00:{magnitude}"),
        "HOUR_SECOND" => format!("00:{}", magnitude.replace('.', ":")),
        "MINUTE_MICROSECOND" => format!("00:{magnitude}"),
        "SECOND_MICROSECOND" => magnitude.to_string(),
        _ => magnitude.to_string(),
    };
    if negative {
        format!("-{formatted}")
    } else {
        formatted
    }
}

/// The old caller discarded IntOverflow with .ok(); keep that result boundary
/// and the original arithmetic, including eager then_some evaluation.
pub(crate) fn decimal_seconds_to_micros(text: &str) -> Option<i64> {
    let text = text.trim();
    let (negative, magnitude) = text.strip_prefix('-').map_or_else(
        || (false, text.strip_prefix('+').unwrap_or(text)),
        |value| (true, value),
    );
    let (whole, fraction) = magnitude.split_once('.').unwrap_or((magnitude, ""));
    let whole = whole.parse::<i128>().ok()?;
    let mut micros = fraction.bytes().take(6).try_fold(0i128, |value, digit| {
        digit
            .is_ascii_digit()
            .then_some(value * 10 + i128::from(digit - b'0'))
    })?;
    for _ in fraction.len().min(6)..6 {
        micros *= 10;
    }
    let value = whole
        .checked_mul(1_000_000)
        .and_then(|value| value.checked_add(micros))?;
    i64::try_from(if negative { -value } else { value }).ok()
}

pub(crate) fn calendar_date_valid(date_text: &str) -> bool {
    let trimmed = date_text.trim();
    let date = trimmed
        .split_once(char::is_whitespace)
        .map_or(trimmed, |(date, _)| date);
    Time::parse_native_date_ymd(date).is_some()
}

fn calendar_result(value: Option<String>) -> CalendarOutcome {
    match value {
        Some(value) => CalendarOutcome::Text(value),
        None => CalendarOutcome::Overflow,
    }
}

pub(crate) fn apply_calendar(
    unit: &str,
    date_text: &str,
    amount: PreparedAmount<'_>,
    sign: i64,
    result_fsp: Option<u32>,
) -> CalendarOutcome {
    let trimmed = date_text.trim();
    let (date_str, time_suffix) = trimmed
        .split_once(char::is_whitespace)
        .map_or((trimmed, None), |(date, time)| (date, Some(time)));
    let Some((y, m, d)) = Time::parse_native_date_ymd(date_str) else {
        return CalendarOutcome::Null;
    };
    if let Some((index, count)) = composite_spec(unit) {
        let PreparedAmount::Composite(format) = amount else {
            return CalendarOutcome::UnsupportedUnit;
        };
        let Some((h, mi, sec, microsecond)) = time_parts_with_micros(time_suffix) else {
            return CalendarOutcome::Null;
        };
        let (years, months, days, nanos) = parse_composite_value(index, count, format);
        if years != 0 || months != 0 {
            let Some((y2, m2, d2)) = years
                .checked_mul(12)
                .and_then(|value| value.checked_add(months))
                .and_then(|value| sign.checked_mul(value))
                .and_then(|delta| add_months(y, m, d, delta))
            else {
                return CalendarOutcome::Overflow;
            };
            return calendar_result(format_ymd_result(y2, m2, d2, time_suffix));
        }
        let Some(delta_micros) = days
            .checked_mul(86_400_000_000)
            .and_then(|value| value.checked_add(nanos / 1_000))
            .and_then(|value| sign.checked_mul(value))
        else {
            return CalendarOutcome::Overflow;
        };
        return calendar_result(date_add_time(
            (y, m, d, h, mi, sec, microsecond),
            delta_micros,
            result_fsp,
        ));
    }
    if unit.eq_ignore_ascii_case("HOUR") || unit.eq_ignore_ascii_case("MINUTE") {
        let PreparedAmount::Whole(number) = amount else {
            return CalendarOutcome::UnsupportedUnit;
        };
        let unit_micros = if unit.eq_ignore_ascii_case("HOUR") {
            3_600_000_000
        } else {
            60_000_000
        };
        let Some(delta_micros) = sign
            .checked_mul(number)
            .and_then(|value| value.checked_mul(unit_micros))
        else {
            return CalendarOutcome::Overflow;
        };
        let Some((h, mi, sec, microsecond)) = time_parts_with_micros(time_suffix) else {
            return CalendarOutcome::Null;
        };
        return calendar_result(date_add_time(
            (y, m, d, h, mi, sec, microsecond),
            delta_micros,
            result_fsp,
        ));
    }
    if unit.eq_ignore_ascii_case("SECOND") {
        let PreparedAmount::SecondMicros(number) = amount else {
            return CalendarOutcome::UnsupportedUnit;
        };
        let Some(delta_micros) = sign.checked_mul(number) else {
            return CalendarOutcome::Overflow;
        };
        let Some((h, mi, sec, microsecond)) = time_parts_with_micros(time_suffix) else {
            return CalendarOutcome::Null;
        };
        return calendar_result(date_add_time(
            (y, m, d, h, mi, sec, microsecond),
            delta_micros,
            result_fsp,
        ));
    }
    let PreparedAmount::Whole(number) = amount else {
        return CalendarOutcome::UnsupportedUnit;
    };
    if unit.eq_ignore_ascii_case("MICROSECOND") {
        // This unit parses the suffix before multiplying the sign, unlike
        // HOUR/MINUTE/SECOND. That distinction controls NULL versus overflow.
        let Some((h, mi, sec, microsecond)) = time_parts_with_micros(time_suffix) else {
            return CalendarOutcome::Null;
        };
        let Some(delta_micros) = sign.checked_mul(number) else {
            return CalendarOutcome::Overflow;
        };
        return calendar_result(date_add_time(
            (y, m, d, h, mi, sec, microsecond),
            delta_micros,
            result_fsp,
        ));
    }
    let scaled = |factor: i64| {
        sign.checked_mul(number)
            .and_then(|value| value.checked_mul(factor))
    };
    let shifted_days = |factor: i64| {
        let days = scaled(factor)
            .and_then(|delta| Time::native_days_from_civil(y, m, d).checked_add(delta))?;
        (Time::native_days_from_civil(0, 1, 1)..=Time::native_days_from_civil(9999, 12, 31))
            .contains(&days)
            .then_some(days)
    };
    let (y2, m2, d2) = if unit.eq_ignore_ascii_case("DAY") {
        match shifted_days(1) {
            Some(days) => Time::native_civil_from_days(days),
            None => return CalendarOutcome::Overflow,
        }
    } else if unit.eq_ignore_ascii_case("WEEK") {
        match shifted_days(7) {
            Some(days) => Time::native_civil_from_days(days),
            None => return CalendarOutcome::Overflow,
        }
    } else if unit.eq_ignore_ascii_case("MONTH") {
        let Some(date) = scaled(1).and_then(|months| add_months(y, m, d, months)) else {
            return CalendarOutcome::Overflow;
        };
        date
    } else if unit.eq_ignore_ascii_case("YEAR") {
        let Some(date) = scaled(12).and_then(|months| add_months(y, m, d, months)) else {
            return CalendarOutcome::Overflow;
        };
        date
    } else if unit.eq_ignore_ascii_case("QUARTER") {
        let Some(date) = scaled(3).and_then(|months| add_months(y, m, d, months)) else {
            return CalendarOutcome::Overflow;
        };
        date
    } else {
        return CalendarOutcome::UnsupportedUnit;
    };
    calendar_result(format_ymd_result(y2, m2, d2, time_suffix))
}

pub(crate) fn apply_duration(
    unit: &str,
    date_nanos: i64,
    amount: PreparedAmount<'_>,
    sign: i64,
    result_fsp: i64,
) -> Option<(i64, i64)> {
    let upper = unit.to_ascii_uppercase();
    let interval_nanos = if let Some((index, count)) = composite_spec(&upper) {
        let PreparedAmount::Composite(text) = amount else {
            return None;
        };
        // Retain the broad ordinary parse first, even though the datatype
        // extraction below parses again under its distinct truncation policy.
        let (years, months, ..) = parse_composite_value(index, count, text);
        if years != 0 || months != 0 {
            return None;
        }
        native_extract_duration_value(&upper, text).ok()?.0
    } else {
        let nanos = match (upper.as_str(), amount) {
            ("MICROSECOND", PreparedAmount::Whole(value)) => value.checked_mul(1_000),
            ("SECOND", PreparedAmount::SecondMicros(value)) => value.checked_mul(1_000),
            ("MINUTE", PreparedAmount::Whole(value)) => value.checked_mul(60_000_000_000),
            ("HOUR", PreparedAmount::Whole(value)) => value.checked_mul(3_600_000_000_000),
            _ => return None,
        };
        // Native MAX_TIME_NANOS excludes fractional seconds here. Do not use
        // the different bound in native_extract_duration_value for this branch.
        nanos.filter(|value| value.unsigned_abs() <= 3_020_399_000_000_000)?
    };
    // Native checked_add/sub only checks i64 arithmetic. It does not enforce
    // SQL TIME range; the caller stamps result_fsp without normalization.
    let value = if sign < 0 {
        date_nanos.checked_sub(interval_nanos)?
    } else {
        date_nanos.checked_add(interval_nanos)?
    };
    Some((value, result_fsp))
}

fn date_add_time(
    parts: (i64, u32, u32, u32, u32, u32, u32),
    delta_micros: i64,
    result_fsp: Option<u32>,
) -> Option<String> {
    let (y, m, d, h, mi, sec, microsecond) = parts;
    const MICROS_PER_SECOND: i64 = 1_000_000;
    const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;
    let total = Time::native_days_from_civil(y, m, d)
        .checked_mul(MICROS_PER_DAY)
        .and_then(|value| {
            value.checked_add(
                (i64::from(h) * 3_600 + i64::from(mi) * 60 + i64::from(sec)) * MICROS_PER_SECOND
                    + i64::from(microsecond),
            )
        })
        .and_then(|value| value.checked_add(delta_micros))?;
    let day_count = total.div_euclid(MICROS_PER_DAY);
    let micros_of_day = total.rem_euclid(MICROS_PER_DAY);
    let seconds_of_day = micros_of_day / MICROS_PER_SECOND;
    let (y2, m2, d2) = Time::native_civil_from_days(day_count);
    format_ymdhms_result(
        (
            y2,
            m2,
            d2,
            (seconds_of_day / 3_600) as u32,
            (seconds_of_day / 60 % 60) as u32,
            (seconds_of_day % 60) as u32,
            (micros_of_day % MICROS_PER_SECOND) as u32,
        ),
        result_fsp,
    )
}

fn format_ymdhms_result(
    parts: (i64, u32, u32, u32, u32, u32, u32),
    result_fsp: Option<u32>,
) -> Option<String> {
    let (y, m, d, h, mi, sec, microsecond) = parts;
    let fsp = result_fsp
        .map(|value| value.min(6))
        .unwrap_or_else(|| if microsecond == 0 { 0 } else { 6 });
    let fraction = if fsp == 0 {
        String::new()
    } else {
        let value = microsecond / 10u32.pow(6 - fsp);
        format!(".{value:0width$}", width = fsp as usize)
    };
    if y == 0 {
        return Some(format!("0000-00-00 {h:02}:{mi:02}:{sec:02}{fraction}"));
    }
    if !(1..=9999).contains(&y) {
        return None;
    }
    Some(format!(
        "{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{sec:02}{fraction}"
    ))
}

fn add_months(y: i64, m: u32, d: u32, n: i64) -> Option<(i64, u32, u32)> {
    let total = y
        .checked_mul(12)?
        .checked_add(i64::from(m - 1))?
        .checked_add(n)?;
    let y2 = total.div_euclid(12);
    if !(0..=9999).contains(&y2) {
        return None;
    }
    let m2 = (total.rem_euclid(12) + 1) as u32;
    let d2 = d.min(Time::native_days_in_month(y2, m2));
    Some((y2, m2, d2))
}

fn format_ymd_result(y: i64, m: u32, d: u32, time_suffix: Option<&str>) -> Option<String> {
    if y == 0 {
        return Some("0000-00-00".to_string());
    }
    if !(1..=9999).contains(&y) {
        return None;
    }
    Some(match time_suffix {
        Some(time) => format!("{y:04}-{m:02}-{d:02} {time}"),
        None => format!("{y:04}-{m:02}-{d:02}"),
    })
}
