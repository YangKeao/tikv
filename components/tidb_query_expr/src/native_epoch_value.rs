// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Actual timestamp conversion values; no identity transport or expression VM.
use chrono::TimeZone;
use tidb_query_datatype::codec::{
    convert::{ToStringValue, native_warning_subject_byte_cap},
    mysql::{
        Decimal, NativeDecimalParseRef, NativeDecimalParseValue, Time, TimeType,
        time::{
            NativeSessionTimeZone, NativeTemporalValue, native_core_from_datetime,
            native_get_time_fsp, native_parse_time,
        },
    },
    native_sql_string::NativeSqlStringInput as Input,
};

use crate::{
    native_from_unixtime::{
        NativeFromUnixTimeEpoch, format_local, native_from_unixtime_instant_to_local, parse_epoch,
        rounded_epoch,
    },
    native_unix_timestamp::{legacy_micros, naive, unix_timestamp_micros_in_range},
};
/// Actual operation lacks a finite allowance or receives an unrelated kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeEpochValueError {
    /// Finite formatter/parser/diagnostic/result allowance exhausted.
    Resource,
    /// Numeric-only conversion received a different actual source kind.
    InvalidKind,
}
type Result<T> = std::result::Result<T, NativeEpochValueError>;
#[derive(Clone, Copy)]
pub(super) struct Budget(pub(super) Option<usize>);
impl Budget {
    fn finite(n: usize) -> Result<Self> {
        if n == usize::MAX {
            Err(NativeEpochValueError::Resource)
        } else {
            Ok(Self(Some(n)))
        }
    }
}
fn work(n: usize, m: usize, extra: usize, max: Budget) -> Result<()> {
    let Some(max) = max.0 else {
        return Ok(());
    };
    if n.checked_mul(m)
        .and_then(|n| n.checked_add(extra))
        .is_none_or(|n| n > max)
    {
        return Err(NativeEpochValueError::Resource);
    }
    Ok(())
}
/// Check original text parsing/diagnostic work before a caller reads its zone.
pub(super) fn native_epoch_text_work_budget(text: &str, maximum: Budget) -> Result<()> {
    work(text.len(), 64, 4096, maximum)
}
fn decimal(digits: Vec<u8>, scale: u32) -> NativeDecimalParseValue {
    NativeDecimalParseValue::from_raw_parts(false, digits.into(), scale, scale, None)
}
/// Actual UNIX_TIMESTAMP terminal value or original demanded second-zone state.
#[derive(Debug)]
pub enum NativeUnixTimestampValue {
    /// SQL NULL.
    Null,
    /// Original signed seconds result.
    Int(i64),
    /// Canonical actual coefficient, visible scale and storage scale.
    Decimal(NativeDecimalParseValue),
    /// Original unconditional SQL1292 warning and NULL.
    Warning(String),
    /// A valid actual parsed time, before a separately demanded zone read.
    NeedZone(NativeUnixTimestampParsed),
}
/// Consuming actual parsed-time continuation, never a report or caller choice.
#[derive(Debug)]
pub struct NativeUnixTimestampParsed {
    pub(super) value: NativeTemporalValue,
}
pub(super) fn numeric(
    micros: i64,
    fsp: usize,
    maximum: Budget,
) -> Result<NativeUnixTimestampValue> {
    work(0, 0, 128, maximum)?;
    let micros = if unix_timestamp_micros_in_range(micros) {
        micros
    } else {
        0
    };
    if fsp == 0 {
        return Ok(NativeUnixTimestampValue::Int(micros / 1_000_000));
    }
    let secs = micros / 1_000_000;
    let frac = (micros % 1_000_000) / 10_i64.pow(6 - fsp as u32);
    let coefficient = secs * 10_i64.pow(fsp as u32) + frac;
    let digits = format!("{coefficient:0fsp$}").into_bytes();
    Ok(NativeUnixTimestampValue::Decimal(decimal(
        digits, fsp as u32,
    )))
}
/// Original now arithmetic, including its ordinary i64 overflow behavior.
pub(super) fn native_unix_timestamp_now_values_budget(
    seconds: i64,
    nanos: u32,
    maximum: Budget,
) -> Result<NativeUnixTimestampValue> {
    work(0, 0, 128, maximum)?;
    numeric(i64::from(nanos / 1000) + seconds * 1_000_000, 0, maximum)
}
/// Original fixed-flag parse with the first actual timezone.
pub(super) fn native_unix_timestamp_parse_values_budget(
    text: &str,
    is_float: bool,
    zone: &NativeSessionTimeZone,
    maximum: Budget,
) -> Result<NativeUnixTimestampValue> {
    native_epoch_text_work_budget(text, maximum)?;
    let Ok(parsed) = native_parse_time(
        text,
        TimeType::DateTime,
        i64::from(native_get_time_fsp(text)),
        is_float,
        true,
        false,
        true,
        zone,
    ) else {
        return Ok(NativeUnixTimestampValue::Warning(format!(
            "Incorrect datetime value: '{text}'"
        )));
    };
    let value = parsed.time;
    let [year, month, day, ..] = Time::native_core_fields(value.raw);
    if year == 0 && month == 0 && day == 0 {
        return Ok(NativeUnixTimestampValue::Null);
    }
    if month == 0 || day == 0 || naive(value).is_none() {
        return numeric(0, usize::from(value.fsp), maximum);
    }
    Ok(NativeUnixTimestampValue::NeedZone(
        NativeUnixTimestampParsed { value },
    ))
}
impl NativeUnixTimestampParsed {
    /// Consume the accepted time using the independently supplied second zone.
    pub(super) fn finish_budget(
        self,
        zone: &NativeSessionTimeZone,
        maximum: Budget,
    ) -> Result<NativeUnixTimestampValue> {
        work(0, 0, 128, maximum)?;
        let Some(local) = naive(self.value) else {
            return numeric(0, usize::from(self.value.fsp), maximum);
        };
        let instant = match zone {
            NativeSessionTimeZone::Local => {
                crate::native_legacy_local_to_instant(&chrono::Local, &local)
            }
            NativeSessionTimeZone::Fixed { offset_secs, .. } => {
                Some(local.and_utc() - chrono::Duration::seconds(i64::from(*offset_secs)))
            }
            NativeSessionTimeZone::Named(tz) => crate::native_legacy_local_to_instant(tz, &local),
        };
        numeric(
            instant.map_or(0, |v| v.timestamp_micros()),
            usize::from(self.value.fsp),
            maximum,
        )
    }
}
/// Distinct strict legacy conversion of actual nullable Time and explicit zone.
pub(super) fn native_unix_timestamp_legacy_values_budget(
    value: Option<NativeTemporalValue>,
    zone: &NativeSessionTimeZone,
    want_decimal: bool,
    maximum: Budget,
) -> Result<NativeUnixTimestampValue> {
    let Some(value) = value else {
        return Ok(NativeUnixTimestampValue::Null);
    };
    work(0, 0, 128, maximum)?;
    let micros = legacy_micros(value, zone);
    if !want_decimal {
        return Ok(NativeUnixTimestampValue::Int(
            micros.map_or(0, |v| v.div_euclid(1_000_000)),
        ));
    }
    let (digits, scale) = match micros {
        Some(n) => (n.to_string().into_bytes(), 6),
        None => (b"0".to_vec(), 0),
    };
    Ok(NativeUnixTimestampValue::Decimal(decimal(digits, scale)))
}
/// Computed actual epoch waiting for a timezone, with an optional original
/// truncation diagnostic to deliver first.
#[derive(Debug)]
pub enum NativeFromUnixTimeValue {
    /// SQL NULL without zone or format demand.
    Null,
    /// Accepted epoch, before the caller's zone getter.
    Ready(NativeFromUnixTimeValues),
    /// Retained actual zero epoch; original HandleTruncate precedes its zone.
    Truncate {
        value: NativeFromUnixTimeValues,
        message: String,
    },
}
/// Actual accepted epoch. Local rendering consumes it once.
#[derive(Debug)]
pub struct NativeFromUnixTimeValues {
    pub(super) epoch: NativeFromUnixTimeEpoch,
}
fn accepted(epoch: Option<NativeFromUnixTimeEpoch>) -> NativeFromUnixTimeValue {
    epoch.map_or(NativeFromUnixTimeValue::Null, |epoch| {
        NativeFromUnixTimeValue::Ready(NativeFromUnixTimeValues { epoch })
    })
}
fn visible(value: NativeDecimalParseRef<'_>) -> String {
    Decimal::native_format_visible(
        value.negative,
        value.digits,
        value.scale,
        value.storage_scale,
    )
}
/// Check numeric text preparation before visible Decimal formatting.
pub(super) fn native_from_unixtime_numeric_work_budget(
    value: Input<'_>,
    maximum: Budget,
) -> Result<()> {
    if !matches!(
        value,
        Input::Int(_) | Input::UInt(_) | Input::Decimal(_) | Input::Real(_) | Input::Float32(_)
    ) {
        return Err(NativeEpochValueError::InvalidKind);
    }
    if maximum.0.is_none() {
        return Ok(());
    }
    let n =
        crate::native_string_coercion_work(value).map_err(|_| NativeEpochValueError::Resource)?;
    work(n, 64, 4096, maximum)
}
/// Original native numeric-kind epoch/FSP policy over the actual source view.
pub(super) fn native_from_unixtime_numeric_values_budget(
    value: Input<'_>,
    maximum: Budget,
) -> Result<NativeFromUnixTimeValue> {
    native_from_unixtime_numeric_work_budget(value, maximum)?;
    let (text, fsp) = match value {
        Input::Int(n) => (n.to_string(), 0),
        Input::UInt(n) => (n.to_string(), 0),
        Input::Decimal(v) => {
            let s = visible(v);
            let fsp = s.split_once('.').map_or(0, |(_, s)| s.len()).min(6);
            (s, fsp)
        }
        Input::Real(v) | Input::Float32(v) => {
            let Some(decimal) = Decimal::native_from_f64(v) else {
                return Ok(NativeFromUnixTimeValue::Null);
            };
            debug_assert_eq!(decimal.storage_scale(), decimal.result_scale());
            (decimal.to_string_value(), 6)
        }
        _ => unreachable!("checked actual numeric kind"),
    };
    Ok(accepted(parse_epoch(&text, fsp)))
}
/// Original trimmed text preparation, including accepted zero with a capped
/// DECIMAL diagnostic for an invalid integer prefix.
pub(super) fn native_from_unixtime_text_values_budget(
    text: &str,
    maximum: Budget,
) -> Result<NativeFromUnixTimeValue> {
    native_epoch_text_work_budget(text, maximum)?;
    let trimmed = text.trim();
    let (int, _) = trimmed.split_once('.').unwrap_or((trimmed, ""));
    if int.parse::<i64>().is_err() {
        let message = format!(
            "Truncated incorrect DECIMAL value: '{}'",
            native_warning_subject_byte_cap(trimmed)
        );
        return Ok(NativeFromUnixTimeValue::Truncate {
            value: NativeFromUnixTimeValues {
                epoch: rounded_epoch(0, 0).expect("source zero epoch is in range"),
            },
            message,
        });
    }
    Ok(accepted(parse_epoch(text, 6)))
}
impl NativeFromUnixTimeValues {
    /// Check local formatting work before demanding the zone.
    pub(super) fn preflight_budget(&self, maximum: Budget) -> Result<()> {
        work(0, 0, 256, maximum)
    }
    /// Consume the computed epoch after truncation policy and the zone getter.
    pub(super) fn local_budget(
        self,
        zone: &NativeSessionTimeZone,
        maximum: Budget,
    ) -> Result<Option<String>> {
        self.preflight_budget(maximum)?;
        Ok(
            native_from_unixtime_instant_to_local(self.epoch.seconds, self.epoch.micros, zone)
                .map(|local| format_local(local, usize::from(self.epoch.fsp))),
        )
    }
}
/// Distinct original legacy visible-Decimal/f64/extra-nanos multiplication
/// policy.
pub(super) fn native_from_unixtime_legacy_values_budget(
    value: Option<NativeDecimalParseRef<'_>>,
    zone: &NativeSessionTimeZone,
    maximum: Budget,
) -> Result<Option<NativeTemporalValue>> {
    let Some(value) = value else { return Ok(None) };
    native_from_unixtime_numeric_work_budget(Input::Decimal(value), maximum)?;
    let text = visible(value);
    let unix = text
        .parse::<f64>()
        .expect("Decimal's own Display always produces valid float syntax");
    if !(0.0..=32_536_771_199.999_999).contains(&unix) {
        return Ok(None);
    }
    let whole = unix.trunc();
    let nanos = ((unix - whole) * 1e9).round() as u32;
    let Some(built) = zone.timestamp_opt(whole as i64, nanos * 1_000).single() else {
        return Ok(None);
    };
    Ok(NativeTemporalValue::new(native_core_from_datetime(built), TimeType::DateTime, 0).ok())
}

/// Original SQL text DATE_FORMAT over actual nullable coerced strings.
pub(super) fn native_date_format_text_values_budget(
    date: Option<&str>,
    layout: Option<&str>,
    maximum: Budget,
) -> Result<Option<String>> {
    let (Some(date), Some(layout)) = (date, layout) else {
        return Ok(None);
    };
    let n = date
        .len()
        .checked_add(layout.len())
        .ok_or(NativeEpochValueError::Resource)?;
    work(n, 64, 4096, maximum)?;
    Ok(Time::native_text_date_format(date, layout))
}
/// Original legacy raw-core DATE_FORMAT, with no text normalization or date
/// validation beyond its existing formatter's own conversion demands.
pub(super) fn native_date_format_core_values_budget(
    value: Option<(u64, Option<&str>)>,
    maximum: Budget,
) -> Result<Option<String>> {
    let Some((raw, Some(layout))) = value else {
        return Ok(None);
    };
    work(layout.len(), 64, 4096, maximum)?;
    Ok(Time::native_core_date_format(raw, layout))
}

/// Finite original text parser/diagnostic preflight.
pub fn native_epoch_text_work(text: &str, maximum: usize) -> Result<()> {
    native_epoch_text_work_budget(text, Budget::finite(maximum)?)
}
/// Actual now result with original arithmetic.
pub fn native_unix_timestamp_now_values(
    seconds: i64,
    nanos: u32,
    maximum: usize,
) -> Result<NativeUnixTimestampValue> {
    native_unix_timestamp_now_values_budget(seconds, nanos, Budget::finite(maximum)?)
}
/// Actual fixed-flag first parse, before a fresh second zone.
pub fn native_unix_timestamp_parse_values(
    text: &str,
    is_float: bool,
    zone: &NativeSessionTimeZone,
    maximum: usize,
) -> Result<NativeUnixTimestampValue> {
    native_unix_timestamp_parse_values_budget(text, is_float, zone, Budget::finite(maximum)?)
}
/// Strict legacy actual-Time conversion with its explicit zone.
pub fn native_unix_timestamp_legacy_values(
    value: Option<NativeTemporalValue>,
    zone: &NativeSessionTimeZone,
    want_decimal: bool,
    maximum: usize,
) -> Result<NativeUnixTimestampValue> {
    native_unix_timestamp_legacy_values_budget(value, zone, want_decimal, Budget::finite(maximum)?)
}
/// Preflight native numeric-kind text formatting.
pub fn native_from_unixtime_numeric_work(value: Input<'_>, maximum: usize) -> Result<()> {
    native_from_unixtime_numeric_work_budget(value, Budget::finite(maximum)?)
}
/// Native numeric-kind epoch preparation over the original actual source.
pub fn native_from_unixtime_numeric_values(
    value: Input<'_>,
    maximum: usize,
) -> Result<NativeFromUnixTimeValue> {
    native_from_unixtime_numeric_values_budget(value, Budget::finite(maximum)?)
}
/// Native actual text epoch preparation and original truncation diagnostic.
pub fn native_from_unixtime_text_values(
    text: &str,
    maximum: usize,
) -> Result<NativeFromUnixTimeValue> {
    native_from_unixtime_text_values_budget(text, Budget::finite(maximum)?)
}
/// Distinct original legacy visible-Decimal/f64/extra-nanos policy.
pub fn native_from_unixtime_legacy_values(
    value: Option<NativeDecimalParseRef<'_>>,
    zone: &NativeSessionTimeZone,
    maximum: usize,
) -> Result<Option<NativeTemporalValue>> {
    native_from_unixtime_legacy_values_budget(value, zone, Budget::finite(maximum)?)
}
/// Original SQL text formatting over actual nullable strings.
pub fn native_date_format_text_values(
    date: Option<&str>,
    layout: Option<&str>,
    maximum: usize,
) -> Result<Option<String>> {
    native_date_format_text_values_budget(date, layout, Budget::finite(maximum)?)
}
/// Original legacy raw-core formatting without text normalization.
pub fn native_date_format_core_values(
    value: Option<(u64, Option<&str>)>,
    maximum: usize,
) -> Result<Option<String>> {
    native_date_format_core_values_budget(value, Budget::finite(maximum)?)
}
impl NativeUnixTimestampParsed {
    /// Consume the actual time with the separately obtained second zone.
    pub fn finish(
        self,
        zone: &NativeSessionTimeZone,
        maximum: usize,
    ) -> Result<NativeUnixTimestampValue> {
        self.finish_budget(zone, Budget::finite(maximum)?)
    }
}
impl NativeFromUnixTimeValues {
    /// Preflight local output before demanding the zone getter.
    pub fn preflight(&self, maximum: usize) -> Result<()> {
        self.preflight_budget(Budget::finite(maximum)?)
    }
    /// Consume the actual epoch using the original local-rendering policy.
    pub fn local(self, zone: &NativeSessionTimeZone, maximum: usize) -> Result<Option<String>> {
        self.local_budget(zone, Budget::finite(maximum)?)
    }
}

#[cfg(test)]
mod actual_tests {
    use super::*;
    const AMPLE: usize = 1 << 20;
    fn value(v: NativeUnixTimestampValue) -> String {
        match v {
            NativeUnixTimestampValue::Null => "NULL".into(),
            NativeUnixTimestampValue::Int(n) => format!("Int:{n}"),
            NativeUnixTimestampValue::Decimal(d) => {
                let v = d.as_ref();
                format!(
                    "Dec:{:?}:{}:{}:{:?}",
                    v.digits, v.scale, v.storage_scale, v.declared_shape
                )
            }
            _ => panic!("terminal numeric expected"),
        }
    }
    #[test]
    fn actual_unix_retains_late_zone_full_zero_partial_zero_warning_and_strict_legacy() {
        let utc = NativeSessionTimeZone::utc();
        for text in ["0000-00-00 12:34:56.123", "0.123"] {
            assert!(matches!(
                native_unix_timestamp_parse_values(text, text == "0.123", &utc, AMPLE).unwrap(),
                NativeUnixTimestampValue::Null
            ));
        }
        assert_eq!(
            value(
                native_unix_timestamp_parse_values("2017-00-02 00:00:00.123", false, &utc, AMPLE)
                    .unwrap()
            ),
            "Dec:[48, 48, 48]:3:3:None"
        );
        assert!(
            matches!(native_unix_timestamp_parse_values("bad",false,&utc,AMPLE).unwrap(),NativeUnixTimestampValue::Warning(s) if s=="Incorrect datetime value: 'bad'")
        );
        let text = "1970-01-02 00:00:01.123456";
        let NativeUnixTimestampValue::NeedZone(state) =
            native_unix_timestamp_parse_values(text, false, &utc, AMPLE).unwrap()
        else {
            panic!()
        };
        assert_eq!(state.value.fsp, 6);
        let zone = NativeSessionTimeZone::Fixed {
            name: "raw+24".into(),
            offset_secs: 86400,
        };
        assert_eq!(
            value(state.finish(&zone, AMPLE).unwrap()),
            "Dec:[49, 49, 50, 51, 52, 53, 54]:6:6:None"
        );
        let paris = NativeSessionTimeZone::Named("Europe/Paris".parse().unwrap());
        let raw = Time::native_core_from_fields(2025, 3, 30, 2, 30, 0, 0);
        let NativeUnixTimestampValue::NeedZone(state) =
            native_unix_timestamp_parse_values("2025-03-30 02:30:00", false, &paris, AMPLE)
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            value(state.finish(&paris, AMPLE).unwrap()),
            "Int:1743296400"
        );
        let t = NativeTemporalValue {
            raw,
            kind: TimeType::Timestamp,
            fsp: 255,
        };
        assert_eq!(
            value(native_unix_timestamp_legacy_values(Some(t), &paris, true, AMPLE).unwrap()),
            "Dec:[48]:0:0:None"
        );
        let t = NativeTemporalValue {
            raw: Time::native_core_from_fields(1970, 1, 1, 0, 0, 1, 123456),
            kind: TimeType::Date,
            fsp: 255,
        };
        assert_eq!(
            value(native_unix_timestamp_legacy_values(Some(t), &utc, true, AMPLE).unwrap()),
            "Dec:[49, 49, 50, 51, 52, 53, 54]:6:6:None"
        );
        assert_eq!(
            value(native_unix_timestamp_now_values(1, u32::MAX, AMPLE).unwrap()),
            "Int:5"
        );
    }
    #[test]
    fn actual_from_epoch_keeps_source_precision_warning_epoch_round_carry_and_legacy_extra_nanos() {
        let utc = NativeSessionTimeZone::utc();
        let d = NativeDecimalParseRef {
            negative: false,
            digits: b"1255",
            scale: 2,
            storage_scale: 3,
            declared_shape: Some((30, 9)),
        };
        let NativeFromUnixTimeValue::Ready(s) =
            native_from_unixtime_numeric_values(Input::Decimal(d), AMPLE).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (s.epoch.seconds, s.epoch.micros, s.epoch.fsp),
            (1, 260000, 2)
        );
        assert_eq!(
            s.local(&utc, AMPLE).unwrap().as_deref(),
            Some("1970-01-01 00:00:01.26")
        );
        let NativeFromUnixTimeValue::Ready(s) =
            native_from_unixtime_numeric_values(Input::Float32(1.00000051), AMPLE).unwrap()
        else {
            panic!()
        };
        assert_eq!((s.epoch.seconds, s.epoch.micros, s.epoch.fsp), (1, 1, 6));
        for (text, seconds, micros) in [
            ("-0.5", 0, 500000),
            ("1.123456789junk", 1, 123457),
            ("32536771199.9999999", 32536771200, 0),
        ] {
            let NativeFromUnixTimeValue::Ready(s) =
                native_from_unixtime_text_values(text, AMPLE).unwrap()
            else {
                panic!()
            };
            assert_eq!((s.epoch.seconds, s.epoch.micros), (seconds, micros));
        }
        let NativeFromUnixTimeValue::Truncate { value: s, message } =
            native_from_unixtime_text_values(" 1e2 ", AMPLE).unwrap()
        else {
            panic!()
        };
        assert_eq!(message, "Truncated incorrect DECIMAL value: '1e2'");
        assert_eq!((s.epoch.seconds, s.epoch.micros, s.epoch.fsp), (0, 0, 0));
        assert_eq!(
            s.local(
                &NativeSessionTimeZone::Fixed {
                    name: "raw+24".into(),
                    offset_secs: 86400
                },
                AMPLE
            )
            .unwrap()
            .as_deref(),
            Some("1970-01-02 00:00:00")
        );
        let d = NativeDecimalParseRef {
            negative: false,
            digits: b"000001",
            scale: 6,
            storage_scale: 6,
            declared_shape: None,
        };
        let t = native_from_unixtime_legacy_values(Some(d), &utc, AMPLE)
            .unwrap()
            .unwrap();
        assert_eq!(
            (t.raw, t.fsp),
            (Time::native_core_from_fields(1970, 1, 1, 0, 0, 0, 1000), 0)
        );
        assert!(matches!(
            native_from_unixtime_numeric_values(Input::UInt(u64::MAX), AMPLE).unwrap(),
            NativeFromUnixTimeValue::Null
        ));
        for n in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(matches!(
                native_from_unixtime_numeric_values(Input::Real(n), AMPLE).unwrap(),
                NativeFromUnixTimeValue::Null
            ));
        }
    }
    #[test]
    fn actual_epoch_finite_work_precedes_parser_formatter_and_consumes_failed_states() {
        let utc = NativeSessionTimeZone::utc();
        for max in [0, 128, usize::MAX] {
            assert!(matches!(
                native_unix_timestamp_parse_values("2020-01-02", false, &utc, max),
                Err(NativeEpochValueError::Resource)
            ));
            assert!(matches!(
                native_from_unixtime_text_values("1e2", max),
                Err(NativeEpochValueError::Resource)
            ));
        }
        assert!(matches!(
            native_unix_timestamp_now_values(i64::MAX, u32::MAX, 0),
            Err(NativeEpochValueError::Resource)
        ));
        assert!(matches!(
            native_unix_timestamp_legacy_values(None, &utc, false, 0).unwrap(),
            NativeUnixTimestampValue::Null
        ));
        assert!(
            native_from_unixtime_legacy_values(None, &utc, 0)
                .unwrap()
                .is_none()
        );
        let bad = NativeDecimalParseRef {
            negative: false,
            digits: &[255],
            scale: 0,
            storage_scale: 0,
            declared_shape: None,
        };
        assert!(matches!(
            native_from_unixtime_numeric_values(Input::Decimal(bad), 0),
            Err(NativeEpochValueError::Resource)
        ));
        assert!(
            std::panic::catch_unwind(|| native_from_unixtime_numeric_values(
                Input::Decimal(bad),
                AMPLE
            ))
            .is_err()
        );
        let NativeUnixTimestampValue::NeedZone(s) =
            native_unix_timestamp_parse_values("2020-01-02", false, &utc, AMPLE).unwrap()
        else {
            panic!()
        };
        assert!(matches!(
            s.finish(&utc, 0),
            Err(NativeEpochValueError::Resource)
        ));
        let NativeFromUnixTimeValue::Ready(s) =
            native_from_unixtime_numeric_values(Input::Int(42), AMPLE).unwrap()
        else {
            panic!()
        };
        assert!(s.preflight(0).is_err());
        assert!(s.local(&utc, 0).is_err());
        assert!(matches!(
            native_from_unixtime_numeric_values(Input::String(b"42"), AMPLE),
            Err(NativeEpochValueError::InvalidKind)
        ));
    }
    #[test]
    fn actual_date_format_keeps_raw_core_text_policy_nullable_and_bounded_expansion() {
        let layout = "%Y-%m-%d %H:%i:%s.%f";
        assert_eq!(
            native_date_format_text_values(Some("2020-01-02 nonsense"), Some(layout), AMPLE)
                .unwrap()
                .as_deref(),
            Some("2020-01-02 00:00:00.000000")
        );
        let raw = Time::native_core_from_fields(2020, 0, 31, 3, 4, 5, 123456);
        assert_eq!(
            native_date_format_core_values(Some((raw, Some(layout))), AMPLE)
                .unwrap()
                .as_deref(),
            Some("2020-00-31 03:04:05.123456")
        );
        assert!(
            native_date_format_core_values(Some((raw, Some("%M"))), AMPLE)
                .unwrap()
                .is_none()
        );
        assert!(
            native_date_format_text_values(None, Some(&"%M".repeat(10000)), 0)
                .unwrap()
                .is_none()
        );
        assert!(native_date_format_core_values(None, 0).unwrap().is_none());
        assert!(
            native_date_format_text_values(Some("2020-01-02"), Some(&"%M".repeat(10000)), AMPLE)
                .is_err()
        );
    }
}
