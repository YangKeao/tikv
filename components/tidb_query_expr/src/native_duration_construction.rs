// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Actual duration/construction operands; native callers supply finite work.
use tidb_query_datatype::codec::mysql::{Duration, Time};
/// Original native time-family policy, never a second expression program.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDurationConstructionOperation {
    /// Native MICROSECOND policy.
    Microsecond,
    /// Native TIME policy.
    Time,
    /// Native TIMEDIFF policy.
    TimeDiff,
    /// Native TIME_TO_SEC policy.
    TimeToSec,
    /// Native SEC_TO_TIME policy.
    SecToTime,
    /// Native TIME_FORMAT policy.
    TimeFormat,
    /// Native MAKEDATE policy.
    MakeDate,
    /// Native MAKETIME policy.
    MakeTime,
    /// Native FROM_DAYS policy.
    FromDays,
    /// Native PERIOD_ADD policy.
    PeriodAdd,
    /// Native PERIOD_DIFF policy.
    PeriodDiff,
    /// Native GET_FORMAT policy.
    GetFormat,
    /// Native DATEDIFF policy.
    DateDiff,
    /// Native CONVERT_TZ policy.
    ConvertTz,
    /// Native ADDTIME policy with original static signature facts.
    AddTime {
        /// Original argument type categories.
        kinds: [crate::NativeTimeAddKind; 2],
        /// Both original arguments are literal constants.
        row_path: bool,
    },
    /// Native SUBTIME policy with original static signature facts.
    SubTime {
        /// Original argument type categories.
        kinds: [crate::NativeTimeAddKind; 2],
        /// Both original arguments are literal constants.
        row_path: bool,
    },
    /// Native one-argument TIMESTAMP policy.
    Timestamp1,
    /// Native two-argument TIMESTAMP policy.
    Timestamp2,
    /// Native TIMESTAMPADD policy.
    TimestampAdd,
}
impl NativeDurationConstructionOperation {
    /// Original function spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Microsecond => "microsecond",
            Self::Time => "time",
            Self::TimeDiff => "timediff",
            Self::TimeToSec => "time_to_sec",
            Self::SecToTime => "sec_to_time",
            Self::TimeFormat => "time_format",
            Self::MakeDate => "makedate",
            Self::MakeTime => "maketime",
            Self::FromDays => "from_days",
            Self::PeriodAdd => "period_add",
            Self::PeriodDiff => "period_diff",
            Self::GetFormat => "get_format",
            Self::DateDiff => "datediff",
            Self::ConvertTz => "convert_tz",
            Self::AddTime { .. } => "addtime",
            Self::SubTime { .. } => "subtime",
            Self::Timestamp1 | Self::Timestamp2 => "timestamp",
            Self::TimestampAdd => "timestampadd",
        }
    }
    /// Original fixed argument count.
    pub fn arity(self) -> usize {
        match self {
            Self::MakeTime | Self::ConvertTz | Self::TimestampAdd => 3,
            Self::AddTime { .. } | Self::SubTime { .. } | Self::Timestamp2 => 2,
            Self::TimeDiff
            | Self::TimeFormat
            | Self::MakeDate
            | Self::PeriodAdd
            | Self::PeriodDiff
            | Self::GetFormat
            | Self::DateDiff => 2,
            _ => 1,
        }
    }
    /// Original generic temporal-result boundary; TIME deliberately is absent.
    pub fn temporal_result(self) -> bool {
        matches!(
            self,
            Self::TimeDiff
                | Self::SecToTime
                | Self::MakeDate
                | Self::MakeTime
                | Self::FromDays
                | Self::ConvertTz
        )
    }
}
/// Resource, API-contract and original SQL argument failures stay distinct.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDurationConstructionError {
    /// Finite work refused before materialization.
    Resource,
    /// Original period_add invalid arguments.
    PeriodAdd,
    /// Original period_diff invalid arguments.
    PeriodDiff,
    /// Operand and demanded metadata disagree.
    Contract,
}
use NativeDurationConstructionError as E;
pub(crate) fn duration_work(maximum: usize, n: usize) -> Result<(), E> {
    if maximum == usize::MAX || n > maximum {
        Err(E::Resource)
    } else {
        Ok(())
    }
}
pub(crate) fn duration_text_work(text: Option<&str>, maximum: usize) -> Result<(), E> {
    duration_work(
        maximum,
        match text {
            None => 0,
            Some(s) => s
                .len()
                .checked_mul(32)
                .and_then(|n| n.checked_add(2048))
                .ok_or(E::Resource)?,
        },
    )
}
/// Original TIME text and parse disposition, without an encoded status byte.
#[derive(Debug, Eq, PartialEq)]
pub struct NativeTimeValue {
    /// Actual formatted value, including the original zero fallback.
    pub value: String,
    /// Original truncation that the caller reports through its context.
    pub truncated: bool,
}
/// TIME's complete original duration parser and zero-value fallback.
pub fn native_time_value(text: Option<&str>, maximum: usize) -> Result<Option<NativeTimeValue>, E> {
    duration_text_work(text, maximum)?;
    let Some(text) = text else { return Ok(None) };
    let (truncated, value) =
        match crate::parse_native_duration(text, crate::native_duration_fsp(text)) {
            Ok(v) => (
                false,
                crate::native_format_time_diff(v.micros, v.fsp.max(0) as usize),
            ),
            Err(_) => (true, "00:00:00".to_owned()),
        };
    Ok(Some(NativeTimeValue { value, truncated }))
}
/// MICROSECOND suppresses parser failure; it does not use TIME_TO_SEC's parser.
pub fn native_microsecond_value(text: Option<&str>, maximum: usize) -> Result<Option<i64>, E> {
    duration_text_work(text, maximum)?;
    Ok(text
        .and_then(|s| crate::parse_native_duration(s, crate::native_duration_fsp(s)).ok())
        .map(|v| v.micro_second()))
}
/// TIME_TO_SEC retains the separate complete native elapsed-time parser.
pub fn native_time_to_sec_value(text: Option<&str>, maximum: usize) -> Result<Option<i64>, E> {
    duration_text_work(text, maximum)?;
    Ok(text
        .and_then(Time::parse_native_duration_text)
        .map(|(s, _)| s))
}
/// Actual SQL seconds and their demanded precision; ordinary arithmetic and
/// exceptional IEEE754 behavior remain the original formatter's policy.
pub fn native_seconds_to_time_value(
    seconds: Option<f64>,
    fsp: Option<usize>,
    maximum: usize,
) -> Result<Option<String>, E> {
    duration_work(maximum, 0)?;
    match (seconds, fsp) {
        (None, None) => Ok(None),
        (Some(s), Some(fsp)) => {
            duration_work(
                maximum,
                fsp.checked_mul(2)
                    .and_then(|n| n.checked_add(4096))
                    .ok_or(E::Resource)?,
            )?;
            Ok(Some(format_duration(s, fsp)))
        }
        _ => Err(E::Contract),
    }
}
fn format_duration(seconds: f64, fsp: usize) -> String {
    let sign = if seconds < 0.0 { "-" } else { "" };
    let max = 838.0 * 3600.0 + 59.0 * 60.0 + 59.0;
    let mut seconds = seconds.abs();
    if seconds > max {
        seconds = max;
    }
    let whole = seconds.trunc() as i64;
    let hour = whole / 3600;
    let minute = whole / 60 % 60;
    let mut second = whole % 60;
    if fsp == 0 {
        return format!("{sign}{hour:02}:{minute:02}:{second:02}");
    }
    // Go reaches this text through `fmt.Sprintf("%v", second)` followed by
    // `ParseDuration`, whose fraction rounding works on the DECIMAL DIGITS
    // and carries half-up at the requested precision
    // (`Duration.RoundFrac`: Go's time.Round rounds nearest values and sends
    // exact ties toward positive infinity).
    // Doing the arithmetic in f64 first re-derives 30.0000005 as
    // ...4999996µs and loses the digit -- so round off the SHORTEST decimal
    // rendering instead.
    // Go formats the REAL second value with %v (shortest repr): 30.1 stays
    // "30.1", 30.0000005 stays "30.0000005".
    let digits = format!("{seconds}");
    let mut digits_fraction = match digits.split_once('.') {
        Some((_, fraction)) => fraction.to_owned(),
        None => String::new(),
    };
    // Round half-up at the requested precision off the FIRST DISCARDED
    // digit, carrying into the whole part when the fraction overflows.
    let round_up = digits_fraction.len() > fsp && digits_fraction.as_bytes()[fsp] >= b'5';
    digits_fraction.truncate(fsp);
    while digits_fraction.len() < fsp {
        digits_fraction.push('0');
    }
    let mut fraction: i64 = digits_fraction.parse().unwrap_or(0);
    if round_up {
        fraction += 1;
        if fraction >= 10_i64.pow(fsp as u32) {
            fraction = 0;
            second += 1;
        }
    }
    format!("{sign}{hour:02}:{minute:02}:{second:02}.{fraction:0fsp$}")
}
/// Compute actual signed seconds before demanding precision. Signed/unsigned
/// hour provenance is retained as data rather than a host-adjusted answer.
pub fn native_make_time_parts_value(
    hour: Option<(i64, bool)>,
    minute: Option<i64>,
    second: Option<f64>,
    maximum: usize,
) -> Result<Option<f64>, E> {
    duration_work(maximum, 0)?;
    let (Some((mut hour, hour_unsigned)), Some(minute), Some(second)) = (hour, minute, second)
    else {
        return Ok(None);
    };
    if !(0..60).contains(&minute) || !(0.0..60.0).contains(&second) {
        return Ok(None);
    }
    let mut overflow = false;
    if hour < 0 && hour_unsigned {
        hour = 838;
        overflow = true;
    }
    let negative = hour < 0;
    let hour_abs = hour.unsigned_abs();
    if hour_abs > 838 || (hour_abs == 838 && minute == 59 && second > 59.0) {
        overflow = true;
    }
    let total = if overflow {
        838.0 * 3600.0 + 59.0 * 60.0 + 59.0
    } else {
        hour_abs as f64 * 3600.0 + minute as f64 * 60.0 + second
    };
    Ok(Some(if negative { -total } else { total }))
}
/// Original native MAKEDATE pivot/calendar arithmetic, including raw panic
/// boundaries. The wire make_date DateTime worker is a different policy.
pub fn native_make_date_value(
    year: Option<i64>,
    day: Option<i64>,
    maximum: usize,
) -> Result<Option<String>, E> {
    duration_work(maximum, 0)?;
    let (Some(mut year), Some(day)) = (year, day) else {
        return Ok(None);
    };
    if day <= 0 || !(0..=9999).contains(&year) {
        return Ok(None);
    }
    if year < 70 {
        year += 2000;
    } else if year < 100 {
        year += 1900;
    }
    let (y, m, d) =
        Time::native_civil_from_days(Time::native_days_from_civil(year, 1, 1) + day - 1);
    if !(1..=9999).contains(&y) {
        return Ok(None);
    }
    duration_work(maximum, 1024)?;
    Ok(Some(format!("{y:04}-{m:02}-{d:02}")))
}
/// FROM_DAYS returns actual date text, including canonical zero-date text and
/// the original narrow NULL band. The caller preserves zero's Time identity.
pub fn native_from_days_value(n: Option<i64>, maximum: usize) -> Result<Option<String>, E> {
    duration_work(maximum, 0)?;
    let Some(n) = n else { return Ok(None) };
    if (3652425..=3652499).contains(&n) {
        return Ok(None);
    }
    duration_work(maximum, 1024)?;
    if !(366..=3652424).contains(&n) {
        return Ok(Some("0000-00-00".into()));
    }
    let (y, m, d) = Time::native_civil_from_days(n - 719528);
    Ok(Some(format!("{y:04}-{m:02}-{d:02}")))
}
/// The original nullable period validation, wrapping arithmetic and SQL cause.
pub fn native_period_value(
    add: bool,
    left: Option<i64>,
    right: Option<i64>,
    maximum: usize,
) -> Result<Option<i64>, E> {
    duration_work(maximum, 0)?;
    let (Some(a), Some(b)) = (left, right) else {
        return Ok(None);
    };
    if !Time::is_valid_native_period(a) || (!add && !Time::is_valid_native_period(b)) {
        return Err(if add { E::PeriodAdd } else { E::PeriodDiff });
    }
    Ok(Some(if add {
        Time::month_to_period_wrapping(
            (Time::period_to_month_wrapping(a as u64) as i64).wrapping_add(b) as u64,
        ) as i64
    } else {
        Time::period_to_month_wrapping(a as u64)
            .wrapping_sub(Time::period_to_month_wrapping(b as u64)) as i64
    }))
}
/// Raw ETString byte lookup; UTF-8 normalization is not part of GET_FORMAT.
pub fn native_get_format_value(
    kind: Option<&[u8]>,
    locale: Option<&[u8]>,
    maximum: usize,
) -> Result<Option<String>, E> {
    duration_work(maximum, 0)?;
    let (Some(kind), Some(locale)) = (kind, locale) else {
        return Ok(None);
    };
    // Both comparisons use fixed literal lengths; a longer borrowed slice
    // fails its length check without scanning or copying that storage. Only
    // the small static lookup and bounded result allocation are demanded.
    duration_work(maximum, 1024)?;
    Ok(Some(Time::get_format_native(kind, locale).to_owned()))
}
/// DATEDIFF parses both actual text operands after both source coercions.
pub fn native_date_diff_value(
    left: Option<&str>,
    right: Option<&str>,
    maximum: usize,
) -> Result<Option<i64>, E> {
    duration_work(maximum, 0)?;
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(None);
    };
    duration_text_work(Some(left), maximum)?;
    duration_text_work(Some(right), maximum)?;
    let (Some((ly, lm, ld)), Some((ry, rm, rd))) = (
        Time::parse_native_date_ymd(left),
        Time::parse_native_date_ymd(right),
    ) else {
        return Ok(None);
    };
    Ok(Some(
        Time::native_days_from_civil(ly, lm, ld) - Time::native_days_from_civil(ry, rm, rd),
    ))
}
/// Unvalidated actual legacy raw cores are a separate DATEDIFF domain.
pub fn native_date_diff_core_value(
    left: Option<u64>,
    right: Option<u64>,
    maximum: usize,
) -> Result<Option<i64>, E> {
    duration_work(maximum, 0)?;
    Ok(left
        .zip(right)
        .map(|(a, b)| Time::native_core_date_diff(a, b) as i64))
}
/// Legacy MICROSECOND projects signed nanoseconds without SQL text parsing.
pub fn native_microsecond_nanos_value(
    nanos: Option<i64>,
    maximum: usize,
) -> Result<Option<i64>, E> {
    duration_work(maximum, 0)?;
    Ok(nanos.map(|n| Duration::micro_secs_from_nanos(n) as i64))
}
/// Legacy DATE is a raw midnight-core predicate, not native DATE projection.
pub fn native_date_predicate_core_value(
    core: Option<u64>,
    maximum: usize,
) -> Result<Option<i64>, E> {
    duration_work(maximum, 0)?;
    Ok(core.map(|v| i64::from(Time::native_date_core(v) != 0)))
}
/// Actual owned elapsed text retained between validity and late layout demand.
/// It is consumed once and never carries a computed answer or replayable flag.
#[derive(Debug)]
pub struct NativeTimeFormatValue {
    text: String,
}
impl NativeTimeFormatValue {
    pub(crate) fn into_text(self) -> String {
        self.text
    }

    /// Actual retained text capacity while the later layout is demanded.
    pub fn retained_bytes(&self) -> usize {
        self.text.capacity()
    }

    /// Consume actual layout only after original duration validity.
    pub fn finish(self, layout: Option<&str>, maximum: usize) -> Result<Option<String>, E> {
        duration_work(maximum, 0)?;
        let Some(layout) = layout else {
            return Ok(None);
        };
        let n = self
            .text
            .len()
            .checked_add(layout.len())
            .and_then(|n| n.checked_mul(32))
            .and_then(|n| n.checked_add(2048))
            .ok_or(E::Resource)?;
        duration_work(maximum, n)?;
        Ok(Time::native_time_format(&self.text, layout))
    }
}
/// Original TIME_FORMAT validity parser, independent from TIME's Go parser.
pub fn native_time_format_value(
    text: Option<String>,
    maximum: usize,
) -> Result<Option<NativeTimeFormatValue>, E> {
    duration_text_work(text.as_deref(), maximum)?;
    Ok(text.and_then(|text| {
        Time::parse_native_duration_text(&text).map(|_| NativeTimeFormatValue { text })
    }))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duration_actual_values_retain_distinct_parsers_reports_and_ieee754() {
        assert_eq!(native_time_value(None, 0), Ok(None));
        assert_eq!(
            native_time_value(Some("bad"), 8192),
            Ok(Some(NativeTimeValue {
                value: "00:00:00".into(),
                truncated: true
            }))
        );
        assert_eq!(
            native_time_value(Some("-00:00:00.1234567"), 8192),
            Ok(Some(NativeTimeValue {
                value: "-00:00:00.123457".into(),
                truncated: false
            }))
        );
        assert_eq!(
            native_microsecond_value(Some("-00:00:00.1234567"), 8192),
            Ok(Some(123457))
        );
        assert_eq!(native_microsecond_value(Some("bad"), 8192), Ok(None));
        assert_eq!(
            native_time_to_sec_value(Some("-838:59:59.999999"), 8192),
            Ok(Some(-3020399))
        );
        assert_eq!(
            native_make_time_parts_value(Some((-1, true)), Some(0), Some(0.0), 0),
            Ok(Some(3020399.0))
        );
        assert_eq!(
            native_make_time_parts_value(Some((12, false)), Some(60), Some(0.0), 0),
            Ok(None)
        );
        assert_eq!(
            native_make_time_parts_value(Some((12, false)), Some(0), Some(f64::NAN), 0),
            Ok(None)
        );
        assert_eq!(
            native_seconds_to_time_value(Some(30.0000005), Some(6), 8192),
            Ok(Some("00:00:30.000001".into()))
        );
        assert_eq!(
            native_seconds_to_time_value(Some(f64::NAN), Some(6), 8192),
            Ok(Some("00:00:00.000000".into()))
        );
        assert_eq!(
            native_seconds_to_time_value(Some(-0.0), Some(6), 8192),
            Ok(Some("00:00:00.000000".into()))
        );
        assert_eq!(
            native_seconds_to_time_value(Some(f64::NEG_INFINITY), Some(0), 8192),
            Ok(Some("-838:59:59".into()))
        );
    }
    #[test]
    fn duration_finite_allowances_do_not_turn_contracts_or_refusals_into_null() {
        assert_eq!(native_seconds_to_time_value(None, None, 0), Ok(None));
        assert_eq!(
            native_seconds_to_time_value(None, Some(6), 8192),
            Err(E::Contract)
        );
        assert_eq!(
            native_seconds_to_time_value(Some(0.0), None, 8192),
            Err(E::Contract)
        );
        assert_eq!(
            native_seconds_to_time_value(None, None, usize::MAX),
            Err(E::Resource)
        );
        assert_eq!(
            native_seconds_to_time_value(Some(0.0), Some(usize::MAX), 8192),
            Err(E::Resource)
        );
        assert_eq!(
            native_time_value(Some(&"0".repeat(100000)), 8192),
            Err(E::Resource)
        );
        assert_eq!(
            native_get_format_value(None, Some(&vec![0xff; 100000]), 0),
            Ok(None)
        );
        assert_eq!(
            native_get_format_value(Some(b"DATE"), Some(b"uSa"), 8192),
            Ok(Some("%m.%d.%Y".into()))
        );
        assert_eq!(
            native_get_format_value(Some(b"date"), Some(b"USA"), 8192),
            Ok(Some(String::new()))
        );
    }
    #[test]
    fn consuming_actual_time_stages_keep_late_demand_and_kind_matching() {
        assert!(
            crate::native_time_diff_value(Some("bad"), 8192)
                .unwrap()
                .is_none()
        );
        let state = crate::native_time_diff_value(Some("2024-01-02"), 8192)
            .unwrap()
            .unwrap();
        assert_eq!(state.finish(Some("00:00:00"), 8192), Ok(None));
        let state = crate::native_time_diff_value(Some("-10:00:00"), 8192)
            .unwrap()
            .unwrap();
        assert_eq!(
            state.finish(Some("01:00:00"), 8192),
            Ok(Some("-11:00:00".into()))
        );
        let state = crate::native_time_diff_value(Some("10:00:00"), 8192)
            .unwrap()
            .unwrap();
        assert_eq!(
            state.finish(Some(&"0".repeat(100000)), 8192),
            Err(E::Resource)
        );
        // The native TIME_FORMAT parser treats nonnumeric text as zero,
        // unlike TIME's warning parser and TIMEDIFF's invalid-left domain.
        let state = native_time_format_value(Some("bad".into()), 8192)
            .unwrap()
            .unwrap();
        assert_eq!(
            state.finish(Some("%H:%i:%s"), 8192),
            Ok(Some("00:00:00".into()))
        );
        assert!(
            native_time_format_value(Some("900:30:15".into()), 8192)
                .unwrap()
                .is_none()
        );
        let state = native_time_format_value(Some("12:34:56.123456".into()), 8192)
            .unwrap()
            .unwrap();
        assert_eq!(
            state.finish(Some("%H:%i:%s.%f"), 8192),
            Ok(Some("12:34:56.123456".into()))
        );
        let state = native_time_format_value(Some("12:34:56".into()), 8192)
            .unwrap()
            .unwrap();
        assert_eq!(state.finish(None, 0), Ok(None));
    }
    #[test]
    fn constructors_period_causes_and_legacy_raw_values_keep_their_domains() {
        assert_eq!(
            native_make_date_value(Some(24), Some(60), 8192),
            Ok(Some("2024-02-29".into()))
        );
        assert_eq!(native_make_date_value(Some(10000), Some(1), 0), Ok(None));
        assert_eq!(
            native_from_days_value(Some(0), 8192),
            Ok(Some("0000-00-00".into()))
        );
        for n in [3652425, 3652499] {
            assert_eq!(native_from_days_value(Some(n), 0), Ok(None));
        }
        assert_eq!(
            native_from_days_value(Some(3652500), 8192),
            Ok(Some("0000-00-00".into()))
        );
        assert_eq!(
            native_period_value(true, Some(202401), Some(3), 0),
            Ok(Some(202404))
        );
        assert_eq!(
            native_period_value(false, Some(202401), Some(202312), 0),
            Ok(Some(1))
        );
        assert_eq!(
            native_period_value(true, Some(0), Some(1), 0),
            Err(E::PeriodAdd)
        );
        assert_eq!(
            native_period_value(false, Some(202401), Some(0), 0),
            Err(E::PeriodDiff)
        );
        assert_eq!(native_period_value(true, None, Some(0), 0), Ok(None));
        for raw in [None, Some(0), Some(u64::MAX), Some(i64::MIN as u64)] {
            assert_eq!(
                native_date_predicate_core_value(raw, 0),
                Ok(raw.map(|v| i64::from(Time::native_date_core(v) != 0)))
            );
            assert_eq!(native_date_diff_core_value(raw, None, 0), Ok(None));
            assert_eq!(
                native_microsecond_nanos_value(raw.map(|v| v as i64), 0),
                Ok(raw.map(|v| Duration::micro_secs_from_nanos(v as i64) as i64))
            );
        }
    }
}
