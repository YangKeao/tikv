// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Actual legacy DATE_ADD/SUB state. Original source-channel demand is
//! explicit; there is no expression tree, report program or fabricated child
//! value.
use tidb_query_datatype::codec::mysql::{
    Decimal, NativeDecimalParseRef, NativeMyDecimal, NativeMyDecimalRoundMode, Time, TimeType,
    time::{
        NativeSessionTimeZone, NativeTemporalValue, native_core_add_date, native_core_add_duration,
        native_extract_duration_value, native_is_clock_unit, native_is_date_format,
        native_parse_duration_value, native_parse_time, native_parse_time_from_decimal_text,
        native_parse_time_from_float64, native_parse_time_from_int64,
    },
};

use crate::{
    NativeLegacyDateArithmeticChannel as Channel, NativeLegacyDateArithmeticDateKind as DateKind,
    NativeLegacyDateArithmeticIntervalKind as IntervalKind,
    NativeLegacyDateArithmeticMetadata as Metadata,
};
/// Failure of finite actual work or continuation sequencing, distinct from SQL
/// NULL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeLegacyDateArithmeticValueError {
    /// Demanded owned storage, parsing, formatting or arithmetic exceeds
    /// allowance.
    ResourceLimit,
    /// A response has the wrong channel or the continuation is no longer live.
    InvalidResponse,
}
use NativeLegacyDateArithmeticValueError as Error;
type Result<T> = std::result::Result<T, Error>;
/// Borrowed original nullable channel result; integers retain their full folded
/// width.
pub enum NativeLegacyDateArithmeticInput<'a> {
    /// Original byte channel, before its lossy UTF-8 normalization.
    Bytes(Option<&'a [u8]>),
    /// Original folded integer channel.
    FoldedInt(Option<i128>),
    /// Original floating-point channel.
    Real(Option<f64>),
    /// Original exact raw Decimal coefficient and metadata.
    Decimal(Option<NativeDecimalParseRef<'a>>),
    /// Original raw calendar value, kind and FSP.
    Time(Option<NativeTemporalValue>),
    /// Original nanoseconds and unnormalized FSP.
    Duration(Option<(i64, i64)>),
}
use NativeLegacyDateArithmeticInput as Input;
impl Input<'_> {
    fn channel(&self) -> Channel {
        match self {
            Self::Bytes(_) => Channel::Bytes,
            Self::FoldedInt(_) => Channel::FoldedInt,
            Self::Real(_) => Channel::Real,
            Self::Decimal(_) => Channel::Decimal,
            Self::Time(_) => Channel::Time,
            Self::Duration(_) => Channel::Duration,
        }
    }
    fn is_null(&self) -> bool {
        match self {
            Self::Bytes(v) => v.is_none(),
            Self::FoldedInt(v) => v.is_none(),
            Self::Real(v) => v.is_none(),
            Self::Decimal(v) => v.is_none(),
            Self::Time(v) => v.is_none(),
            Self::Duration(v) => v.is_none(),
        }
    }
}
/// Actual terminal value, with no identity frame or result report.
#[derive(Debug, PartialEq)]
pub enum NativeLegacyDateArithmeticValue {
    /// Original SQL NULL.
    Null,
    /// Original legacy text bytes.
    Text(Vec<u8>),
    /// Original raw temporal result.
    Time(NativeTemporalValue),
    /// Original duration nanoseconds and FSP.
    Duration {
        /// Shifted nanoseconds.
        nanos: i64,
        /// Original max(source,delta) FSP.
        fsp: i64,
    },
}
use NativeLegacyDateArithmeticValue as Value;
/// Actual next operation, or the value and its independent presence projection.
#[derive(Debug, PartialEq)]
pub enum NativeLegacyDateArithmeticValueStep {
    /// Demand one original source. The owner must retain that channel's
    /// original errors.
    Source {
        /// Original child ordinal.
        index: usize,
        /// Original child reader channel.
        channel: Channel,
    },
    /// Parse the retained original date using the explicit legacy timezone.
    ParseDate,
    /// Execute the original generic MyDecimal primitive over retained text.
    DecimalText {
        /// HalfUp to zero for non-composite Decimal intervals only.
        round_to_zero: bool,
    },
    /// One actual result and the SDK's separately produced presence.
    Ready {
        /// Actual value.
        value: Value,
        /// Zero for NULL, one for a present result, independent of truth.
        presence: i128,
    },
}
use NativeLegacyDateArithmeticValueStep as Step;
pub(super) struct OwnedDecimal {
    negative: bool,
    digits: Vec<u8>,
    scale: u32,
    storage_scale: u32,
    shape: Option<(i64, i64)>,
}
impl OwnedDecimal {
    fn view(&self) -> NativeDecimalParseRef<'_> {
        NativeDecimalParseRef {
            negative: self.negative,
            digits: &self.digits,
            scale: self.scale,
            storage_scale: self.storage_scale,
            declared_shape: self.shape,
        }
    }
}
pub(super) enum Date {
    Bytes(Vec<u8>),
    Int(i128),
    Real(f64),
    Decimal(OwnedDecimal),
}
impl Date {
    fn retained(&self) -> usize {
        match self {
            Self::Bytes(v) => v.capacity(),
            Self::Decimal(v) => v.digits.capacity(),
            _ => 0,
        }
    }
}
/// Opaque actual continuation. It owns only demanded unit/date/decimal scratch;
/// source readers and the caller's original borrowed storage stay outside it.
pub struct NativeLegacyDateArithmeticValues {
    pub(super) meta: Metadata,
    pub(super) phase: u8,
    pub(super) unit: Option<String>,
    pub(super) date: Option<Date>,
    pub(super) time: Option<NativeTemporalValue>,
    pub(super) duration: Option<(i64, i64)>,
    pub(super) text: Option<String>,
    maximum: Option<usize>,
    started: bool,
    // An advance reopens only on success; unwind leaves the owner closed.
    live: bool,
}
impl NativeLegacyDateArithmeticValues {
    /// Construct a finite continuation without reading any source.
    pub fn new(meta: Metadata, maximum: usize) -> Result<Self> {
        if maximum == usize::MAX {
            return Err(Error::ResourceLimit);
        }
        Ok(Self::initial(meta, Some(maximum)))
    }
    fn initial(meta: Metadata, maximum: Option<usize>) -> Self {
        Self {
            meta,
            phase: if matches!(meta.date, DateKind::Datetime | DateKind::Duration) {
                1
            } else {
                0
            },
            unit: None,
            date: None,
            time: None,
            duration: None,
            text: None,
            maximum,
            started: false,
            live: true,
        }
    }
    pub(super) fn compat_initial(meta: Metadata) -> Self {
        Self::initial(meta, None)
    }
    pub(super) fn from_compat_parts(
        meta: Metadata,
        phase: u8,
        unit: Option<&str>,
        date: Option<Input<'_>>,
        time: Option<NativeTemporalValue>,
        duration: Option<(i64, i64)>,
        text: Option<&str>,
    ) -> Result<Self> {
        let mut s = Self::initial(meta, None);
        s.phase = phase;
        s.started = true;
        s.unit = unit.map(str::to_owned);
        s.date = date.map(|v| s.copy_date(v)).transpose()?;
        s.time = time;
        s.duration = duration;
        s.text = text.map(str::to_owned);
        Ok(s)
    }
    pub(super) fn text_domain(&self) -> bool {
        !matches!(self.meta.date, DateKind::Datetime | DateKind::Duration)
    }
    /// Actual capacities of owned continuation payloads.
    pub fn retained_bytes(&self) -> Option<usize> {
        self.unit
            .as_ref()
            .map_or(0, String::capacity)
            .checked_add(self.date.as_ref().map_or(0, Date::retained))?
            .checked_add(self.text.as_ref().map_or(0, String::capacity))
    }
    /// Finite work remaining after actual owned continuation capacities.
    pub fn remaining_bytes(&self) -> Result<usize> {
        match self.maximum {
            Some(n) => n
                .checked_sub(self.retained_bytes().ok_or(Error::ResourceLimit)?)
                .ok_or(Error::ResourceLimit),
            None => Ok(usize::MAX),
        }
    }
    fn work(&self, n: usize) -> Result<()> {
        if self.maximum.is_some() && n > self.remaining_bytes()? {
            Err(Error::ResourceLimit)
        } else {
            Ok(())
        }
    }
    fn extent(&self, n: usize, m: usize, extra: usize) -> Result<()> {
        self.work(
            n.checked_mul(m)
                .and_then(|v| v.checked_add(extra))
                .ok_or(Error::ResourceLimit)?,
        )
    }
    fn ready(&mut self, value: Value) -> Step {
        self.phase = 255;
        let presence = i128::from(!matches!(value, Value::Null));
        Step::Ready { value, presence }
    }
    fn step(&self) -> Result<Step> {
        Ok(match self.phase {
            0 => Step::Source {
                index: 2,
                channel: Channel::Bytes,
            },
            1 => Step::Source {
                index: 0,
                channel: match self.meta.date {
                    DateKind::String => Channel::Bytes,
                    DateKind::Int => Channel::FoldedInt,
                    DateKind::Real => Channel::Real,
                    DateKind::Decimal => Channel::Decimal,
                    DateKind::Datetime => Channel::Time,
                    DateKind::Duration => Channel::Duration,
                },
            },
            2 => Step::ParseDate,
            3 => Step::Source {
                index: 1,
                channel: match self.meta.interval {
                    IntervalKind::String => Channel::Bytes,
                    IntervalKind::Int => Channel::FoldedInt,
                    IntervalKind::Real => Channel::Real,
                    IntervalKind::Decimal => Channel::Decimal,
                },
            },
            4 | 5 => Step::DecimalText {
                round_to_zero: self.phase == 5,
            },
            _ => return Err(Error::InvalidResponse),
        })
    }
    fn attempt(&mut self, run: impl FnOnce(&mut Self) -> Result<Step>) -> Result<Step> {
        if !self.live {
            return Err(Error::InvalidResponse);
        }
        self.live = false;
        let result = run(self);
        self.live = result.is_ok();
        if result.is_err() {
            self.phase = 254
        }
        result
    }
    /// Start once. Failed advances, including invariant unwind, permanently
    /// invalidate this continuation without catching or replacing the panic.
    pub fn begin(&mut self) -> Result<Step> {
        self.attempt(|state| {
            if state.started {
                Err(Error::InvalidResponse)
            } else {
                state.started = true;
                state.step()
            }
        })
    }
    fn lossy(&self, bytes: &[u8]) -> Result<String> {
        self.extent(bytes.len(), 3, 128)?;
        let v = String::from_utf8_lossy(bytes).into_owned();
        self.work(v.capacity())?;
        Ok(v)
    }
    fn visible_decimal(&self, v: NativeDecimalParseRef<'_>) -> Result<String> {
        let n = crate::native_string_coercion_work(
            tidb_query_datatype::codec::native_sql_string::NativeSqlStringInput::Decimal(v),
        )
        .map_err(|_| Error::ResourceLimit)?;
        self.work(n)?;
        let text = Decimal::native_format_visible(v.negative, v.digits, v.scale, v.storage_scale);
        self.work(text.capacity())?;
        Ok(text)
    }
    fn copy_date(&self, value: Input<'_>) -> Result<Date> {
        Ok(match value {
            Input::Bytes(Some(v)) => {
                self.extent(v.len(), 2, 128)?;
                Date::Bytes(v.to_vec())
            }
            Input::FoldedInt(Some(v)) => Date::Int(v),
            Input::Real(Some(v)) => Date::Real(v),
            Input::Decimal(Some(v)) => {
                self.extent(v.digits.len(), 2, 128)?;
                Date::Decimal(OwnedDecimal {
                    negative: v.negative,
                    digits: v.digits.to_vec(),
                    scale: v.scale,
                    storage_scale: v.storage_scale,
                    shape: v.declared_shape,
                })
            }
            _ => return Err(Error::InvalidResponse),
        })
    }
    fn resume_source_inner(&mut self, value: Input<'_>) -> Result<Step> {
        if !self.started {
            return Err(Error::InvalidResponse);
        }
        let Step::Source { channel, .. } = self.step()? else {
            return Err(Error::InvalidResponse);
        };
        if value.channel() != channel {
            return Err(Error::InvalidResponse);
        }
        if value.is_null() {
            return Ok(self.ready(Value::Null));
        }
        match self.phase {
            0 => {
                let Input::Bytes(Some(v)) = value else {
                    unreachable!()
                };
                self.unit = Some(self.lossy(v)?);
                self.phase = if self.text_domain() { 1 } else { 3 };
                self.step()
            }
            1 if self.text_domain() => {
                if matches!(value,Input::FoldedInt(Some(v)) if i64::try_from(v).is_err()) {
                    return Ok(self.ready(Value::Null));
                }
                self.date = Some(self.copy_date(value)?);
                self.phase = 2;
                self.step()
            }
            1 => {
                match value {
                    Input::Time(Some(v)) => self.time = Some(v),
                    Input::Duration(Some(v)) => self.duration = Some(v),
                    _ => return Err(Error::InvalidResponse),
                };
                self.phase = 0;
                self.step()
            }
            3 => match value {
                Input::Bytes(Some(v)) => {
                    let text = self.lossy(v)?;
                    self.string_interval(text)
                }
                Input::FoldedInt(Some(v)) => {
                    self.work(128)?;
                    self.apply(&v.to_string())
                }
                Input::Real(Some(v)) => {
                    self.work(512)?;
                    self.apply(&v.to_string())
                }
                Input::Decimal(Some(v)) => {
                    let text = self.visible_decimal(v)?;
                    self.decimal_interval(text)
                }
                _ => Err(Error::InvalidResponse),
            },
            _ => Err(Error::InvalidResponse),
        }
    }
    /// Supply exactly the requested original source channel once.
    pub fn resume_source(&mut self, value: Input<'_>) -> Result<Step> {
        self.attempt(|state| state.resume_source_inner(value))
    }
    fn parse_inner(&mut self, zone: &NativeSessionTimeZone) -> Result<Step> {
        if self.phase != 2 || !self.started {
            return Err(Error::InvalidResponse);
        }
        let unit = self.unit.as_deref().ok_or(Error::InvalidResponse)?;
        self.extent(unit.len(), 2, 0)?;
        let clock = native_is_clock_unit(unit);
        let date = self.date.as_ref().ok_or(Error::InvalidResponse)?;
        let parsed = match date {
            Date::Bytes(v) => {
                self.extent(v.len(), 64, 4096)?;
                let text = String::from_utf8_lossy(v);
                let kind = if !native_is_date_format(&text) || clock {
                    TimeType::DateTime
                } else {
                    TimeType::Date
                };
                native_parse_time(&text, kind, 6, false, false, false, true, zone)
                    .map(|p| p.time)
                    .ok()
            }
            Date::Int(v) => {
                self.work(4096)?;
                i64::try_from(*v)
                    .ok()
                    .and_then(|v| native_parse_time_from_int64(v, false, false, zone).ok())
            }
            Date::Real(v) => {
                self.work(4096)?;
                native_parse_time_from_float64(*v, false, false, zone)
                    .into_result()
                    .ok()
            }
            Date::Decimal(v) => {
                let text = self.visible_decimal(v.view())?;
                self.extent(text.len(), 64, 4096)?;
                native_parse_time_from_decimal_text(&text, false, false, zone)
                    .into_result()
                    .ok()
            }
        };
        let Some(mut time) = parsed else {
            return Ok(self.ready(Value::Null));
        };
        if clock && self.meta.date != DateKind::String {
            time.set_kind(TimeType::DateTime)
        }
        if time.raw == 0 {
            return Ok(self.ready(Value::Null));
        }
        self.date = None;
        self.time = Some(time);
        self.phase = 3;
        self.step()
    }
    /// Parse the retained date using the caller's explicit legacy timezone and
    /// fixed flags.
    pub fn parse_date(&mut self, zone: &NativeSessionTimeZone) -> Result<Step> {
        self.attempt(|state| state.parse_inner(zone))
    }
    fn string_interval(&mut self, text: String) -> Result<Step> {
        let unit = self.unit.as_deref().ok_or(Error::InvalidResponse)?;
        self.extent(unit.len(), 2, 0)?;
        match unit.to_ascii_uppercase().as_str() {
            "MICROSECOND" | "MINUTE" | "HOUR" | "DAY" | "WEEK" | "MONTH" | "QUARTER" | "YEAR" => {
                let trimmed = text.trim();
                let bytes = trimmed.as_bytes();
                let mut end = usize::from(matches!(bytes.first(), Some(b'+') | Some(b'-')));
                let from = end;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1
                }
                self.apply(if end == from { "0" } else { &trimmed[..end] })
            }
            "SECOND" => {
                self.text = Some(text);
                self.phase = 4;
                self.step()
            }
            _ => self.apply(&text),
        }
    }
    fn decimal_interval(&mut self, text: String) -> Result<Step> {
        let unit = self.unit.as_deref().ok_or(Error::InvalidResponse)?;
        self.extent(unit.len(), 2, 0)?;
        self.extent(text.len(), 4, 256)?;
        let reformatted = match unit.to_ascii_uppercase().as_str() {
            "HOUR_MINUTE" | "MINUTE_SECOND" => text.replace('.', ":"),
            "YEAR_MONTH" => text.replace('.', "-"),
            "DAY_HOUR" => text.replace('.', " "),
            "DAY_MINUTE" => format!("0 {}", text.replace('.', ":")),
            "DAY_SECOND" => format!("0 00:{}", text.replace('.', ":")),
            "DAY_MICROSECOND" => format!("0 00:00:{text}"),
            "HOUR_MICROSECOND" => format!("00:00:{text}"),
            "HOUR_SECOND" => format!("00:{}", text.replace('.', ":")),
            "MINUTE_MICROSECOND" => format!("00:{text}"),
            "SECOND" | "SECOND_MICROSECOND" => text.to_owned(),
            _ => {
                self.text = Some(text);
                self.phase = 5;
                return self.step();
            }
        };
        self.work(reformatted.capacity())?;
        self.apply(&reformatted)
    }
    fn apply(&mut self, interval: &str) -> Result<Step> {
        let unit = self.unit.as_deref().ok_or(Error::InvalidResponse)?;
        let n = interval
            .len()
            .checked_add(unit.len())
            .ok_or(Error::ResourceLimit)?;
        self.extent(n, 64, 4096)?;
        if let Some((nanos, fsp)) = self.duration {
            let Ok((delta, delta_fsp)) = native_extract_duration_value(unit, interval) else {
                return Ok(self.ready(Value::Null));
            };
            let nanos = if self.meta.subtract {
                nanos.checked_sub(delta)
            } else {
                nanos.checked_add(delta)
            };
            return Ok(match nanos {
                Some(nanos) => self.ready(Value::Duration {
                    nanos,
                    fsp: fsp.max(delta_fsp),
                }),
                None => self.ready(Value::Null),
            });
        }
        let mut time = self.time.ok_or(Error::InvalidResponse)?;
        let Ok(parsed) = native_parse_duration_value(unit, interval) else {
            return Ok(self.ready(Value::Null));
        };
        let sign = if self.meta.subtract { -1_i64 } else { 1_i64 };
        let raw = native_core_add_duration(time.raw, sign * parsed.nanoseconds);
        let Some(raw) = native_core_add_date(
            raw,
            sign * parsed.years,
            sign * parsed.months,
            sign * parsed.days,
        ) else {
            return Ok(self.ready(Value::Null));
        };
        time.raw = raw;
        if self.text_domain() {
            let fsp = i64::from(Time::native_core_fields(raw)[6] != 0) * 6;
            if time.set_fsp(fsp).is_err() {
                return Ok(self.ready(Value::Null));
            }
            self.work(256)?;
            let mut text = String::new();
            Time::write_native_core_display(
                time.raw,
                time.kind == TimeType::Date,
                time.fsp,
                &mut text,
            )
            .expect("String formatting cannot fail");
            self.work(text.capacity())?;
            Ok(self.ready(Value::Text(text.into_bytes())))
        } else {
            Ok(self.ready(Value::Time(time)))
        }
    }
    /// Actual pending decimal text, for explicit compatibility owners only.
    pub fn decimal_text(&self) -> Result<&str> {
        if !self.live {
            return Err(Error::InvalidResponse);
        }
        self.decimal_text_inner()
    }
    fn decimal_text_inner(&self) -> Result<&str> {
        if !matches!(self.phase, 4 | 5) || !self.started {
            return Err(Error::InvalidResponse);
        };
        self.text.as_deref().ok_or(Error::InvalidResponse)
    }
    fn decimal_reply_inner(&mut self, text: &[u8]) -> Result<Step> {
        if !matches!(self.phase, 4 | 5) || !self.started {
            return Err(Error::InvalidResponse);
        }
        self.extent(text.len(), 3, 128)?;
        self.apply(&String::from_utf8_lossy(text))
    }
    /// Explicit wire compatibility reply: consume the original generic
    /// primitive's bytes.
    pub fn resume_decimal_text(&mut self, text: &[u8]) -> Result<Step> {
        self.attempt(|state| state.decimal_reply_inner(text))
    }
    fn decimal_inner(&mut self) -> Result<Step> {
        let text = self.decimal_text_inner()?;
        self.extent(text.len(), 32, 4096)?;
        // Exactly the shared primitive used by the frontend MyDecimal facade.
        let (mut decimal, _) = NativeMyDecimal::from_string(text.as_bytes());
        if self.phase == 5 {
            decimal.round_in_place(0, NativeMyDecimalRoundMode::HalfUp);
        }
        let bytes = decimal.to_string_bytes();
        self.work(bytes.capacity())?;
        self.decimal_reply_inner(&bytes)
    }
    /// Run the original shared MyDecimal parse/optional HalfUp/render
    /// primitive.
    pub fn finish_decimal(&mut self) -> Result<Step> {
        self.attempt(Self::decimal_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn zone() -> NativeSessionTimeZone {
        NativeSessionTimeZone::utc()
    }
    fn time() -> NativeTemporalValue {
        native_parse_time(
            "2020-01-31",
            TimeType::DateTime,
            0,
            false,
            true,
            false,
            true,
            &zone(),
        )
        .unwrap()
        .time
    }
    fn source(meta: Metadata, index: usize) -> Input<'static> {
        if index == 2 {
            return Input::Bytes(Some(if meta.date == DateKind::Duration {
                b"SECOND"
            } else {
                b"DAY"
            }));
        }
        if index == 0 {
            return match meta.date {
                DateKind::String => Input::Bytes(Some(b"2020-01-31")),
                DateKind::Int => Input::FoldedInt(Some(20200131)),
                DateKind::Real => Input::Real(Some(20200131.0)),
                DateKind::Decimal => Input::Decimal(Some(NativeDecimalParseRef {
                    negative: false,
                    digits: b"20200131",
                    scale: 0,
                    storage_scale: 0,
                    declared_shape: None,
                })),
                DateKind::Datetime => Input::Time(Some(time())),
                DateKind::Duration => Input::Duration(Some((0, 6))),
            };
        }
        match meta.interval {
            IntervalKind::String => Input::Bytes(Some(b"1")),
            IntervalKind::Int => Input::FoldedInt(Some(1)),
            IntervalKind::Real => Input::Real(Some(1.0)),
            IntervalKind::Decimal => Input::Decimal(Some(NativeDecimalParseRef {
                negative: false,
                digits: b"1",
                scale: 0,
                storage_scale: 0,
                declared_shape: None,
            })),
        }
    }
    fn null(channel: Channel) -> Input<'static> {
        match channel {
            Channel::Bytes => Input::Bytes(None),
            Channel::FoldedInt => Input::FoldedInt(None),
            Channel::Real => Input::Real(None),
            Channel::Decimal => Input::Decimal(None),
            Channel::Time => Input::Time(None),
            Channel::Duration => Input::Duration(None),
        }
    }
    fn prefix(meta: Metadata, wanted: usize) -> NativeLegacyDateArithmeticValues {
        let mut state = NativeLegacyDateArithmeticValues::new(meta, 8192).unwrap();
        let mut step = state.begin().unwrap();
        loop {
            step = match step {
                Step::Source { index, .. } if index == wanted => return state,
                Step::Source { index, .. } => state.resume_source(source(meta, index)).unwrap(),
                Step::ParseDate => state.parse_date(&zone()).unwrap(),
                Step::DecimalText { .. } => state.finish_decimal().unwrap(),
                Step::Ready { .. } => panic!("wanted source was skipped"),
            }
        }
    }
    #[test]
    fn legacy_actual_all_source_prefixes_validate_channel_null_and_terminal_generation() {
        for date in [
            DateKind::String,
            DateKind::Int,
            DateKind::Real,
            DateKind::Decimal,
            DateKind::Datetime,
            DateKind::Duration,
        ] {
            for interval in [
                IntervalKind::String,
                IntervalKind::Int,
                IntervalKind::Real,
                IntervalKind::Decimal,
            ] {
                for index in 0..3 {
                    let meta = Metadata {
                        date,
                        interval,
                        subtract: false,
                    };
                    let mut state = prefix(meta, index);
                    let Step::Source { channel, .. } = state.step().unwrap() else {
                        panic!()
                    };
                    let wrong = if channel == Channel::Real {
                        Input::Bytes(None)
                    } else {
                        Input::Real(None)
                    };
                    assert!(matches!(
                        state.resume_source(wrong),
                        Err(Error::InvalidResponse)
                    ));
                    assert!(matches!(
                        state.resume_source(null(channel)),
                        Err(Error::InvalidResponse)
                    ));
                    let mut state = prefix(meta, index);
                    assert_eq!(
                        state.resume_source(null(channel)).unwrap(),
                        Step::Ready {
                            value: Value::Null,
                            presence: 0
                        }
                    );
                    assert!(matches!(state.begin(), Err(Error::InvalidResponse)));
                    assert!(matches!(
                        state.resume_source(source(meta, index)),
                        Err(Error::InvalidResponse)
                    ));
                }
            }
        }
    }
    #[test]
    fn legacy_actual_zero_time_demands_later_sources_and_duration_zero_is_present() {
        let meta = Metadata {
            date: DateKind::Datetime,
            interval: IntervalKind::Int,
            subtract: false,
        };
        let mut state = NativeLegacyDateArithmeticValues::new(meta, 8192).unwrap();
        state.begin().unwrap();
        assert_eq!(
            state
                .resume_source(Input::Time(Some(NativeTemporalValue {
                    raw: 0,
                    kind: TimeType::DateTime,
                    fsp: 0
                })))
                .unwrap(),
            Step::Source {
                index: 2,
                channel: Channel::Bytes
            }
        );
        assert_eq!(
            state.resume_source(Input::Bytes(Some(b"DAY"))).unwrap(),
            Step::Source {
                index: 1,
                channel: Channel::FoldedInt
            }
        );
        let meta = Metadata {
            date: DateKind::Int,
            ..meta
        };
        let mut state = prefix(meta, 0);
        assert_eq!(
            state
                .resume_source(Input::FoldedInt(Some(i128::MAX)))
                .unwrap(),
            Step::Ready {
                value: Value::Null,
                presence: 0
            }
        );
        let meta = Metadata {
            date: DateKind::Duration,
            ..meta
        };
        let mut state = prefix(meta, 1);
        assert_eq!(
            state.resume_source(Input::FoldedInt(Some(0))).unwrap(),
            Step::Ready {
                value: Value::Duration { nanos: 0, fsp: 6 },
                presence: 1
            }
        );
    }
    #[test]
    fn legacy_actual_demanded_work_refuses_before_owned_format_parse_and_generic_decimal() {
        let meta = Metadata {
            date: DateKind::String,
            interval: IntervalKind::String,
            subtract: false,
        };
        let mut state = NativeLegacyDateArithmeticValues::new(meta, 0).unwrap();
        state.begin().unwrap();
        assert_eq!(
            state.resume_source(Input::Bytes(None)).unwrap(),
            Step::Ready {
                value: Value::Null,
                presence: 0
            }
        );
        assert!(NativeLegacyDateArithmeticValues::new(meta, usize::MAX).is_err());
        let mut state = NativeLegacyDateArithmeticValues::new(meta, 32).unwrap();
        state.begin().unwrap();
        assert!(matches!(
            state.resume_source(Input::Bytes(Some(&[b'X'; 128]))),
            Err(Error::ResourceLimit)
        ));
        assert!(matches!(
            state.resume_source(Input::Bytes(None)),
            Err(Error::InvalidResponse)
        ));
        let meta = Metadata {
            date: DateKind::Decimal,
            ..meta
        };
        let mut state = prefix(meta, 0);
        state
            .resume_source(Input::Decimal(Some(NativeDecimalParseRef {
                negative: false,
                digits: b"1",
                scale: u32::MAX,
                storage_scale: 0,
                declared_shape: None,
            })))
            .unwrap();
        assert!(matches!(
            state.parse_date(&zone()),
            Err(Error::ResourceLimit)
        ));
        assert!(matches!(
            state.parse_date(&zone()),
            Err(Error::InvalidResponse)
        ));
        let meta = Metadata {
            date: DateKind::String,
            ..meta
        };
        let mut state = prefix(meta, 0);
        state
            .resume_source(Input::Bytes(Some(&[b'1'; 1024])))
            .unwrap();
        assert!(matches!(
            state.parse_date(&zone()),
            Err(Error::ResourceLimit)
        ));
        let meta = Metadata {
            date: DateKind::Duration,
            ..meta
        };
        let mut state = prefix(meta, 1);
        assert_eq!(
            state
                .resume_source(Input::Bytes(Some(&[b'1'; 2048])))
                .unwrap(),
            Step::DecimalText {
                round_to_zero: false
            }
        );
        assert!(matches!(state.finish_decimal(), Err(Error::ResourceLimit)));
        assert!(matches!(
            state.resume_decimal_text(b"0"),
            Err(Error::InvalidResponse)
        ));
        // A newly constructed valid owner works after independent failures.
        let mut state = prefix(meta, 1);
        assert_eq!(
            state.resume_source(Input::Bytes(Some(b"0"))).unwrap(),
            Step::DecimalText {
                round_to_zero: false
            }
        );
        assert_eq!(
            state.finish_decimal().unwrap(),
            Step::Ready {
                value: Value::Duration { nanos: 0, fsp: 6 },
                presence: 1
            }
        );
    }
    #[test]
    fn legacy_actual_invariant_unwind_cannot_be_replayed_as_sql_null() {
        let meta = Metadata {
            date: DateKind::Duration,
            interval: IntervalKind::Decimal,
            subtract: false,
        };
        let mut state = prefix(meta, 1);
        // Preserve the original raw-coefficient formatter panic, not a new
        // normalization or lossy conversion of invalid coefficient bytes.
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state.resume_source(Input::Decimal(Some(NativeDecimalParseRef {
                negative: false,
                digits: b"\xff",
                scale: 0,
                storage_scale: 0,
                declared_shape: None,
            })))
        }));
        assert!(panic.is_err());
        assert!(matches!(
            state.resume_source(Input::Decimal(None)),
            Err(Error::InvalidResponse)
        ));
    }
}
