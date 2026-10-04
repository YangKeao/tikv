// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native public datatype STR_TO_DATE parsing. This is distinct from both the
//! ordinary expression's string/sentinel parser and the legacy wire parser.
//! Construction and SQL-mode/timezone validation remain caller-owned adapters
//! over NativeTemporalValue's existing constructor and validator.

use unicode_general_category::{GeneralCategory, get_general_category};

use super::{MONTH_NAMES, NativeTemporalValue, NativeTimeError, Time, TimeType};

type ParseResult<T> = std::result::Result<T, NativeTimeError>;

#[derive(Default)]
struct ParsedTime {
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    microsecond: u32,
    hour12: bool,
    hour24: bool,
    meridiem: Option<bool>,
}

/// Parse and fix meridiem, then pack the original field widths without date
/// validation. The boolean reports trailing non-whitespace source characters.
/// The caller must retain its original Time::new and validate(flags, timezone)
/// steps; fractional microseconds are preserved even though result FSP is zero.
pub fn native_parse_str_to_date(
    date: &str,
    format: &str,
) -> ParseResult<(NativeTemporalValue, bool)> {
    let mut parsed = ParsedTime::default();
    let warning = parse_format(&mut parsed, date, format)?;
    fix_meridiem(&mut parsed)?;
    let raw = Time::native_core_from_fields(
        parsed.year as u16,
        parsed.month,
        parsed.day,
        parsed.hour,
        parsed.minute,
        parsed.second,
        parsed.microsecond,
    );
    Ok((
        NativeTemporalValue {
            raw,
            kind: TimeType::DateTime,
            fsp: 0,
        },
        warning,
    ))
}

/// Source get_format_type's (has_time, has_date), not runtime parse success or
/// result-kind inference. A malformed trailing token resets the result unless
/// the original early stop after seeing BOTH kinds already occurred.
#[must_use]
pub fn native_str_to_date_format_type(mut format: &str) -> (bool, bool) {
    let mut is_duration = false;
    let mut is_date = false;
    loop {
        format = format.trim_start_matches(char::is_whitespace);
        if format.is_empty() {
            break;
        }
        let Ok((token, remaining)) = next_token(format) else {
            return (false, false);
        };
        format = remaining;
        if let Some(conversion) = token
            .strip_prefix('%')
            .and_then(|value| value.chars().next())
        {
            match conversion {
                'h' | 'H' | 'i' | 'I' | 's' | 'S' | 'k' | 'l' | 'f' | 'r' | 'T' => {
                    is_duration = true
                }
                'y' | 'Y' | 'm' | 'M' | 'c' | 'b' | 'D' | 'd' | 'e' => is_date = true,
                _ => {}
            }
        }
        if is_duration && is_date {
            break;
        }
    }
    (is_duration, is_date)
}

fn parse_format(parsed: &mut ParsedTime, mut date: &str, mut format: &str) -> ParseResult<bool> {
    loop {
        date = date.trim_start_matches(char::is_whitespace);
        format = format.trim_start_matches(char::is_whitespace);
        if format.is_empty() {
            return Ok(!date.is_empty());
        }
        if date.is_empty() {
            // Exhaustion records presence of the current token only. In
            // particular %H (not %k) records hour24 on this path.
            let (token, _) = next_token(format)?;
            match token {
                "%p" => parsed.meridiem = Some(false),
                "%H" => parsed.hour24 = true,
                "%h" | "%I" | "%l" => parsed.hour12 = true,
                _ => {}
            }
            return Ok(false);
        }
        let (token, remaining_format) = next_token(format)?;
        format = remaining_format;
        date = parse_token(parsed, date, token)?;
    }
}

fn next_token(format: &str) -> ParseResult<(&str, &str)> {
    if let Some(remaining) = format.strip_prefix('%') {
        let Some(character) = remaining.chars().next() else {
            return Err(NativeTimeError::InvalidDate);
        };
        let end = 1 + character.len_utf8();
        Ok((&format[..end], &format[end..]))
    } else {
        let character = format.chars().next().expect("nonempty format");
        let end = character.len_utf8();
        Ok((&format[..end], &format[end..]))
    }
}

fn parse_token<'a>(parsed: &mut ParsedTime, input: &'a str, token: &str) -> ParseResult<&'a str> {
    match token {
        "%b" => parse_month_name(parsed, input, true),
        "%M" => parse_month_name(parsed, input, false),
        "%c" | "%m" => {
            let (value, remaining) = parse_digits(input, 2)?;
            if value > 12 {
                return Err(NativeTimeError::InvalidDate);
            }
            parsed.month = value as u8;
            Ok(remaining)
        }
        "%d" | "%e" => {
            let (value, remaining) = parse_digits(input, 2)?;
            if value > 31 {
                return Err(NativeTimeError::InvalidDate);
            }
            parsed.day = value as u8;
            Ok(remaining)
        }
        "%f" => {
            let (value, digits, remaining) = parse_optional_digits(input, 6);
            parsed.microsecond = value * 10_u32.pow(6 - digits as u32);
            Ok(remaining)
        }
        "%h" | "%I" | "%l" => {
            let (value, remaining) = parse_digits(input, 2)?;
            if !(1..=12).contains(&value) {
                return Err(NativeTimeError::InvalidClock);
            }
            parsed.hour = value as u8;
            parsed.hour12 = true;
            Ok(remaining)
        }
        "%H" | "%k" => {
            let (value, remaining) = parse_digits(input, 2)?;
            if value > 23 {
                return Err(NativeTimeError::InvalidClock);
            }
            parsed.hour = value as u8;
            parsed.hour24 = true;
            Ok(remaining)
        }
        "%i" => {
            let (value, remaining) = parse_digits(input, 2)?;
            if value > 59 {
                return Err(NativeTimeError::InvalidClock);
            }
            parsed.minute = value as u8;
            Ok(remaining)
        }
        "%s" | "%S" => {
            let (value, remaining) = parse_digits(input, 2)?;
            if value > 59 {
                return Err(NativeTimeError::InvalidClock);
            }
            parsed.second = value as u8;
            Ok(remaining)
        }
        "%p" => {
            if has_prefix(input, "AM") {
                parsed.meridiem = Some(false);
                Ok(&input[2..])
            } else if has_prefix(input, "PM") {
                parsed.meridiem = Some(true);
                Ok(&input[2..])
            } else {
                Err(NativeTimeError::InvalidClock)
            }
        }
        "%r" => parse_compound_time(parsed, input, true),
        "%T" => parse_compound_time(parsed, input, false),
        "%Y" => parse_year(parsed, input, 4),
        "%y" => parse_year(parsed, input, 2),
        "%j" => {
            let (value, remaining) = parse_digits(input, 3)?;
            if value == 0 {
                Err(NativeTimeError::InvalidDate)
            } else {
                Ok(remaining)
            }
        }
        "%#" => Ok(skip_while(input, char::is_numeric)),
        "%." => Ok(skip_while(input, native_is_go_punctuation)),
        "%@" => Ok(skip_while(input, char::is_alphabetic)),
        _ if input.starts_with(token) => Ok(&input[token.len()..]),
        _ => Err(NativeTimeError::InvalidDate),
    }
}

fn parse_month_name<'a>(
    parsed: &mut ParsedTime,
    input: &'a str,
    abbreviated: bool,
) -> ParseResult<&'a str> {
    for (index, name) in MONTH_NAMES.iter().enumerate() {
        let candidate = if abbreviated { &name[..3] } else { name };
        if has_prefix(input, candidate) {
            parsed.month = index as u8 + 1;
            return Ok(&input[candidate.len()..]);
        }
    }
    Err(NativeTimeError::InvalidDate)
}

fn parse_year<'a>(parsed: &mut ParsedTime, input: &'a str, limit: usize) -> ParseResult<&'a str> {
    let (year, digits, remaining) = parse_digits_with_len(input, limit)?;
    // The digit scanner guarantees year <= 99 when digits <= 2, so the
    // existing written-width primitive exactly matches source adjust_year.
    parsed.year = Time::native_expand_date_year(year, digits) as i32;
    Ok(remaining)
}

fn parse_compound_time<'a>(
    parsed: &mut ParsedTime,
    input: &'a str,
    twelve_hour: bool,
) -> ParseResult<&'a str> {
    let (hour, _, mut remaining) = parse_digits_with_len(input, 2)?;
    if (twelve_hour && !(1..=12).contains(&hour)) || (!twelve_hour && hour > 23) {
        return Err(NativeTimeError::InvalidClock);
    }
    parsed.hour = if twelve_hour && hour == 12 {
        0
    } else {
        hour as u8
    };
    if remaining.is_empty() {
        return Ok(remaining);
    }
    remaining = parse_separator(remaining)?;
    if remaining.is_empty() {
        return Ok(remaining);
    }
    let (minute, next) = parse_digits(remaining, 2)?;
    if minute > 59 {
        return Err(NativeTimeError::InvalidClock);
    }
    parsed.minute = minute as u8;
    remaining = next;
    if remaining.is_empty() {
        return Ok(remaining);
    }
    remaining = parse_separator(remaining)?;
    if remaining.is_empty() {
        return Ok(remaining);
    }
    let (second, next) = parse_digits(remaining, 2)?;
    if second > 59 {
        return Err(NativeTimeError::InvalidClock);
    }
    parsed.second = second as u8;
    remaining = next.trim_start_matches(char::is_whitespace);
    if !twelve_hour || remaining.is_empty() {
        return Ok(remaining);
    }
    if has_prefix(remaining, "AM") {
        Ok(&remaining[2..])
    } else if has_prefix(remaining, "PM") {
        parsed.hour += 12;
        Ok(&remaining[2..])
    } else {
        Err(NativeTimeError::InvalidClock)
    }
}

fn parse_separator(input: &str) -> ParseResult<&str> {
    let input = input.trim_start_matches(char::is_whitespace);
    let Some(input) = input.strip_prefix(':') else {
        return Err(NativeTimeError::InvalidClock);
    };
    Ok(input.trim_start_matches(char::is_whitespace))
}

fn fix_meridiem(parsed: &mut ParsedTime) -> ParseResult<()> {
    let Some(pm) = parsed.meridiem else {
        if parsed.hour12 && parsed.hour == 12 {
            parsed.hour = 0;
        }
        return Ok(());
    };
    if parsed.hour24 || parsed.hour == 0 {
        return Err(NativeTimeError::InvalidClock);
    }
    if parsed.hour == 12 {
        parsed.hour = if pm { 12 } else { 0 };
    } else if pm {
        parsed.hour += 12;
    }
    Ok(())
}

fn parse_digits(input: &str, limit: usize) -> ParseResult<(u32, &str)> {
    let (value, _, remaining) = parse_digits_with_len(input, limit)?;
    Ok((value, remaining))
}

fn parse_digits_with_len(input: &str, limit: usize) -> ParseResult<(u32, usize, &str)> {
    let (value, digits, remaining) = parse_optional_digits(input, limit);
    if digits == 0 {
        Err(NativeTimeError::InvalidDate)
    } else {
        Ok((value, digits, remaining))
    }
}

fn parse_optional_digits(input: &str, limit: usize) -> (u32, usize, &str) {
    let digits = input
        .as_bytes()
        .iter()
        .take(limit)
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let value = input[..digits].parse().unwrap_or(0);
    (value, digits, &input[digits..])
}

fn has_prefix(input: &str, prefix: &str) -> bool {
    input
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

fn skip_while(input: &str, predicate: impl Fn(char) -> bool) -> &str {
    let consumed = input
        .char_indices()
        .take_while(|(_, character)| predicate(*character))
        .map(|(index, character)| index + character.len_utf8())
        .last()
        .unwrap_or(0);
    &input[consumed..]
}

/// Go's Unicode 15 punctuation set using the pinned Unicode 16 category table,
/// excluding exactly the 13 punctuation code points introduced in Unicode 16.
pub fn native_is_go_punctuation(character: char) -> bool {
    if matches!(
        character,
        '\u{1b4e}'
            | '\u{1b4f}'
            | '\u{1b7f}'
            | '\u{10d6e}'
            | '\u{113d4}'
            | '\u{113d5}'
            | '\u{113d7}'
            | '\u{113d8}'
            | '\u{11be1}'
            | '\u{16d6d}'
            | '\u{16d6e}'
            | '\u{16d6f}'
            | '\u{1e5ff}'
    ) {
        return false;
    }
    matches!(
        get_general_category(character),
        GeneralCategory::ClosePunctuation
            | GeneralCategory::ConnectorPunctuation
            | GeneralCategory::DashPunctuation
            | GeneralCategory::FinalPunctuation
            | GeneralCategory::InitialPunctuation
            | GeneralCategory::OpenPunctuation
            | GeneralCategory::OtherPunctuation
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_public_parser_keeps_trailing_meridiem_validation_and_unicode_policies() {
        let (value, trailing) =
            native_parse_str_to_date("29/fEb/2021 12:34:56.125 tail", "%d/%b/%Y %H:%i:%s.%f")
                .unwrap();
        assert!(trailing);
        assert_eq!(
            Time::native_core_fields(value.raw),
            [2021, 2, 29, 12, 34, 56, 125_000]
        );
        assert_eq!(value.kind, TimeType::DateTime);
        assert_eq!(value.fsp, 0); // parsing keeps hidden fraction; validation is later
        assert!(value.validate(true, false, &chrono::Utc).is_err());
        assert!(value.validate(true, true, &chrono::Utc).is_ok());
        let (partial, trailing) = native_parse_str_to_date("2013-05   ", "%Y-%m").unwrap();
        assert!(!trailing);
        assert_eq!(
            partial.validate(false, false, &chrono::Utc),
            Err(NativeTimeError::ZeroInDate)
        );
        assert!(partial.validate(true, false, &chrono::Utc).is_ok());
        assert_eq!(
            native_parse_str_to_date("", "%p"),
            Err(NativeTimeError::InvalidClock)
        );
        assert_eq!(
            native_parse_str_to_date("12", "%h%H%p"),
            Ok((
                NativeTemporalValue {
                    raw: 0,
                    kind: TimeType::DateTime,
                    fsp: 0,
                },
                false
            ))
        );
        let (clock, _) = native_parse_str_to_date("12:", "%r").unwrap();
        assert_eq!(Time::native_core_fields(clock.raw)[3], 0);
        // %T does not set hour24 in this public parser: the later PM yields
        // 35, and original CoreTime packing truncates it to the five-bit hour.
        let (packed, _) = native_parse_str_to_date("23:00:00 PM", "%T %p").unwrap();
        assert_eq!(Time::native_core_fields(packed.raw)[3], 3);
        assert!(native_parse_str_to_date("é%Z", "é%Z").is_ok());
        let (unicode_skip, _) = native_parse_str_to_date("零²:2020", "%@%#:%Y").unwrap();
        assert_eq!(Time::native_core_fields(unicode_skip.raw)[0], 2020);
        assert_eq!(native_str_to_date_format_type("%Y %H %"), (true, true));
        assert_eq!(native_str_to_date_format_type("%Y %"), (false, false));
        assert_eq!(native_str_to_date_format_type("%j %p"), (false, false));
        assert!(native_is_go_punctuation('¿'));
        assert!(!native_is_go_punctuation('+'));
        for character in [
            '\u{1b4e}',
            '\u{1b4f}',
            '\u{1b7f}',
            '\u{10d6e}',
            '\u{113d4}',
            '\u{113d5}',
            '\u{113d7}',
            '\u{113d8}',
            '\u{11be1}',
            '\u{16d6d}',
            '\u{16d6e}',
            '\u{16d6f}',
            '\u{1e5ff}',
        ] {
            assert!(!native_is_go_punctuation(character));
        }
    }
}
