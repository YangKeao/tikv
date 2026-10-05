// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native interval parsing, distinct from the wire interval conversion policy.
//! Keep the original integer widths, unchecked arithmetic and truncation flag.

use super::{NativeFspError, NativeTimeError, Time};

/// Parsed `INTERVAL` value before it is applied to a date or duration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParsedInterval {
    /// Calendar years.
    pub years: i64,
    /// Calendar months.
    pub months: i64,
    /// Whole days.
    pub days: i64,
    /// Signed sub-day nanoseconds.
    pub nanoseconds: i64,
    /// Fractional-seconds precision.
    pub fsp: u8,
    /// Whether the value carries a truncation diagnostic.
    pub truncated: bool,
}

/// Parses a native interval without discarding its truncation diagnostic.
pub fn native_parse_duration_value(
    unit: &str,
    format: &str,
) -> Result<ParsedInterval, NativeTimeError> {
    let unit = unit.to_ascii_uppercase();
    match unit.as_str() {
        "MICROSECOND" | "SECOND" | "MINUTE" | "HOUR" | "DAY" | "WEEK" | "MONTH" | "QUARTER"
        | "YEAR" => parse_single_interval(&unit, format),
        "SECOND_MICROSECOND" => parse_composite_interval(format, 6, 2),
        "MINUTE_MICROSECOND" => parse_composite_interval(format, 6, 3),
        "MINUTE_SECOND" => parse_composite_interval(format, 5, 2),
        "HOUR_MICROSECOND" => parse_composite_interval(format, 6, 4),
        "HOUR_SECOND" => parse_composite_interval(format, 5, 3),
        "HOUR_MINUTE" => parse_composite_interval(format, 4, 2),
        "DAY_MICROSECOND" => parse_composite_interval(format, 6, 5),
        "DAY_SECOND" => parse_composite_interval(format, 5, 4),
        "DAY_MINUTE" => parse_composite_interval(format, 4, 3),
        "DAY_HOUR" => parse_composite_interval(format, 3, 2),
        "YEAR_MONTH" => parse_composite_interval(format, 1, 2),
        _ => Err(NativeTimeError::InvalidUnit(unit)),
    }
}

/// Returns native duration nanoseconds and normalized FSP after the original
/// interval-specific truncation, calendar-part and TIME-range checks.
pub fn native_extract_duration_value(
    unit: &str,
    format: &str,
) -> Result<(i64, i64), NativeTimeError> {
    let parsed = native_parse_duration_value(unit, format)?;
    let unit = unit.to_ascii_uppercase();
    if parsed.truncated {
        return Err(NativeTimeError::InvalidDate);
    }
    if parsed.years != 0 {
        return Err(NativeTimeError::OutOfRange("time"));
    }
    let total_days = parsed
        .days
        .checked_add(parsed.months.saturating_mul(30))
        .ok_or(NativeTimeError::OutOfRange("time"))?;
    let total = total_days
        .checked_mul(86_400_000_000_000)
        .and_then(|days| days.checked_add(parsed.nanoseconds))
        .ok_or(NativeTimeError::OutOfRange("time"))?;
    if unit == "YEAR_MONTH" || total.unsigned_abs() > 3_020_399_999_999_999 {
        return Err(NativeTimeError::OutOfRange("time"));
    }
    let fsp = i64::from(parsed.fsp);
    let fsp = Time::native_normalize_fsp(fsp)
        .ok_or(NativeTimeError::InvalidFsp(NativeFspError::InvalidFsp(fsp)))?;
    Ok((total, fsp))
}

fn parse_single_interval(unit: &str, format: &str) -> Result<ParsedInterval, NativeTimeError> {
    let format = format.trim();
    let (integer_text, fraction_text) = format.split_once('.').unwrap_or((format, ""));
    let integer = integer_text
        .parse::<i64>()
        .map_err(|_| NativeTimeError::InvalidDate)?;
    let sign = if format.starts_with('-') { -1 } else { 1 };
    let fraction_digits: String = fraction_text
        .chars()
        .take(6)
        .take_while(char::is_ascii_digit)
        .collect();
    let fraction_len = fraction_digits.len();
    let mut padded = fraction_digits;
    while padded.len() < 6 {
        padded.push('0');
    }
    let fraction = padded.parse::<i64>().unwrap_or(0) * sign;
    let rounded = integer
        + if fraction.unsigned_abs() >= 500_000 {
            sign
        } else {
            0
        };
    let truncated = !fraction_text.is_empty() && unit != "SECOND";
    let mut parsed = ParsedInterval {
        years: 0,
        months: 0,
        days: 0,
        nanoseconds: 0,
        fsp: 0,
        truncated,
    };
    match unit {
        "MICROSECOND" => {
            parsed.days = rounded / 86_400_000_000;
            parsed.nanoseconds = rounded % 86_400_000_000 * 1_000;
            parsed.fsp = 6;
        }
        "SECOND" => {
            parsed.days = integer / 86_400;
            parsed.nanoseconds = integer % 86_400 * 1_000_000_000 + fraction * 1_000;
            parsed.fsp = fraction_len as u8;
        }
        "MINUTE" => {
            parsed.days = rounded / 1_440;
            parsed.nanoseconds = rounded % 1_440 * 60_000_000_000;
        }
        "HOUR" => {
            parsed.days = rounded / 24;
            parsed.nanoseconds = rounded % 24 * 3_600_000_000_000;
        }
        "DAY" => parsed.days = rounded,
        "WEEK" => parsed.days = rounded * 7,
        "MONTH" => parsed.months = rounded,
        "QUARTER" => parsed.months = rounded * 3,
        "YEAR" => parsed.years = rounded,
        _ => unreachable!("single interval unit was matched by caller"),
    }
    Ok(parsed)
}

fn parse_composite_interval(
    format: &str,
    final_index: usize,
    maximum_fields: usize,
) -> Result<ParsedInterval, NativeTimeError> {
    let negative = format.trim_start().starts_with('-');
    let matches = numeric_fields_in_text(format);
    if matches.len() > maximum_fields {
        return Err(NativeTimeError::InvalidDate);
    }
    let mut fields = [0_i64; 7];
    let mut index = final_index;
    for value in matches.iter().rev() {
        let parsed = value
            .parse::<i64>()
            .map_err(|_| NativeTimeError::InvalidDate)?;
        fields[index] = if negative { -parsed } else { parsed };
        if index == 0 {
            break;
        }
        index -= 1;
    }
    if final_index == 6 {
        let sign = if negative { -1 } else { 1 };
        let mut value = matches.last().copied().unwrap_or("0").to_owned();
        while value.len() < 6 {
            value.push('0');
        }
        fields[6] = value
            .parse::<i64>()
            .map_err(|_| NativeTimeError::InvalidDate)?
            * sign;
    }
    let seconds = fields[3] * 3_600 + fields[4] * 60 + fields[5];
    Ok(ParsedInterval {
        years: fields[0],
        months: fields[1],
        days: fields[2] + seconds / 86_400,
        nanoseconds: seconds % 86_400 * 1_000_000_000 + fields[6] * 1_000,
        fsp: if final_index == 6 { 6 } else { 0 },
        truncated: false,
    })
}

fn numeric_fields_in_text(input: &str) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut start = None;
    for (index, byte) in input.bytes().enumerate() {
        if byte.is_ascii_digit() {
            start.get_or_insert(index);
        } else if let Some(start) = start.take() {
            fields.push(&input[start..index]);
        }
    }
    if let Some(start) = start {
        fields.push(&input[start..]);
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_duration_interval_preserves_parse_and_extraction_boundaries() {
        let seconds = native_parse_duration_value("second", "-1.1234567").unwrap();
        assert_eq!(seconds.nanoseconds, -1_123_456_000);
        assert_eq!(seconds.fsp, 6);
        assert!(!seconds.truncated);
        assert!(format!("{seconds:?}").starts_with("ParsedInterval {"));
        let days = native_parse_duration_value("DAY", "1.5").unwrap();
        assert_eq!(days.days, 2);
        assert!(days.truncated);
        assert_eq!(
            native_extract_duration_value("DAY", "1.5"),
            Err(NativeTimeError::InvalidDate)
        );
        assert_eq!(
            native_extract_duration_value("HOUR_MINUTE", "30"),
            Ok((1_800_000_000_000, 0))
        );
        assert_eq!(
            native_extract_duration_value("SECOND_MICROSECOND", "1.2"),
            Ok((1_200_000_000, 6))
        );
        assert_eq!(
            native_extract_duration_value("SECOND_MICROSECOND", "-1.2"),
            Ok((-1_200_000_000, 6))
        );
        assert_eq!(
            native_parse_duration_value("HOUR_MINUTE", "1:2:3"),
            Err(NativeTimeError::InvalidDate)
        );
        assert_eq!(
            native_extract_duration_value("YEAR_MONTH", "0-0"),
            Err(NativeTimeError::OutOfRange("time"))
        );
        assert_eq!(
            native_extract_duration_value("HOUR", "839"),
            Err(NativeTimeError::OutOfRange("time"))
        );
        assert_eq!(
            native_parse_duration_value(" bad ", "0"),
            Err(NativeTimeError::InvalidUnit(" BAD ".to_owned()))
        );
        #[cfg(debug_assertions)]
        assert!(
            std::panic::catch_unwind(|| native_parse_duration_value("WEEK", "9223372036854775807"))
                .is_err()
        );
    }
}
