// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native TIMESTAMPADD computation over the shared temporal value domain.
//! Prefix NULL and demanded-date calls have separate physical signatures, so
//! an undemanded third coercion is never represented by a fabricated SQL NULL.

use crate::{
    native_duration_parse::NativeGoDuration,
    native_time_parse::{NativeDurationDateTime, parse_native_duration_datetime},
};

/// Both original prefix coercions have run. At least one actually yielded
/// SQL NULL; there is no third argument slot in this profile.
pub fn native_timestamp_add_prefix_null_args_valid(
    unit: Option<&[u8]>,
    amount: Option<i64>,
) -> bool {
    (unit.is_none() || amount.is_none())
        && unit.is_none_or(|bytes| std::str::from_utf8(bytes).is_ok())
}

/// The date coercion was demanded by two present prefix values. Its NULL is
/// genuine data. Every raw IEEE-754 bit pattern is admitted, without rounding,
/// finite-value screening, unit classification or datetime parsing here.
pub fn native_timestamp_add_args_valid(
    unit: Option<&[u8]>,
    date: Option<&[u8]>,
    amount: Option<i64>,
) -> bool {
    unit.is_some_and(|bytes| std::str::from_utf8(bytes).is_ok())
        && amount.is_some()
        && date.is_none_or(|bytes| std::str::from_utf8(bytes).is_ok())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeTimestampAddResult<'a> {
    Value(&'a str),
    UnknownUnit,
    IncorrectDateTimeInput,
    IncorrectTimeResult(&'a str),
}

/// Tag 0 carries nonempty UTF-8 output, tags 1 and 2 have no payload, and tag 3
/// carries the complete warning formatted from actual computed result fields.
/// Silent SQL NULL is represented outside this packet by the nullable result.
pub fn decode_native_timestamp_add_result(bytes: &[u8]) -> Option<NativeTimestampAddResult<'_>> {
    let (&tag, tail) = bytes.split_first()?;
    match tag {
        0 | 3 if !tail.is_empty() => {
            let text = std::str::from_utf8(tail).ok()?;
            Some(if tag == 0 {
                NativeTimestampAddResult::Value(text)
            } else {
                NativeTimestampAddResult::IncorrectTimeResult(text)
            })
        }
        1 if tail.is_empty() => Some(NativeTimestampAddResult::UnknownUnit),
        2 if tail.is_empty() => Some(NativeTimestampAddResult::IncorrectDateTimeInput),
        _ => None,
    }
}

pub fn native_timestamp_add_result_valid(bytes: &[u8]) -> bool {
    decode_native_timestamp_add_result(bytes).is_some()
}

fn text_packet(tag: u8, text: String) -> Vec<u8> {
    // Keep allocation/count failures outside the SQL NULL option. No fallible
    // allocation is softened into a business overflow or a silent NULL.
    let mut bytes = Vec::with_capacity(text.len() + 1);
    bytes.push(tag);
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

pub(crate) fn evaluate_native_timestamp_add(
    unit: &str,
    date: Option<&str>,
    amount: f64,
) -> Option<Vec<u8>> {
    let text = date?;
    let Some(base) = parse_native_duration_datetime(text) else {
        return Some(vec![2]);
    };
    if !base.in_range() {
        return Some(vec![2]);
    }
    // Invalid source dates warn before even classifying an unknown unit.
    let unit = unit.to_ascii_uppercase();
    let Some(result) = add_unit_to_time(&unit, base, amount) else {
        return Some(vec![1]);
    };
    let result = result?;
    if !result.in_range() {
        return Some(text_packet(
            3,
            format!(
                "Incorrect time value: '{{{} {} {} {} {} {} {}}}'",
                result.year,
                result.month,
                result.day,
                result.hour,
                result.minute,
                result.second,
                result.micros,
            ),
        ));
    }
    let fsp = if result.micros == 0 { 0 } else { 6 };
    Some(text_packet(
        0,
        NativeDurationDateTime { fsp, ..result }.format(),
    ))
}

/// Outer None is an unknown unit, inner None is the original overflow/NULL.
fn add_unit_to_time(
    unit: &str,
    base: NativeDurationDateTime,
    amount: f64,
) -> Option<Option<NativeDurationDateTime>> {
    // Both expressions and their order are retained. SECOND truncates the
    // scaled microseconds; the other units round the original whole amount.
    let truncated_micros = (amount * 1_000_000.0).trunc();
    let rounded = amount.round();
    let micros = match unit {
        "MICROSECOND" => rounded,
        "SECOND" => truncated_micros,
        "MINUTE" => rounded * 60_000_000.0,
        "HOUR" => rounded * 3_600_000_000.0,
        "DAY" => rounded * 86_400_000_000.0,
        "WEEK" => rounded * 7.0 * 86_400_000_000.0,
        "MONTH" => return Some(add_months(base, rounded, true)),
        "QUARTER" => return Some(add_months(base, rounded * 3.0, false)),
        "YEAR" => return Some(add_months(base, rounded * 12.0, false)),
        _ => return None,
    };
    if !micros.is_finite() || micros.abs() > 9e18 {
        return Some(None);
    }
    Some(base.add(NativeGoDuration {
        micros: micros as i64,
        fsp: base.fsp,
    }))
}

fn add_months(
    base: NativeDurationDateTime,
    months: f64,
    clamp: bool,
) -> Option<NativeDurationDateTime> {
    if !months.is_finite() || months.abs() > 1e6 {
        return None;
    }
    let total = base.year * 12 + i64::from(base.month) - 1 + months as i64;
    if total < 0 {
        return None;
    }
    let year = total / 12;
    let month = (total % 12 + 1) as u32;
    let day = if clamp {
        base.day.min(last_day_of_month(year, month))
    } else {
        base.day
    };
    let (year, month, day) = NativeDurationDateTime::date_from_daynr(
        NativeDurationDateTime::daynr(year, month, 1) + i64::from(day) - 1,
    );
    Some(NativeDurationDateTime {
        year,
        month,
        day,
        ..base
    })
}

fn last_day_of_month(year: i64, month: u32) -> u32 {
    let next = if month == 12 {
        NativeDurationDateTime::daynr(year + 1, 1, 1)
    } else {
        NativeDurationDateTime::daynr(year, month + 1, 1)
    };
    (next - NativeDurationDateTime::daynr(year, month, 1)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_timestamp_add_preserves_units_warning_precedence_null_roles_and_ieee_inputs() {
        let value = |unit, date, amount, expected: &str| {
            let output = evaluate_native_timestamp_add(unit, Some(date), amount).unwrap();
            assert_eq!(
                decode_native_timestamp_add_result(&output),
                Some(NativeTimestampAddResult::Value(expected))
            );
        };
        for (unit, amount, expected) in [
            ("microsecond", 5.0, "2020-01-01 00:00:00.000005"),
            ("SECOND", 1.1, "2020-01-01 00:00:01.100000"),
            ("SECOND", 0.0000099999, "2020-01-01 00:00:00.000009"),
            ("SECOND", -0.0000099999, "2019-12-31 23:59:59.999991"),
            ("MINUTE", 1.5, "2020-01-01 00:02:00"),
            ("HOUR", -0.5, "2019-12-31 23:00:00"),
            ("DAY", 1.0, "2020-01-02 00:00:00"),
            ("WEEK", 1.0, "2020-01-08 00:00:00"),
        ] {
            value(unit, "2020-01-01", amount, expected);
        }
        value("MONTH", "2020-01-31", 1.0, "2020-02-29 00:00:00");
        value("QUARTER", "2020-01-31", 1.0, "2020-05-01 00:00:00");
        value("YEAR", "2020-02-29", 1.0, "2021-03-01 00:00:00");
        value(
            "SECOND",
            "2020-01-01 00:00:00.000000",
            -0.0,
            "2020-01-01 00:00:00",
        );
        assert_eq!(
            evaluate_native_timestamp_add("unknown", None, f64::NAN),
            None
        );
        assert_eq!(
            evaluate_native_timestamp_add("unknown", Some("bad"), f64::NAN),
            Some(vec![2])
        );
        assert_eq!(
            evaluate_native_timestamp_add("unknown", Some("0000-00-00"), f64::NAN),
            Some(vec![2])
        );
        assert_eq!(
            evaluate_native_timestamp_add("unknown", Some("10000-01-01"), f64::NAN),
            Some(vec![2])
        );
        assert_eq!(
            evaluate_native_timestamp_add("unknown", Some("2020-01-01"), f64::NAN),
            Some(vec![1])
        );
        for amount in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 9e18 + 2048.0] {
            assert_eq!(
                evaluate_native_timestamp_add("MICROSECOND", Some("2020-01-01"), amount),
                None
            );
        }
        let output =
            evaluate_native_timestamp_add("MICROSECOND", Some("2020-01-01"), 9e18).unwrap();
        assert_eq!(
            decode_native_timestamp_add_result(&output),
            Some(NativeTimestampAddResult::IncorrectTimeResult(
                "Incorrect time value: '{0 0 0 16 0 0 0}'",
            ))
        );
        assert_eq!(
            evaluate_native_timestamp_add("MONTH", Some("2020-01-01"), 1_000_001.0),
            None
        );
        assert_eq!(
            evaluate_native_timestamp_add("MONTH", Some("2020-01-01"), -1_000_000.0),
            None
        );
        let output =
            evaluate_native_timestamp_add("DAY", Some("0001-01-01 12:34:56.7"), -1.0).unwrap();
        assert_eq!(
            decode_native_timestamp_add_result(&output),
            Some(NativeTimestampAddResult::IncorrectTimeResult(
                "Incorrect time value: '{0 0 0 12 34 56 700000}'",
            ))
        );
        let output =
            evaluate_native_timestamp_add("MONTH", Some("2020-01-01"), 1_000_000.0).unwrap();
        assert_eq!(
            decode_native_timestamp_add_result(&output),
            Some(NativeTimestampAddResult::IncorrectTimeResult(
                "Incorrect time value: '{0 0 0 0 0 0 0}'",
            ))
        );
        for bits in [
            0,
            (-0.0_f64).to_bits() as i64,
            f64::INFINITY.to_bits() as i64,
            0x7ff8_0000_0000_0042_i64,
            i64::MIN,
            i64::MAX,
        ] {
            assert!(native_timestamp_add_args_valid(
                Some(b"DAY"),
                None,
                Some(bits)
            ));
            assert!(native_timestamp_add_args_valid(
                Some(b"DAY"),
                Some(b"unparsed"),
                Some(bits)
            ));
            assert!(native_timestamp_add_prefix_null_args_valid(
                None,
                Some(bits)
            ));
            assert!(!native_timestamp_add_prefix_null_args_valid(
                Some(b"DAY"),
                Some(bits)
            ));
        }
        assert!(native_timestamp_add_prefix_null_args_valid(None, None));
        assert!(native_timestamp_add_prefix_null_args_valid(Some(b""), None));
        assert!(!native_timestamp_add_prefix_null_args_valid(
            Some(&[255]),
            None
        ));
        assert!(!native_timestamp_add_args_valid(None, None, Some(0)));
        assert!(!native_timestamp_add_args_valid(Some(b"DAY"), None, None));
        assert!(!native_timestamp_add_args_valid(
            Some(&[255]),
            None,
            Some(0)
        ));
        assert!(!native_timestamp_add_args_valid(
            Some(b"DAY"),
            Some(&[255]),
            Some(0)
        ));
        for packet in [
            vec![],
            vec![0],
            vec![3],
            vec![0, 255],
            vec![3, 255],
            vec![1, 0],
            vec![2, 0],
            vec![4],
        ] {
            assert!(!native_timestamp_add_result_valid(&packet));
        }
        assert_eq!(
            decode_native_timestamp_add_result(&[1]),
            Some(NativeTimestampAddResult::UnknownUnit)
        );
        assert_eq!(
            decode_native_timestamp_add_result(&[2]),
            Some(NativeTimestampAddResult::IncorrectDateTimeInput)
        );
    }
}
