// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Ordinary native STR_TO_DATE, deliberately distinct from public datatype and
//! wire grammars. Only a successful date scan requests date_modes. Typed clock
//! prefixing has its own later request, preserving the original getter demand.

use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::{
    FieldTypeTp,
    codec::{
        data_type::{Bytes, BytesRef, ChunkedVec, EvaluableRef, EvaluableRet, Int},
        mysql::{Time, time::native_is_go_punctuation},
    },
};

use crate::NativeIdentityFrameError;

type FrameResult<T> = std::result::Result<T, NativeIdentityFrameError>;
const ZERO_WARNING: &str = "Incorrect datetime value: '0000-00-00 00:00:00'";
const INPUT_WARNING_PREFIX: &str = "Incorrect datetime value: '";
const INPUT_WARNING_SUFFIX: &str = "' for function str_to_date";
const INVALID_DATE_WARNING_SUFFIX: &str = "'";
const STATE_HEADER: usize = 34;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeStrToDateResult<'a> {
    Value(&'a str),
    Warning {
        code: u16,
        message: &'a str,
    },
    /// Borrow of the WHOLE tag-3 report, not just its payload.
    NeedDateModes(&'a [u8]),
    /// Decoded clock text view. Transport must forward the ORIGINAL tag-4
    /// report to typed-finish, not encode this view again.
    NeedTypedDateMode(&'a str),
}

#[derive(Default)]
struct ParsedDateTime {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    microsecond: u32,
    saw_date: bool,
    saw_time: bool,
    saw_fraction: bool,
    saw_24_hour: bool,
    saw_12_hour: bool,
    am_pm: Option<bool>,
}

fn read_state(report: &[u8]) -> Option<(ParsedDateTime, &str)> {
    if report.first() != Some(&3) || report.len() < STATE_HEADER {
        return None;
    }
    let year = i64::from_le_bytes(report[1..9].try_into().ok()?);
    let mut fields = [0u32; 6];
    for (index, field) in fields.iter_mut().enumerate() {
        let start = 9 + index * 4;
        *field = u32::from_le_bytes(report[start..start + 4].try_into().ok()?);
    }
    let [month, day, hour, minute, second, microsecond] = fields;
    let flags = report[33];
    let saw_time = flags & 1 != 0;
    let saw_fraction = flags & 2 != 0;
    if !(0..=9999).contains(&year)
        || month > 12
        || day > 999
        || hour > 23
        || minute > 59
        || second > 59
        || microsecond > 999_999
        || flags > 3
        || (saw_fraction && !saw_time)
        || (!saw_fraction && microsecond != 0)
    {
        return None;
    }
    let input = std::str::from_utf8(&report[STATE_HEADER..]).ok()?;
    Some((
        ParsedDateTime {
            year,
            month,
            day,
            hour,
            minute,
            second,
            microsecond,
            saw_date: true,
            saw_time,
            saw_fraction,
            ..ParsedDateTime::default()
        },
        input,
    ))
}

fn clock_needs_prefix(text: &str) -> bool {
    text.contains(':') && !text.contains('-')
}

pub fn decode_native_str_to_date_result(report: &[u8]) -> Option<NativeStrToDateResult<'_>> {
    let (&tag, payload) = report.split_first()?;
    if tag == 3 {
        read_state(report)?;
        return Some(NativeStrToDateResult::NeedDateModes(report));
    }
    let text = std::str::from_utf8(payload).ok()?;
    match tag {
        0 => Some(NativeStrToDateResult::Value(text)),
        1 if text == ZERO_WARNING => Some(NativeStrToDateResult::Warning {
            code: 1292,
            message: text,
        }),
        2 if text.starts_with(INPUT_WARNING_PREFIX)
            && text.ends_with(INPUT_WARNING_SUFFIX)
            && text.len() >= INPUT_WARNING_PREFIX.len() + INPUT_WARNING_SUFFIX.len() =>
        {
            Some(NativeStrToDateResult::Warning {
                code: 1411,
                message: text,
            })
        }
        5 if text.starts_with(INPUT_WARNING_PREFIX)
            && text.ends_with(INVALID_DATE_WARNING_SUFFIX)
            && text.len() > INPUT_WARNING_PREFIX.len() + INVALID_DATE_WARNING_SUFFIX.len() =>
        {
            Some(NativeStrToDateResult::Warning {
                code: 1292,
                message: text,
            })
        }
        4 if clock_needs_prefix(text) => Some(NativeStrToDateResult::NeedTypedDateMode(text)),
        _ => None,
    }
}

pub fn str_to_date_head_native_args_valid(
    input: Option<&[u8]>,
    format: Option<&[u8]>,
    target: Option<i64>,
) -> bool {
    input.map_or(true, |value| std::str::from_utf8(value).is_ok())
        && format.map_or(true, |value| std::str::from_utf8(value).is_ok())
        // Actual enum codec: named variants use mysql_type; Unknown(byte) uses
        // -1-byte, preserving Unknown(12) as distinct from named Datetime(+12).
        && target.map_or(true, |value| (-256..=255).contains(&value))
}

fn actual_flag(value: Option<i64>) -> bool {
    matches!(value, Some(0 | 1))
}

pub fn str_to_date_finish_native_args_valid(
    state: Option<&[u8]>,
    no_zero_date: Option<i64>,
    allow_invalid_dates: Option<i64>,
) -> bool {
    state.and_then(read_state).is_some()
        && actual_flag(no_zero_date)
        && actual_flag(allow_invalid_dates)
}

pub fn str_to_date_typed_finish_native_args_valid(
    report: Option<&[u8]>,
    no_zero_date: Option<i64>,
) -> bool {
    matches!(
        report.and_then(decode_native_str_to_date_result),
        Some(NativeStrToDateResult::NeedTypedDateMode(_))
    ) && actual_flag(no_zero_date)
}

/// First operand is the actual input text, whole date-state report, or whole
/// typed-state report respectively. This bounds retained output, not transient
/// scanner Vec<char>/formatting allocations or allocator peak memory.
pub(crate) fn native_str_to_date_output_bound(first: Option<&[u8]>) -> Option<usize> {
    first.map_or(0, <[u8]>::len).checked_add(64)
}

fn output_buffer(length: usize, bound: usize) -> FrameResult<Vec<u8>> {
    if length > bound {
        return Err(NativeIdentityFrameError::Capacity);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| NativeIdentityFrameError::Capacity)?;
    if output.capacity() > bound {
        return Err(NativeIdentityFrameError::Capacity);
    }
    Ok(output)
}

fn text_report(tag: u8, text: &str, bound: usize) -> FrameResult<Option<Vec<u8>>> {
    let length = text
        .len()
        .checked_add(1)
        .ok_or(NativeIdentityFrameError::Capacity)?;
    let mut output = output_buffer(length, bound)?;
    output.push(tag);
    output.extend_from_slice(text.as_bytes());
    Ok(Some(output))
}

fn warning_report(month_zero: bool, input: &str, bound: usize) -> FrameResult<Option<Vec<u8>>> {
    if month_zero {
        text_report(
            2,
            &format!("{INPUT_WARNING_PREFIX}{input}{INPUT_WARNING_SUFFIX}"),
            bound,
        )
    } else {
        text_report(1, ZERO_WARNING, bound)
    }
}

fn invalid_date_warning_report(
    value: &ParsedDateTime,
    bound: usize,
) -> FrameResult<Option<Vec<u8>>> {
    let rendered = format!(
        "{:04}-{:02}-{:02} {}",
        value.year,
        value.month,
        value.day,
        render_clock(value)
    );
    text_report(
        5,
        &format!("{INPUT_WARNING_PREFIX}{rendered}{INVALID_DATE_WARNING_SUFFIX}"),
        bound,
    )
}

fn date_state(value: &ParsedDateTime, input: &str, bound: usize) -> FrameResult<Option<Vec<u8>>> {
    let length = STATE_HEADER
        .checked_add(input.len())
        .ok_or(NativeIdentityFrameError::Capacity)?;
    let mut output = output_buffer(length, bound)?;
    output.push(3);
    output.extend_from_slice(&value.year.to_le_bytes());
    for field in [
        value.month,
        value.day,
        value.hour,
        value.minute,
        value.second,
        value.microsecond,
    ] {
        output.extend_from_slice(&field.to_le_bytes());
    }
    output.push(u8::from(value.saw_time) | (u8::from(value.saw_fraction) << 1));
    output.extend_from_slice(input.as_bytes());
    Ok(Some(output))
}

fn render_clock(value: &ParsedDateTime) -> String {
    if value.saw_fraction {
        format!(
            "{:02}:{:02}:{:02}.{:06}",
            value.hour, value.minute, value.second, value.microsecond
        )
    } else {
        format!("{:02}:{:02}:{:02}", value.hour, value.minute, value.second)
    }
}

pub(crate) fn evaluate_str_to_date_head_native(
    input: Option<&[u8]>,
    format: Option<&[u8]>,
    target: Option<i64>,
) -> FrameResult<Option<Vec<u8>>> {
    if !str_to_date_head_native_args_valid(input, format, target) {
        return Err(NativeIdentityFrameError::Invalid);
    }
    let bound = native_str_to_date_output_bound(input).ok_or(NativeIdentityFrameError::Capacity)?;
    let (Some(input), Some(format)) = (input, format) else {
        return Ok(None);
    };
    let input = std::str::from_utf8(input).expect("validated input");
    let format = std::str::from_utf8(format).expect("validated format");
    let value = match scan(input, format) {
        Ok(value) => value,
        Err(month_zero) => return warning_report(month_zero, input, bound),
    };
    if value.saw_date {
        return date_state(&value, input, bound);
    }
    if value.saw_time {
        let text = render_clock(&value);
        let tag = if target == Some(FieldTypeTp::DateTime as i64) && clock_needs_prefix(&text) {
            4
        } else {
            0
        };
        return text_report(tag, &text, bound);
    }
    warning_report(false, input, bound)
}

pub(crate) fn evaluate_str_to_date_finish_native(
    state: Option<&[u8]>,
    no_zero_date: Option<i64>,
    allow_invalid_dates: Option<i64>,
) -> FrameResult<Option<Vec<u8>>> {
    if !str_to_date_finish_native_args_valid(state, no_zero_date, allow_invalid_dates) {
        return Err(NativeIdentityFrameError::Invalid);
    }
    let bound = native_str_to_date_output_bound(state).ok_or(NativeIdentityFrameError::Capacity)?;
    let (value, input) = read_state(state.expect("validated state")).expect("validated date state");
    // The caller has now performed the original date_modes getter, including
    // the month-zero case where the flags themselves cannot affect the answer.
    if value.month == 0 {
        return warning_report(true, input, bound);
    }
    let max_day = if allow_invalid_dates == Some(1) || value.month == 0 {
        31
    } else {
        Time::native_days_in_month(value.year, value.month)
    };
    if value.month > 12 || value.day > max_day {
        return if value.month == 0 {
            warning_report(true, input, bound)
        } else {
            invalid_date_warning_report(&value, bound)
        };
    }
    if no_zero_date == Some(1) && (value.year == 0 || value.month == 0 || value.day == 0) {
        return warning_report(value.saw_date && value.month == 0, input, bound);
    }
    if value.year == 0 && value.month == 0 && value.day == 0 {
        return warning_report(true, input, bound);
    }
    let date = format!("{:04}-{:02}-{:02}", value.year, value.month, value.day);
    let result = if value.saw_time || value.saw_fraction {
        format!("{date} {}", render_clock(&value))
    } else {
        date
    };
    text_report(0, &result, bound)
}

pub(crate) fn evaluate_str_to_date_typed_finish_native(
    report: Option<&[u8]>,
    no_zero_date: Option<i64>,
) -> FrameResult<Option<Vec<u8>>> {
    if !str_to_date_typed_finish_native_args_valid(report, no_zero_date) {
        return Err(NativeIdentityFrameError::Invalid);
    }
    let bound =
        native_str_to_date_output_bound(report).ok_or(NativeIdentityFrameError::Capacity)?;
    if no_zero_date == Some(1) {
        return Ok(None);
    }
    let Some(NativeStrToDateResult::NeedTypedDateMode(text)) =
        report.and_then(decode_native_str_to_date_result)
    else {
        unreachable!()
    };
    text_report(0, &format!("0000-00-00 {text}"), bound)
}

// Err(true) is the original empty-Bytes month-zero sentinel; Err(false) is
// zero-time failure. This classifier stays in the SDK, never in the adapter.
fn scan(input: &str, format: &str) -> std::result::Result<ParsedDateTime, bool> {
    let date: Vec<char> = input.chars().collect();
    let format: Vec<char> = format.chars().collect();
    let mut value = ParsedDateTime::default();
    let mut date_pos = 0;
    let mut format_pos = 0;
    while format_pos < format.len() {
        skip_parser_whitespace(&date, &mut date_pos);
        skip_parser_whitespace(&format, &mut format_pos);
        if format_pos >= format.len() {
            break;
        }
        let token = format[format_pos];
        format_pos += 1;
        if token != '%' {
            if date.get(date_pos) != Some(&token) {
                return Err(month_zero(&value));
            }
            date_pos += 1;
            continue;
        }
        let Some(specifier) = format.get(format_pos).copied() else {
            return Err(month_zero(&value));
        };
        format_pos += 1;
        if date_pos >= date.len() {
            match specifier {
                'p' => {
                    value.am_pm = Some(false);
                    break;
                }
                'H' | 'k' => {
                    value.saw_24_hour = true;
                    break;
                }
                'h' | 'I' | 'l' => {
                    value.saw_12_hour = true;
                    break;
                }
                'f' | '@' | '#' | '.' => {}
                _ => break,
            }
        }
        match specifier {
            'Y' | 'y' => {
                let limit = if specifier == 'Y' { 4 } else { 2 };
                let Some((raw, consumed)) = parse_ascii_digits(&date[date_pos..], limit) else {
                    return Err(month_zero(&value));
                };
                value.year = Time::native_expand_date_year(raw, consumed);
                value.saw_date = true;
                date_pos += consumed;
            }
            'm' | 'c' => {
                let Some((month, consumed)) = parse_ascii_digits(&date[date_pos..], 2) else {
                    return Err(month_zero(&value));
                };
                // Assignment precedes the range check: month99 is NOT month0.
                value.month = month;
                value.saw_date = true;
                date_pos += consumed;
                if month > 12 {
                    return Err(month_zero(&value));
                }
            }
            'd' | 'e' => {
                let Some((day, consumed)) = parse_ascii_digits(&date[date_pos..], 2) else {
                    return Err(month_zero(&value));
                };
                if day > 31 {
                    return Err(month_zero(&value));
                }
                value.day = day;
                value.saw_date = true;
                date_pos += consumed;
            }
            'j' => {
                let Some((day, consumed)) = parse_ascii_digits(&date[date_pos..], 3) else {
                    return Err(month_zero(&value));
                };
                value.day = day;
                value.saw_date = true;
                date_pos += consumed;
            }
            'H' | 'k' => {
                let Some((hour, consumed)) = parse_ascii_digits(&date[date_pos..], 2) else {
                    return Err(month_zero(&value));
                };
                if hour > 23 {
                    return Err(month_zero(&value));
                }
                value.hour = hour;
                value.saw_time = true;
                value.saw_24_hour = true;
                date_pos += consumed;
            }
            'h' | 'I' | 'l' => {
                let Some((hour, consumed)) = parse_ascii_digits(&date[date_pos..], 2) else {
                    return Err(month_zero(&value));
                };
                if hour == 0 || hour > 12 {
                    return Err(month_zero(&value));
                }
                value.hour = hour;
                value.saw_time = true;
                value.saw_12_hour = true;
                date_pos += consumed;
            }
            'i' => {
                let Some((minute, consumed)) = parse_ascii_digits(&date[date_pos..], 2) else {
                    return Err(month_zero(&value));
                };
                if minute > 59 {
                    return Err(month_zero(&value));
                }
                value.minute = minute;
                value.saw_time = true;
                date_pos += consumed;
            }
            's' | 'S' => {
                let Some((second, consumed)) = parse_ascii_digits(&date[date_pos..], 2) else {
                    return Err(month_zero(&value));
                };
                if second > 59 {
                    return Err(month_zero(&value));
                }
                value.second = second;
                value.saw_time = true;
                date_pos += consumed;
            }
            'f' => {
                let (microsecond, consumed) = parse_ascii_digits(&date[date_pos..], 6)
                    .map_or((0, 0), |(raw, consumed)| {
                        (raw * 10u32.pow(6 - consumed as u32), consumed)
                    });
                value.microsecond = microsecond;
                value.saw_fraction = true;
                value.saw_time = true;
                date_pos += consumed;
            }
            'p' => {
                let Some(am_pm) = parse_am_pm(&date[date_pos..]) else {
                    return Err(month_zero(&value));
                };
                if value.saw_24_hour {
                    return Err(month_zero(&value));
                }
                value.am_pm = Some(am_pm);
                date_pos += 2;
            }
            'r' => {
                let Some((hour, minute, second, am_pm, consumed)) =
                    parse_time_12(&date[date_pos..])
                else {
                    return Err(month_zero(&value));
                };
                value.hour = hour;
                value.minute = minute;
                value.second = second;
                value.saw_time = true;
                value.saw_12_hour = true;
                value.am_pm = am_pm;
                date_pos += consumed;
            }
            'T' => {
                let Some((hour, minute, second, consumed)) = parse_time_24(&date[date_pos..])
                else {
                    return Err(month_zero(&value));
                };
                value.hour = hour;
                value.minute = minute;
                value.second = second;
                value.saw_time = true;
                value.saw_24_hour = true;
                date_pos += consumed;
            }
            '@' => skip_parser_class(&date, &mut date_pos, |c| c.is_ascii_alphabetic()),
            '#' => skip_parser_class(&date, &mut date_pos, |c| c.is_ascii_digit()),
            '.' => skip_parser_class(&date, &mut date_pos, native_is_go_punctuation),
            _ => return Err(false),
        }
    }
    if let Some(am_pm) = value.am_pm {
        if value.saw_24_hour || !value.saw_12_hour {
            return Err(month_zero(&value));
        }
        value.hour = if value.hour == 12 {
            if am_pm { 12 } else { 0 }
        } else if am_pm {
            value.hour + 12
        } else {
            value.hour
        };
    }
    Ok(value)
}

fn month_zero(value: &ParsedDateTime) -> bool {
    value.saw_date && value.month == 0
}

fn skip_parser_whitespace(input: &[char], position: &mut usize) {
    while input
        .get(*position)
        .is_some_and(|character| character.is_whitespace())
    {
        *position += 1;
    }
}

fn parse_ascii_digits(input: &[char], limit: usize) -> Option<(u32, usize)> {
    let mut value = 0u32;
    let mut consumed = 0;
    while consumed < limit {
        let Some(character) = input.get(consumed) else {
            break;
        };
        let Some(digit) = character.to_digit(10).filter(|_| character.is_ascii()) else {
            break;
        };
        value = value.checked_mul(10)?.checked_add(digit)?;
        consumed += 1;
    }
    (consumed > 0).then_some((value, consumed))
}

fn skip_parser_class(input: &[char], position: &mut usize, predicate: fn(char) -> bool) {
    while input
        .get(*position)
        .is_some_and(|character| predicate(*character))
    {
        *position += 1;
    }
}

fn parse_am_pm(input: &[char]) -> Option<bool> {
    let [first, second, ..] = input else {
        return None;
    };
    match (first.to_ascii_lowercase(), second.to_ascii_lowercase()) {
        ('a', 'm') => Some(false),
        ('p', 'm') => Some(true),
        _ => None,
    }
}

fn parse_time_24(input: &[char]) -> Option<(u32, u32, u32, usize)> {
    let mut position = 0;
    let (hour, consumed) = parse_ascii_digits(&input[position..], 2)?;
    if hour > 23 {
        return None;
    }
    position += consumed;
    skip_parser_whitespace(input, &mut position);
    if input.get(position) != Some(&':') {
        return None;
    }
    position += 1;
    skip_parser_whitespace(input, &mut position);
    let (minute, consumed) = parse_ascii_digits(&input[position..], 2)?;
    if minute > 59 {
        return None;
    }
    position += consumed;
    skip_parser_whitespace(input, &mut position);
    if input.get(position) != Some(&':') {
        return None;
    }
    position += 1;
    skip_parser_whitespace(input, &mut position);
    let (second, consumed) = parse_ascii_digits(&input[position..], 2)?;
    if second > 59 {
        return None;
    }
    position += consumed;
    Some((hour, minute, second, position))
}

fn parse_time_12(input: &[char]) -> Option<(u32, u32, u32, Option<bool>, usize)> {
    let mut position = 0;
    let (hour, consumed) = parse_ascii_digits(&input[position..], 2)?;
    if hour == 0 || hour > 12 {
        return None;
    }
    position += consumed;
    skip_parser_whitespace(input, &mut position);
    if input.get(position) != Some(&':') {
        return None;
    }
    position += 1;
    skip_parser_whitespace(input, &mut position);
    let (minute, consumed) = parse_ascii_digits(&input[position..], 2)?;
    if minute > 59 {
        return None;
    }
    position += consumed;
    skip_parser_whitespace(input, &mut position);
    if input.get(position) != Some(&':') {
        return None;
    }
    position += 1;
    skip_parser_whitespace(input, &mut position);
    let (second, consumed) = parse_ascii_digits(&input[position..], 2)?;
    if second > 59 {
        return None;
    }
    position += consumed;
    skip_parser_whitespace(input, &mut position);
    let am_pm = if let Some(am_pm) = parse_am_pm(&input[position..]) {
        position += 2;
        Some(am_pm)
    } else if position == input.len() {
        None
    } else {
        return None;
    };
    Some((hour, minute, second, am_pm, position))
}

fn transport_error(error: NativeIdentityFrameError) -> tidb_query_common::Error {
    other_err!("Invalid native STR_TO_DATE transport: {:?}", error)
}

#[rpn_fn(nullable)]
fn str_to_date_head_native(
    input: Option<BytesRef>,
    format: Option<BytesRef>,
    target: Option<&Int>,
) -> Result<Option<Bytes>> {
    evaluate_str_to_date_head_native(input, format, target.copied()).map_err(transport_error)
}

#[rpn_fn(nullable)]
fn str_to_date_finish_native(
    state: Option<BytesRef>,
    no_zero_date: Option<&Int>,
    allow_invalid_dates: Option<&Int>,
) -> Result<Option<Bytes>> {
    evaluate_str_to_date_finish_native(state, no_zero_date.copied(), allow_invalid_dates.copied())
        .map_err(transport_error)
}

#[rpn_fn(nullable)]
fn str_to_date_typed_finish_native(
    report: Option<BytesRef>,
    no_zero_date: Option<&Int>,
) -> Result<Option<Bytes>> {
    evaluate_str_to_date_typed_finish_native(report, no_zero_date.copied()).map_err(transport_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_stages_preserve_unpacked_day_warning_choice_and_late_mode_demand() {
        let head = |input: &str, format: &str, target| {
            evaluate_str_to_date_head_native(
                Some(input.as_bytes()),
                Some(format.as_bytes()),
                target,
            )
            .unwrap()
            .unwrap()
        };
        let finish = |state: &[u8], no_zero, invalid| {
            evaluate_str_to_date_finish_native(Some(state), Some(no_zero), Some(invalid))
                .unwrap()
                .unwrap()
        };
        let date = head("2020 999 02", "%Y %j %m", Some(12));
        assert!(matches!(
            decode_native_str_to_date_result(&date),
            Some(NativeStrToDateResult::NeedDateModes(_))
        ));
        assert_eq!(read_state(&date).unwrap().0.day, 999);
        assert!(matches!(
            decode_native_str_to_date_result(&finish(&date, 0, 1)),
            Some(NativeStrToDateResult::Warning { code: 1292, .. })
        ));
        let missing_month = head("2020", "%Y", None);
        assert_eq!(missing_month[0], 3); // even month0 MUST request the getter
        assert!(matches!(
            decode_native_str_to_date_result(&finish(&missing_month, 0, 0)),
            Some(NativeStrToDateResult::Warning {
                code: 1411,
                message: "Incorrect datetime value: '2020' for function str_to_date"
            })
        ));
        assert_eq!(head("2020-99", "%Y-%m", None)[0], 1);
        assert_eq!(head("2020 x", "%Y-%m", None)[0], 2); // fails before requesting modes
        assert_eq!(head("2020 x", "%Y %M", None)[0], 1); // unknown token bypasses sentinel
        let partial = head("1", "%m", None);
        assert!(matches!(
            decode_native_str_to_date_result(&finish(&partial, 0, 0)),
            Some(NativeStrToDateResult::Value("0000-01-00"))
        ));
        assert_eq!(finish(&partial, 1, 0)[0], 1);
        let invalid_date = head("2021-02-29", "%Y-%m-%d", None);
        assert!(matches!(
            decode_native_str_to_date_result(&finish(&invalid_date, 0, 0)),
            Some(NativeStrToDateResult::Warning {
                code: 1292,
                message: "Incorrect datetime value: '2021-02-29 00:00:00'"
            })
        ));
        assert!(matches!(
            decode_native_str_to_date_result(&finish(&invalid_date, 0, 1)),
            Some(NativeStrToDateResult::Value("2021-02-29"))
        ));
        assert!(matches!(
            decode_native_str_to_date_result(&head("12", "%h", None)),
            Some(NativeStrToDateResult::Value("12:00:00"))
        ));
        assert!(matches!(
            decode_native_str_to_date_result(&head("12", "%h", Some(-13))),
            Some(NativeStrToDateResult::Value("12:00:00"))
        )); // actual Unknown(12)
        // Exhausted %h records presence without saw_time; earlier PM may still
        // change hour. Date-only rendering must keep dropping that hidden hour.
        let hidden_clock = head("2020-01-01 PM", "%Y-%m-%d %p%h", None);
        assert_eq!(read_state(&hidden_clock).unwrap().0.hour, 12);
        assert!(matches!(
            decode_native_str_to_date_result(&finish(&hidden_clock, 0, 0)),
            Some(NativeStrToDateResult::Value("2020-01-01"))
        ));
        let typed = head("12", "%h", Some(12));
        assert!(matches!(
            decode_native_str_to_date_result(&typed),
            Some(NativeStrToDateResult::NeedTypedDateMode("12:00:00"))
        ));
        assert_eq!(
            evaluate_str_to_date_typed_finish_native(Some(&typed), Some(1)).unwrap(),
            None
        );
        let prefixed = evaluate_str_to_date_typed_finish_native(Some(&typed), Some(0))
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_str_to_date_result(&prefixed),
            Some(NativeStrToDateResult::Value("0000-00-00 12:00:00"))
        );
        assert_eq!(head("12:", "%r", None)[0], 1); // not the broader datatype parser
        assert_eq!(head("零2020", "%@%Y", None)[0], 1); // ASCII skip classes only
        assert_eq!(head("", "%p", None)[0], 1);
        assert!(matches!(
            decode_native_str_to_date_result(&head("12", "%h%p", None)),
            Some(NativeStrToDateResult::Value("00:00:00"))
        ));
        assert!(matches!(
            decode_native_str_to_date_result(&head("12:34:56.tail", "%T.%f", None)),
            Some(NativeStrToDateResult::Value("12:34:56.000000"))
        ));
        assert!(str_to_date_head_native_args_valid(
            None,
            Some(b""),
            Some(255)
        ));
        assert!(!str_to_date_head_native_args_valid(
            None,
            Some(b""),
            Some(256)
        ));
        assert_eq!(
            evaluate_str_to_date_head_native(None, Some(b"%Y"), None).unwrap(),
            None
        );
        assert!(!str_to_date_finish_native_args_valid(
            Some(&typed),
            Some(0),
            Some(0)
        ));
        assert!(!str_to_date_typed_finish_native_args_valid(
            Some(&date),
            Some(0)
        ));
        assert!(date.capacity() <= native_str_to_date_output_bound(Some(b"2020 999 02")).unwrap());
        assert!(decode_native_str_to_date_result(&[3]).is_none());
        assert!(decode_native_str_to_date_result(&[4]).is_none());
    }
}
