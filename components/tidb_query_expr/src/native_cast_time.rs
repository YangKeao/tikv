// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native calendar CAST control. The two views describe the same actual datum;
//! context callbacks are data getters or the original warning effect only.
use std::fmt::Debug;

use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use tidb_query_datatype::codec::{
    convert::native_warning_subject_byte_cap,
    mysql::{
        Decimal, Time,
        time::{
            NativeTemporalValue, TimeType, native_get_time_fsp, native_parse_time,
            native_parse_time_from_decimal_text, native_parse_time_from_float64,
            native_parse_time_from_num,
        },
    },
    native_duration_convert::NativeDurationParts,
    native_numeric::{NativeNumericInput, native_datum_to_i64},
    native_sql_string::NativeSqlStringInput,
    native_temporal_convert::{
        native_duration_convert_to_time, native_parse_time_from_year, native_time_round_frac,
    },
    native_type_name::NativeTypeNameCode,
};

use crate::native_coerce_string::native_coerce_string;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeTimeCastModes {
    pub no_zero_date: bool,
    pub no_zero_in_date: bool,
    pub allow_invalid_dates: bool,
}

/// Preserve YEAR and Duration's early returns, including their bypass of the
/// normal DATE clock-clearing tail and their distinct context demand order.
#[allow(clippy::too_many_arguments)]
pub fn native_cast_time<TZ: TimeZone + Debug>(
    text: NativeSqlStringInput<'_>,
    number: NativeNumericInput<'_>,
    source: Option<NativeTypeNameCode>,
    kind: TimeType,
    fsp: Option<i64>,
    mut modes: impl FnMut() -> NativeTimeCastModes,
    now: impl FnOnce() -> Option<(i64, u32, i32)>,
    mut zone: impl FnMut() -> TZ,
    mut warning: impl FnMut(u16, &str),
) -> Result<Option<NativeTemporalValue>, &'static str> {
    if let Some(year) = year_source_value(number, source) {
        return native_parse_time_from_year(year)
            .map(Some)
            .map_err(|_| "a YEAR value outside the year range");
    }
    if let NativeSqlStringInput::Duration {
        nanoseconds,
        fsp: source_fsp,
    } = text
    {
        let modes = modes();
        let (utc_secs, nanos, tz_offset) = now().ok_or("no statement clock for a TIME cast")?;
        let fixed =
            FixedOffset::east_opt(tz_offset).ok_or("session time-zone offset out of range")?;
        let Some(stamp) = DateTime::<Utc>::from_timestamp(utc_secs, nanos) else {
            return Ok(None);
        };
        return Ok(native_duration_convert_to_time(
            NativeDurationParts {
                nanoseconds,
                fsp: source_fsp,
            },
            stamp.with_timezone(&fixed),
            kind,
            !modes.no_zero_in_date,
            modes.allow_invalid_dates,
        )
        .and_then(|time| match fsp {
            Some(fsp) => native_time_round_frac(time, fsp, &zone()),
            None => Ok(time),
        })
        .ok());
    }
    let Some(s) = native_coerce_string(text)? else {
        return Ok(None);
    };
    let modes = modes();
    let parsed = parse_time_by_source(number, &s, kind, fsp, modes.allow_invalid_dates, &zone());
    let Ok((time, truncated, dst_adjusted)) = parsed else {
        match number {
            NativeNumericInput::String(_)
            | NativeNumericInput::Bytes(_)
            | NativeNumericInput::Decimal(_)
            | NativeNumericInput::Real(_)
            | NativeNumericInput::Float32(_) => {
                invalid_time_warning(&s, fsp.unwrap_or(0), &mut warning)
            }
            _ => {
                let signed = match number {
                    NativeNumericInput::UInt(value) => format!("{}", value as i64),
                    _ => native_datum_to_i64(number, &Utc)
                        .map(|converted| format!("{}", converted.value))
                        .unwrap_or_else(|_| s.clone()),
                };
                warning(1292, &format!("Incorrect time value: '{signed}'"));
            }
        }
        return Ok(None);
    };
    if truncated {
        warning(
            1292,
            &format!(
                "Truncated incorrect datetime value: '{}'",
                native_warning_subject_byte_cap(&s)
            ),
        );
    }
    if dst_adjusted {
        warning(
            8179,
            &format!(
                "Timestamp is not valid, since it is in Daylight Saving Time transition '{}' for time zone '{:?}'",
                s,
                zone()
            ),
        );
    }
    if matches!(
        number,
        NativeNumericInput::String(_) | NativeNumericInput::Bytes(_)
    ) && time.raw == 0
        && modes.no_zero_date
    {
        invalid_time_warning(&s, 6, &mut warning);
        return Ok(None);
    }
    Ok(Some(truncate_clock_for_date(time, kind)))
}

/// Argument wrapping returns existing temporal storage (and NULL) before any
/// coercion, source-type check, clock access, mode access or zone access.
pub fn native_cast_arg_as_datetime<TZ: TimeZone + Debug>(
    text: NativeSqlStringInput<'_>,
    number: NativeNumericInput<'_>,
    source: Option<NativeTypeNameCode>,
    modes: impl FnMut() -> NativeTimeCastModes,
    now: impl FnOnce() -> Option<(i64, u32, i32)>,
    zone: impl FnMut() -> TZ,
    warning: impl FnMut(u16, &str),
) -> Result<Option<NativeTemporalValue>, &'static str> {
    match text {
        NativeSqlStringInput::Time(time) => Ok(Some(time)),
        NativeSqlStringInput::Null => Ok(None),
        _ => native_cast_time(
            text,
            number,
            source,
            TimeType::DateTime,
            None,
            modes,
            now,
            zone,
            warning,
        ),
    }
}

fn year_source_value(
    input: NativeNumericInput<'_>,
    source: Option<NativeTypeNameCode>,
) -> Option<i64> {
    if source? != NativeTypeNameCode::Known(13) {
        return None;
    }
    match input {
        NativeNumericInput::Int(value) => Some(value),
        NativeNumericInput::UInt(value) => i64::try_from(value).ok(),
        _ => None,
    }
}
fn truncate_clock_for_date(mut time: NativeTemporalValue, kind: TimeType) -> NativeTemporalValue {
    if kind != TimeType::Date {
        return time;
    }
    let fields = Time::native_core_fields(time.raw);
    time.raw = Time::native_core_from_fields(
        fields[0] as u16,
        fields[1] as u8,
        fields[2] as u8,
        0,
        0,
        0,
        0,
    );
    time
}
fn parse_time_by_source<TZ: TimeZone>(
    input: NativeNumericInput<'_>,
    text: &str,
    kind: TimeType,
    fsp: Option<i64>,
    allow_invalid: bool,
    zone: &TZ,
) -> Result<(NativeTemporalValue, bool, bool), ()> {
    use NativeNumericInput as I;
    match input {
        I::Int(value) => native_parse_time_from_num(
            value,
            kind,
            fsp.unwrap_or(0),
            true,
            allow_invalid,
            true,
            zone,
        )
        .into_result()
        .map(|parsed| (parsed.time, false, parsed.dst_adjusted))
        .map_err(|_| ()),
        I::UInt(value) => {
            let signed = i64::try_from(value).map_err(|_| ())?;
            native_parse_time_from_num(
                signed,
                kind,
                fsp.unwrap_or(0),
                true,
                allow_invalid,
                true,
                zone,
            )
            .into_result()
            .map(|parsed| (parsed.time, false, parsed.dst_adjusted))
            .map_err(|_| ())
        }
        I::Decimal(value) => {
            // Native Decimal Display uses exactly this visible-scale formatter.
            let text = Decimal::native_format_visible(
                value.negative,
                value.digits,
                value.scale,
                value.storage_scale,
            );
            let mut time = native_parse_time_from_decimal_text(&text, true, allow_invalid, zone)
                .into_result()
                .map_err(|_| ())?;
            time.set_kind(kind);
            match fsp {
                Some(fsp) => native_time_round_frac(time, fsp, zone)
                    .map(|time| (time, false, false))
                    .map_err(|_| ()),
                None => Ok((time, false, false)),
            }
        }
        I::Real(value) | I::Float32(value) => {
            real_to_time(value, kind, fsp.unwrap_or(0), allow_invalid, zone)
                .map(|time| (time, false, false))
        }
        I::Time(mut time) => {
            time.set_kind(kind);
            match fsp {
                Some(fsp) => native_time_round_frac(time, fsp, zone)
                    .map(|time| (time, false, false))
                    .map_err(|_| ()),
                None => Ok((time, false, false)),
            }
        }
        _ => native_parse_time(
            text,
            kind,
            fsp.unwrap_or_else(|| i64::from(native_get_time_fsp(text))),
            false,
            true,
            allow_invalid,
            true,
            zone,
        )
        .map(|parsed| (parsed.time, parsed.truncated, parsed.dst_adjusted))
        .map_err(|_| ()),
    }
}
fn real_to_time<TZ: TimeZone>(
    value: f64,
    kind: TimeType,
    fsp: i64,
    allow_invalid: bool,
    zone: &TZ,
) -> Result<NativeTemporalValue, ()> {
    let mut time = native_parse_time_from_float64(value, true, allow_invalid, zone)
        .into_result()
        .map_err(|_| ())?;
    time.set_kind(kind);
    native_time_round_frac(time, fsp, zone).map_err(|_| ())
}
/// Retain the source diagnostic shape classifier, including unchecked digit
/// accumulation and the uncapped zero-date fraction width. These are not parser
/// fixes or replacements for the frozen warning policy.
fn invalid_time_warning(input: &str, fsp: i64, warning: &mut impl FnMut(u16, &str)) {
    let trimmed = input.trim();
    let head: Vec<&str> = trimmed.splitn(3, '-').collect();
    if head.len() == 3
        && !head[0].is_empty()
        && !head[1].is_empty()
        && head[0].bytes().all(|b| b.is_ascii_digit())
        && head[1].bytes().all(|b| b.is_ascii_digit())
    {
        let digits = |part: &str| -> i64 {
            part.bytes()
                .take_while(|byte| byte.is_ascii_digit())
                .fold(0i64, |acc, byte| acc * 10 + i64::from(byte - b'0'))
        };
        let (year, month) = (digits(head[0]), digits(head[1]));
        let day_digits = head[2]
            .bytes()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        let day = digits(&head[2][..day_digits]);
        if year == 0 && month == 0 && day == 0 {
            let fraction = if fsp > 0 {
                format!(".{:0width$}", 0, width = fsp as usize)
            } else {
                String::new()
            };
            warning(
                1292,
                &format!("Incorrect datetime value: '0000-00-00 00:00:00{fraction}'"),
            );
            return;
        }
        if (1..=12).contains(&month) && (1..=31).contains(&day) {
            let days_in_month = match month {
                1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
                4 | 6 | 9 | 11 => 30,
                _ => {
                    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
                    if leap { 29 } else { 28 }
                }
            };
            if day <= days_in_month {
                warning(8034, &format!("Incorrect datetime value: '{input}'"));
                return;
            }
            if head[2].bytes().all(|byte| byte.is_ascii_digit()) {
                let rendered = format!("{year}-{month}-{day}");
                warning(1292, &format!("Incorrect datetime value: '{rendered}'"));
                return;
            }
        }
    }
    warning(1292, &format!("Incorrect datetime value: '{input}'"));
}

#[cfg(test)]
mod tests {
    use NativeNumericInput as N;
    use NativeSqlStringInput as I;
    use tidb_query_datatype::codec::mysql::NativeDecimalParseRef;

    use super::*;
    const MODES: NativeTimeCastModes = NativeTimeCastModes {
        no_zero_date: true,
        no_zero_in_date: true,
        allow_invalid_dates: false,
    };
    #[test]
    fn calendar_cast_source_precision_and_diagnostic_policy() {
        let mut warnings = Vec::new();
        let zero = native_cast_time(
            I::Int(0),
            N::Int(0),
            None,
            TimeType::DateTime,
            Some(0),
            || MODES,
            || panic!("numeric clock read"),
            || Utc,
            |c, m| warnings.push((c, m.to_owned())),
        )
        .unwrap()
        .unwrap();
        assert_eq!(zero.raw, 0);
        assert!(warnings.is_empty());
        let text = b"0000-00-00";
        assert!(
            native_cast_time(
                I::String(text),
                N::String(text),
                None,
                TimeType::DateTime,
                Some(0),
                || MODES,
                || panic!("string clock read"),
                || Utc,
                |c, m| warnings.push((c, m.to_owned()))
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(
            warnings,
            vec![(
                1292,
                "Incorrect datetime value: '0000-00-00 00:00:00.000000'".into()
            )]
        );
        for (input, code, message) in [
            (
                "2020-01-01x",
                8034,
                "Incorrect datetime value: '2020-01-01x'",
            ),
            ("2020-02-30", 1292, "Incorrect datetime value: '2020-2-30'"),
            ("abc", 1292, "Incorrect datetime value: 'abc'"),
        ] {
            warnings.clear();
            assert!(
                native_cast_time(
                    I::String(input.as_bytes()),
                    N::String(input.as_bytes()),
                    None,
                    TimeType::Date,
                    Some(0),
                    || MODES,
                    || None,
                    || Utc,
                    |c, m| warnings.push((c, m.to_owned()))
                )
                .unwrap()
                .is_none()
            );
            assert_eq!(warnings, vec![(code, message.into())]);
        }
        warnings.clear();
        assert!(
            native_cast_time(
                I::UInt(u64::MAX),
                N::UInt(u64::MAX),
                None,
                TimeType::DateTime,
                Some(0),
                || MODES,
                || None,
                || Utc,
                |c, m| warnings.push((c, m.to_owned()))
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(warnings, vec![(1292, "Incorrect time value: '-1'".into())]);
        warnings.clear();
        assert!(
            native_cast_time(
                I::Enum(b"not a date"),
                N::Enum(37),
                None,
                TimeType::DateTime,
                Some(0),
                || MODES,
                || None,
                || Utc,
                |c, m| warnings.push((c, m.to_owned()))
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(warnings, vec![(1292, "Incorrect time value: '37'".into())]);
        let decimal = NativeDecimalParseRef {
            negative: false,
            digits: b"1212121111",
            scale: 4,
            storage_scale: 4,
            declared_shape: None,
        };
        let time = native_cast_time(
            I::Decimal(decimal),
            N::Decimal(decimal),
            None,
            TimeType::DateTime,
            Some(6),
            || MODES,
            || None,
            || Utc,
            |_, _| panic!("decimal warning"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            Time::native_core_fields(time.raw),
            [2012, 12, 12, 0, 0, 0, 0]
        );
        assert_eq!(time.fsp, 6);
        let raw = NativeTemporalValue {
            raw: Time::native_core_from_fields(2020, 1, 2, 12, 34, 56, 123456),
            kind: TimeType::DateTime,
            fsp: 6,
        };
        let date = native_cast_time(
            I::Time(raw),
            N::Time(raw),
            None,
            TimeType::Date,
            Some(9),
            || MODES,
            || None,
            || Utc,
            |_, _| panic!("DATE warning"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(Time::native_core_fields(date.raw), [2020, 1, 2, 0, 0, 0, 0]);
        assert_eq!(date.fsp, 0);
        let text = b"2020-01-02 12:34:56.123";
        let time = native_cast_time(
            I::String(text),
            N::String(text),
            None,
            TimeType::DateTime,
            None,
            || MODES,
            || None,
            || Utc,
            |_, _| panic!("fsp warning"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(time.fsp, 3);
        assert_eq!(
            Time::native_core_fields(time.raw),
            [2020, 1, 2, 12, 34, 56, 123000]
        );
        if cfg!(debug_assertions) {
            warnings.clear();
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                invalid_time_warning("9223372036854775808-01-01", 0, &mut |c, m| {
                    warnings.push((c, m.to_owned()))
                })
            }));
            assert!(panic.is_err());
            assert!(warnings.is_empty());
        }
    }
    #[test]
    fn calendar_cast_preserves_getter_order_and_year_duration_argument_early_returns() {
        use std::cell::RefCell;
        fn run(
            text: I<'_>,
            number: N<'_>,
            source: Option<NativeTypeNameCode>,
            kind: TimeType,
            fsp: Option<i64>,
            stamp: Option<(i64, u32, i32)>,
            calls: &RefCell<Vec<&'static str>>,
        ) -> Result<Option<NativeTemporalValue>, &'static str> {
            native_cast_time(
                text,
                number,
                source,
                kind,
                fsp,
                || {
                    calls.borrow_mut().push("modes");
                    MODES
                },
                || {
                    calls.borrow_mut().push("now");
                    stamp
                },
                || {
                    calls.borrow_mut().push("zone");
                    Utc
                },
                |_, _| calls.borrow_mut().push("warning"),
            )
        }
        let calls = RefCell::new(Vec::new());
        let year = run(
            I::Int(2018),
            N::Int(2018),
            Some(NativeTypeNameCode::Known(13)),
            TimeType::Date,
            Some(99),
            None,
            &calls,
        )
        .unwrap()
        .unwrap();
        assert_eq!(year.kind, TimeType::DateTime);
        assert_eq!(Time::native_core_fields(year.raw), [2018, 0, 0, 0, 0, 0, 0]);
        assert!(calls.borrow().is_empty());
        assert_eq!(
            run(
                I::Int(-1),
                N::Int(-1),
                Some(NativeTypeNameCode::Known(13)),
                TimeType::Date,
                Some(0),
                None,
                &calls
            )
            .unwrap_err(),
            "a YEAR value outside the year range"
        );
        assert!(calls.borrow().is_empty());
        assert!(
            run(
                I::Null,
                N::Null,
                None,
                TimeType::DateTime,
                None,
                None,
                &calls
            )
            .unwrap()
            .is_none()
        );
        assert!(calls.borrow().is_empty());
        assert_eq!(
            run(
                I::Bytes(&[255]),
                N::Bytes(&[255]),
                None,
                TimeType::DateTime,
                None,
                None,
                &calls
            )
            .unwrap_err(),
            "invalid UTF-8 byte datum"
        );
        assert!(calls.borrow().is_empty());
        let raw = NativeTemporalValue {
            raw: 1,
            kind: TimeType::Timestamp,
            fsp: 9,
        };
        let passthrough = native_cast_arg_as_datetime(
            I::Time(raw),
            N::Time(raw),
            Some(NativeTypeNameCode::Known(13)),
            || panic!("argument modes"),
            || panic!("argument now"),
            || -> Utc { panic!("argument zone") },
            |_, _| panic!("argument warning"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (passthrough.raw, passthrough.kind, passthrough.fsp),
            (1, TimeType::Timestamp, 9)
        );
        assert!(
            native_cast_arg_as_datetime(
                I::Null,
                N::Null,
                None,
                || panic!("NULL modes"),
                || panic!("NULL now"),
                || -> Utc { panic!("NULL zone") },
                |_, _| panic!("NULL warning")
            )
            .unwrap()
            .is_none()
        );
        let duration = NativeDurationParts {
            nanoseconds: 46_800_000_000_000,
            fsp: 0,
        };
        let text = I::Duration {
            nanoseconds: duration.nanoseconds,
            fsp: duration.fsp,
        };
        let number = N::Duration(duration);
        assert_eq!(
            run(
                text,
                number,
                None,
                TimeType::DateTime,
                Some(0),
                None,
                &calls
            )
            .unwrap_err(),
            "no statement clock for a TIME cast"
        );
        assert_eq!(*calls.borrow(), vec!["modes", "now"]);
        calls.borrow_mut().clear();
        assert_eq!(
            run(
                text,
                number,
                None,
                TimeType::DateTime,
                Some(0),
                Some((i64::MAX, 0, 86400)),
                &calls
            )
            .unwrap_err(),
            "session time-zone offset out of range"
        );
        assert_eq!(*calls.borrow(), vec!["modes", "now"]);
        calls.borrow_mut().clear();
        assert!(
            run(
                text,
                number,
                None,
                TimeType::DateTime,
                Some(0),
                Some((i64::MAX, 0, 0)),
                &calls
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(*calls.borrow(), vec!["modes", "now"]);
        for (fsp, expected) in [
            (None, vec!["modes", "now"]),
            (Some(3), vec!["modes", "now", "zone"]),
        ] {
            calls.borrow_mut().clear();
            let date = run(
                text,
                number,
                None,
                TimeType::Date,
                fsp,
                Some((0, 0, -3600)),
                &calls,
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                Time::native_core_fields(date.raw),
                [1969, 12, 31, 13, 0, 0, 0]
            );
            assert_eq!(date.kind, TimeType::Date);
            assert_eq!(*calls.borrow(), expected);
        }
        calls.borrow_mut().clear();
        assert!(
            run(
                text,
                number,
                None,
                TimeType::DateTime,
                // CheckFsp clamps 7 to 6; -2 is rejected (-1 means default).
                Some(-2),
                Some((0, 0, 0)),
                &calls
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(*calls.borrow(), vec!["modes", "now", "zone"]);
        calls.borrow_mut().clear();
        assert!(
            run(
                I::String(b"abc"),
                N::String(b"abc"),
                None,
                TimeType::DateTime,
                None,
                None,
                &calls
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(*calls.borrow(), vec!["modes", "zone", "warning"]);
    }
}
