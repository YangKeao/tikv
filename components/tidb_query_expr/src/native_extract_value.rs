// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Actual EXTRACT selection and arithmetic. Mixed state retains the first
//! parsed duration and original borrowed text, with no encoded continuation.
use chrono::Utc;
use tidb_query_datatype::codec::{
    mysql::{
        Duration, Time, TimeType,
        duration::native_parse_mysql_duration,
        time::{
            native_extract_datetime_num, native_extract_duration_num, native_get_time_fsp,
            native_is_clock_unit, native_is_date_unit, native_parse_time,
        },
    },
    native_sql_string::NativeSqlStringInput as Input,
    native_type_name::NativeTypeNameCode,
};

use crate::native_extract::{
    ZERO_WARNING, datetime_composite_value, duration_composite_value, parse_signed_duration_hms,
    time_parts_with_micros,
};
/// Finite actual scratch, parser, diagnostic or output allowance is exhausted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeExtractWorkError;
type Result<T> = std::result::Result<T, NativeExtractWorkError>;
#[derive(Clone, Copy)]
struct Budget(Option<usize>);
impl Budget {
    fn finite(n: usize) -> Result<Self> {
        if n == usize::MAX {
            Err(NativeExtractWorkError)
        } else {
            Ok(Self(Some(n)))
        }
    }
    fn extent(self, n: usize, m: usize, extra: usize) -> Result<()> {
        if let Some(max) = self.0 {
            let work = n
                .checked_mul(m)
                .and_then(|n| n.checked_add(extra))
                .ok_or(NativeExtractWorkError)?;
            if work > max {
                return Err(NativeExtractWorkError);
            }
        }
        Ok(())
    }
    fn parser(self, unit: &str, text: &str) -> Result<()> {
        if self.0.is_none() {
            return Ok(());
        }
        let n = text
            .len()
            .checked_mul(64)
            .and_then(|n| unit.len().checked_mul(4).and_then(|u| n.checked_add(u)))
            .and_then(|n| n.checked_add(4096))
            .ok_or(NativeExtractWorkError)?;
        self.extent(n, 1, 0)
    }
}
/// Original selected cast route, derived from the actual value and source type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExtractSelection {
    /// Datetime argument wrapper.
    Datetime,
    /// Duration argument wrapper.
    Duration,
    /// Original string wrapper followed by two mixed parse stages.
    Mixed,
}
/// Actual terminal SQL result, not a tagged transport report.
#[derive(Debug, PartialEq)]
pub enum NativeExtractValue {
    /// SQL NULL, including nullable broad-composite input.
    Null,
    /// Original signed extraction arithmetic.
    Value(i64),
    /// Complete original message for hard SQL1105.
    InvalidUnit(String),
    /// Complete original message for hard SQL1292.
    InvalidTime(String),
    /// Original unconditional warning1292 and NULL.
    Warning(&'static str),
}
/// The first actual mixed parse, either a terminal diagnostic or opaque state.
#[derive(Debug)]
pub enum NativeExtractMixedStage<'a> {
    /// Original hard invalid-time result.
    Ready(NativeExtractValue),
    /// Actual accepted duration, before a separately demanded mode.
    NeedDatetime(NativeExtractMixedValue<'a>),
}
/// Opaque actual first-parse state. Finish consumes it once; the second mode
/// never replays the duration parse or substitutes a cached first-mode value.
#[derive(Debug)]
pub struct NativeExtractMixedValue<'a> {
    pub(super) unit: &'a str,
    pub(super) text: &'a str,
    pub(super) nanos: i64,
    pub(super) fsp: i64,
}
#[derive(Clone, Copy)]
pub(super) enum Kind {
    Time,
    Duration,
    Other,
}
fn selection(
    unit: &str,
    source: Option<NativeTypeNameCode>,
    kind: Kind,
    budget: Budget,
) -> Result<NativeExtractSelection> {
    budget.extent(unit.len(), 4, 0)?;
    let clock = native_is_clock_unit(unit);
    let date = native_is_date_unit(unit);
    let datetime = matches!(source, Some(NativeTypeNameCode::Known(7 | 10 | 12)))
        || matches!(kind, Kind::Time);
    Ok(if !clock || (date && datetime) {
        NativeExtractSelection::Datetime
    } else if date && !matches!(kind, Kind::Duration) {
        NativeExtractSelection::Mixed
    } else {
        NativeExtractSelection::Duration
    })
}
/// Select using actual nullable storage and original named/unknown type
/// identity.
pub fn native_extract_select_values(
    unit: &str,
    source: Option<NativeTypeNameCode>,
    value: Input<'_>,
    maximum: usize,
) -> Result<NativeExtractSelection> {
    let kind = match value {
        Input::Time(_) => Kind::Time,
        Input::Duration { .. } => Kind::Duration,
        _ => Kind::Other,
    };
    selection(unit, source, kind, Budget::finite(maximum)?)
}
pub(super) fn select_compat(
    unit: &str,
    source: Option<NativeTypeNameCode>,
    kind: Kind,
) -> Result<NativeExtractSelection> {
    selection(unit, source, kind, Budget(None))
}
fn number(
    value: std::result::Result<i64, tidb_query_datatype::codec::mysql::time::NativeTimeError>,
    unit: &str,
) -> NativeExtractValue {
    match value {
        Ok(n) => NativeExtractValue::Value(n),
        Err(_) => NativeExtractValue::InvalidUnit(format!("invalid unit {unit}")),
    }
}
fn datetime(unit: &str, raw: u64, budget: Budget) -> Result<NativeExtractValue> {
    budget.extent(unit.len(), 4, 128)?;
    Ok(number(native_extract_datetime_num(raw, unit), unit))
}
fn duration(unit: &str, nanos: i64, budget: Budget) -> Result<NativeExtractValue> {
    budget.extent(unit.len(), 4, 128)?;
    Ok(number(native_extract_duration_num(nanos, unit), unit))
}
/// Extract from actual raw calendar core bits without validating kind or FSP.
pub fn native_extract_datetime_values(
    unit: &str,
    raw: u64,
    maximum: usize,
) -> Result<NativeExtractValue> {
    datetime(unit, raw, Budget::finite(maximum)?)
}
/// Extract from actual nanoseconds, preserving original sign arithmetic.
pub fn native_extract_duration_values(
    unit: &str,
    nanos: i64,
    maximum: usize,
) -> Result<NativeExtractValue> {
    duration(unit, nanos, Budget::finite(maximum)?)
}
pub(super) fn datetime_compat(unit: &str, raw: u64) -> Result<NativeExtractValue> {
    datetime(unit, raw, Budget(None))
}
pub(super) fn duration_compat(unit: &str, nanos: i64) -> Result<NativeExtractValue> {
    duration(unit, nanos, Budget(None))
}
fn mixed_begin<'a>(
    unit: &'a str,
    input: &'a str,
    allow: bool,
    budget: Budget,
) -> Result<NativeExtractMixedStage<'a>> {
    budget.parser(unit, input)?;
    let invalid = || {
        NativeExtractMixedStage::Ready(NativeExtractValue::InvalidTime(format!(
            "Truncated incorrect time value: '{input}'"
        )))
    };
    let parsed = match native_parse_mysql_duration(
        input,
        i64::from(native_get_time_fsp(input)),
        &Utc,
        true,
        allow,
    ) {
        Ok(p) if p.overflow.is_none() && !p.truncated => p,
        _ => return Ok(invalid()),
    };
    let Some(fsp) = Time::native_normalize_fsp(parsed.fsp) else {
        return Ok(invalid());
    };
    Ok(NativeExtractMixedStage::NeedDatetime(
        NativeExtractMixedValue {
            unit,
            text: input,
            nanos: parsed.nanos,
            fsp,
        },
    ))
}
/// Pure finite work preflight before a caller obtains the first mode callback.
pub fn native_extract_mixed_work(unit: &str, text: &str, maximum: usize) -> Result<()> {
    Budget::finite(maximum)?.parser(unit, text)
}
/// Perform the actual duration parse with the first independently supplied
/// mode.
pub fn native_extract_mixed_duration_values<'a>(
    unit: &'a str,
    text: &'a str,
    allow: bool,
    maximum: usize,
) -> Result<NativeExtractMixedStage<'a>> {
    mixed_begin(unit, text, allow, Budget::finite(maximum)?)
}
pub(super) fn mixed_begin_compat<'a>(
    unit: &'a str,
    text: &'a str,
    allow: bool,
) -> Result<NativeExtractMixedStage<'a>> {
    mixed_begin(unit, text, allow, Budget(None))
}
impl NativeExtractMixedValue<'_> {
    /// Check actual second-stage work before obtaining its separately demanded
    /// mode.
    pub fn preflight(&self, maximum: usize) -> Result<()> {
        Budget::finite(maximum)?.parser(self.unit, self.text)
    }
    fn finish_inner(self, allow: bool, budget: Budget) -> Result<NativeExtractValue> {
        budget.parser(self.unit, self.text)?;
        if let Ok(parsed) = native_parse_time(
            self.text,
            TimeType::DateTime,
            i64::from(native_get_time_fsp(self.text)),
            false,
            true,
            allow,
            true,
            &Utc,
        ) {
            let [year, _, _, hour, minute, second, _] = Time::native_core_fields(parsed.time.raw);
            if year > 0
                && i64::from(hour) == i64::from(Duration::hours_from_nanos(self.nanos))
                && i64::from(minute) == i64::from(Duration::minutes_from_nanos(self.nanos))
                && i64::from(second) == i64::from(Duration::secs_from_nanos(self.nanos))
            {
                return datetime(self.unit, parsed.time.raw, budget);
            }
        }
        duration(self.unit, self.nanos, budget)
    }
    /// Consume the actual duration state with a fresh second allow-invalid-date
    /// flag.
    pub fn finish(self, allow: bool, maximum: usize) -> Result<NativeExtractValue> {
        self.finish_inner(allow, Budget::finite(maximum)?)
    }
    pub(super) fn finish_compat(self, allow: bool) -> Result<NativeExtractValue> {
        self.finish_inner(allow, Budget(None))
    }
}
fn composite(unit: &str, input: Option<&str>, budget: Budget) -> Result<NativeExtractValue> {
    let Some(input) = input else {
        return Ok(NativeExtractValue::Null);
    };
    budget.parser(unit, input)?;
    let trimmed = input.trim();
    if let Some((negative, hour, minute, second, microsecond)) = parse_signed_duration_hms(trimmed)
    {
        let n = duration_composite_value(unit, hour, minute, second, microsecond);
        return Ok(NativeExtractValue::Value(if negative { -n } else { n }));
    }
    let (date, suffix) = trimmed
        .split_once(char::is_whitespace)
        .map_or((trimmed, None), |(date, time)| (date, Some(time)));
    let Some((year, month, day)) = Time::parse_native_date_ymd(date) else {
        return Ok(NativeExtractValue::Warning(ZERO_WARNING));
    };
    let (hour, minute, second, microsecond) =
        time_parts_with_micros(suffix).unwrap_or((0, 0, 0, 0));
    Ok(NativeExtractValue::Value(datetime_composite_value(
        unit,
        year,
        month,
        day,
        hour,
        minute,
        second,
        microsecond,
    )))
}
/// The distinct broad calendar-composite policy over the original coerced text.
pub fn native_extract_composite_values(
    unit: &str,
    input: Option<&str>,
    maximum: usize,
) -> Result<NativeExtractValue> {
    composite(unit, input, Budget::finite(maximum)?)
}
pub(super) fn composite_compat(unit: &str, input: Option<&str>) -> Result<NativeExtractValue> {
    composite(unit, input, Budget(None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        NativeStringCoercionError, native_byte_coercion_work, native_coerce_bytes_bounded,
        native_duration_argument_cast_work,
    };
    #[test]
    fn actual_extract_selection_keeps_named_unknown_and_actual_storage_precedence() {
        let string = Input::String(b"2020-01-02");
        for code in [7, 10, 12] {
            assert_eq!(
                native_extract_select_values(
                    "DAY_HOUR",
                    Some(NativeTypeNameCode::Known(code)),
                    string,
                    8192
                )
                .unwrap(),
                NativeExtractSelection::Datetime
            );
        }
        assert_eq!(
            native_extract_select_values(
                "DAY_HOUR",
                Some(NativeTypeNameCode::Unknown(12)),
                string,
                8192
            )
            .unwrap(),
            NativeExtractSelection::Mixed
        );
        assert_eq!(
            native_extract_select_values(
                "DAY_HOUR",
                None,
                Input::Duration {
                    nanoseconds: 0,
                    fsp: i64::MAX
                },
                8192
            )
            .unwrap(),
            NativeExtractSelection::Duration
        );
        assert_eq!(
            native_extract_select_values(
                "DAY_HOUR",
                Some(NativeTypeNameCode::Known(12)),
                Input::Duration {
                    nanoseconds: 0,
                    fsp: 6
                },
                8192
            )
            .unwrap(),
            NativeExtractSelection::Datetime
        );
        let time = Input::Time(
            tidb_query_datatype::codec::mysql::time::NativeTemporalValue {
                raw: u64::MAX,
                kind: TimeType::Timestamp,
                fsp: 255,
            },
        );
        assert_eq!(
            native_extract_select_values("DAY_HOUR", None, time, 8192).unwrap(),
            NativeExtractSelection::Datetime
        );
        assert_eq!(
            native_extract_select_values("HOUR", Some(NativeTypeNameCode::Known(12)), time, 8192)
                .unwrap(),
            NativeExtractSelection::Duration
        );
        assert!(native_extract_select_values("DAY", None, Input::Null, 0).is_err());
        assert_eq!(
            native_extract_select_values("", None, Input::Null, 0).unwrap(),
            NativeExtractSelection::Datetime
        );
    }
    #[test]
    fn actual_extract_mixed_keeps_parsed_duration_and_late_independent_mode() {
        let unit = "DAY_SECOND";
        let text = "2020-02-31 03:04:05";
        assert!(
            matches!(native_extract_mixed_duration_values(unit,text,false,8192).unwrap(),NativeExtractMixedStage::Ready(NativeExtractValue::InvalidTime(ref s))if s=="Truncated incorrect time value: '2020-02-31 03:04:05'")
        );
        for (allow, expected) in [(false, 30_405), (true, 31_030_405)] {
            let NativeExtractMixedStage::NeedDatetime(state) =
                native_extract_mixed_duration_values(unit, text, true, 8192).unwrap()
            else {
                panic!()
            };
            assert_eq!(state.nanos, 11_045_000_000_000);
            assert_eq!(state.fsp, 0);
            assert_eq!(
                state.finish(allow, 8192).unwrap(),
                NativeExtractValue::Value(expected)
            );
        }
        let NativeExtractMixedStage::NeedDatetime(state) =
            native_extract_mixed_duration_values(unit, "1:02", false, 8192).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            state.finish(false, 8192).unwrap(),
            NativeExtractValue::Value(10_200)
        );
        assert_eq!(
            native_extract_composite_values(unit, Some("1:02"), 8192).unwrap(),
            NativeExtractValue::Value(102)
        );
        let NativeExtractMixedStage::NeedDatetime(state) = native_extract_mixed_duration_values(
            "DAY_MICROSECOND",
            "2020-01-02 00:00:00.000000",
            false,
            8192,
        )
        .unwrap() else {
            panic!()
        };
        // A date-prefixed zero clock retains the parsed six-digit FSP.
        assert_eq!((state.nanos, state.fsp), (0, 6));
        assert_eq!(
            state.finish(false, 8192).unwrap(),
            NativeExtractValue::Value(2_000_000_000_000)
        );
        for bad in ["1x", "838:59:59.1"] {
            assert!(matches!(
                native_extract_mixed_duration_values(unit, bad, false, 8192).unwrap(),
                NativeExtractMixedStage::Ready(NativeExtractValue::InvalidTime(_))
            ));
        }
    }
    #[test]
    fn actual_extract_broad_values_and_finite_work_do_not_normalize_original_policy() {
        for (unit, text, expected) in [
            ("DAY_MICROSECOND", "-01:02:03.1234567", -10_203_123_456),
            ("DAY_HOUR", "4294967295:00:00", 4_294_967_295),
            ("YEAR_MONTH", "4294967295-01-02", 429_496_729_501),
            ("DAY_SECOND", "2020-01-02 nonsense", 2_000_000),
            ("unknown", "12", 0),
        ] {
            assert_eq!(
                native_extract_composite_values(unit, Some(text), 8192).unwrap(),
                NativeExtractValue::Value(expected)
            );
        }
        assert_eq!(
            native_extract_composite_values("DAY", Some("bad"), 8192).unwrap(),
            NativeExtractValue::Warning(ZERO_WARNING)
        );
        assert_eq!(
            native_extract_composite_values(&"X".repeat(8192), None, 0).unwrap(),
            NativeExtractValue::Null
        );
        assert!(native_extract_composite_values("DAY", None, usize::MAX).is_err());
        assert!(native_extract_mixed_work("DAY_SECOND", "2020-01-02", 128).is_err());
        assert!(
            native_extract_mixed_duration_values("DAY_SECOND", "2020-01-02", true, 128).is_err()
        );
        let NativeExtractMixedStage::NeedDatetime(state) =
            native_extract_mixed_duration_values("DAY_SECOND", "1:02", false, 8192).unwrap()
        else {
            panic!()
        };
        assert!(state.preflight(0).is_err());
        // Finishing consumes the actual state even on a resource error.
        assert!(state.finish(false, 0).is_err());
        let raw = Time::native_core_from_fields(2020, 0, 31, 3, 4, 5, 123456);
        assert_eq!(
            native_extract_datetime_values("DAY_HOUR", raw, 8192).unwrap(),
            NativeExtractValue::Value(3103)
        );
        assert_eq!(
            native_extract_datetime_values("hOuR", raw, 8192).unwrap(),
            NativeExtractValue::InvalidUnit("invalid unit hOuR".into())
        );
        assert!(native_extract_datetime_values("DAY", raw, 0).is_err());
    }
    #[test]
    fn extract_byte_unit_and_duration_preflights_keep_original_error_and_no_format_routes() {
        let bytes = &[0xff, 0];
        for input in [
            Input::String(bytes),
            Input::Bytes(bytes),
            Input::BinaryLiteral(bytes),
            Input::Bit(bytes),
            Input::Enum(bytes),
            Input::Set(bytes),
            Input::Raw(bytes),
        ] {
            assert_eq!(native_byte_coercion_work(input).unwrap(), 2);
            assert_eq!(
                native_coerce_bytes_bounded(input, 2).unwrap(),
                Some(bytes.to_vec())
            );
            assert!(matches!(
                native_coerce_bytes_bounded(input, 1),
                Err(NativeStringCoercionError::ResourceLimit)
            ));
            assert!(native_duration_argument_cast_work(input).is_ok());
        }
        assert_eq!(native_coerce_bytes_bounded(Input::Null, 0).unwrap(), None);
        assert!(matches!(
            native_coerce_bytes_bounded(Input::MinNotNull, 8192),
            Err(NativeStringCoercionError::Unsupported(
                "range sentinel byte coercion"
            ))
        ));
        assert_eq!(native_duration_argument_cast_work(Input::Null).unwrap(), 0);
        assert_eq!(
            native_duration_argument_cast_work(Input::Duration {
                nanoseconds: i64::MIN,
                fsp: i64::MAX
            })
            .unwrap(),
            0
        );
        let time = Input::Time(
            tidb_query_datatype::codec::mysql::time::NativeTemporalValue {
                raw: u64::MAX,
                kind: TimeType::Timestamp,
                fsp: 255,
            },
        );
        assert!(native_duration_argument_cast_work(time).unwrap() > 0);
        assert_eq!(
            crate::native_datetime_argument_cast_work(
                time,
                tidb_query_datatype::codec::native_numeric::NativeNumericInput::Time(match time {
                    Input::Time(t) => t,
                    _ => unreachable!(),
                }),
                None
            )
            .unwrap(),
            0
        );
    }
}
