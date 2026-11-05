// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Actual ordinary date-arithmetic continuation. There are no expression trees,
//! transport headers or precomputed responses in this state. The owner supplies
//! only demanded datatype conversions and statement overflow disposition.
use std::borrow::Cow;

use chrono::Utc;
use tidb_query_datatype::codec::{
    mysql::{Decimal, NativeDecimalParseRef, Time, TimeType, time::native_parse_time_from_num},
    native_sql_string::NativeSqlStringInput as Input,
};

use crate::{NativeDateArithmeticRequest as Request, native_date_arithmetic_helpers as helpers};

pub(super) const OVERFLOW: &str = "Datetime function: datetime field overflow";

/// Actual work or continuation-contract failure, distinct from SQL outcomes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDateArithmeticValueError {
    /// Demanded scratch, formatting, parsing or result exceeds its allowance.
    ResourceLimit,
    /// A response is supplied to a different or completed continuation.
    InvalidResponse,
}
use NativeDateArithmeticValueError as Error;
type Result<T> = std::result::Result<T, Error>;

/// Typed metadata, preserving the separate calendar and duration domains.
#[derive(Clone, Copy, Debug)]
pub struct NativeDateArithmeticValueConfig {
    pub(super) duration: bool,
    pub(super) sign: i64,
    pub(super) fsp: Option<u32>,
    pub(super) duration_fsp: i64,
    pub(super) decimal: Option<i64>,
}
impl NativeDateArithmeticValueConfig {
    /// Calendar result precision retains None's value-dependent rendering.
    pub const fn calendar(sign: i64, result_fsp: Option<u32>) -> Self {
        Self {
            duration: false,
            sign,
            fsp: result_fsp,
            duration_fsp: 0,
            decimal: None,
        }
    }
    /// Duration result FSP and amount declared scale remain unnormalized.
    pub const fn duration(sign: i64, result_fsp: i64, amount_decimal: Option<i64>) -> Self {
        Self {
            duration: true,
            sign,
            fsp: None,
            duration_fsp: result_fsp,
            decimal: amount_decimal,
        }
    }
}
/// Late overflow disposition, requested only after actual calendar overflow.
#[derive(Clone, Copy, Debug)]
pub enum NativeDateArithmeticOverflowMode {
    /// Return NULL without a warning.
    Ignore,
    /// Append the original1441 warning, then return NULL.
    Warn,
    /// Return the original1441 conversion error.
    Error,
}
/// An original unconditional warning owned until its next demand boundary.
#[derive(Debug)]
pub struct NativeDateArithmeticValueWarning {
    /// Original MySQL warning code.
    pub code: u16,
    /// Exact original diagnostic text.
    pub message: String,
}
/// Actual terminal value; unsupported and overflow stay separate SQL outcomes.
#[derive(Debug)]
pub enum NativeDateArithmeticValue {
    /// SQL NULL.
    Null,
    /// Calendar output text before original caller result conversion.
    Text(String),
    /// Actual duration, retaining raw result precision.
    Duration {
        /// Original signed nanoseconds.
        nanos: i64,
        /// Original result metadata, including nonstandard raw precision.
        fsp: i64,
    },
    /// Original unsupported-operation reason.
    Unsupported(&'static str),
    /// Original1441 overflow message.
    Error(&'static str),
}
/// The next actual operation, never an expression-evaluation callback.
#[derive(Debug)]
pub enum NativeDateArithmeticValueAction {
    /// Actual terminal result.
    Ready(NativeDateArithmeticValue),
    /// A demanded conversion of an original operand or current decimal text.
    Request {
        /// Original conversion policy.
        kind: Request,
        /// Original operand position.
        index: usize,
    },
    /// Read the original statement's overflow disposition now.
    Overflow,
}
/// Warning delivery precedes the action. Internal work may replace an earlier
/// pending warning before reaching this external boundary, just as originally.
#[derive(Debug)]
pub struct NativeDateArithmeticValueStep {
    /// At most one original pending warning at this boundary.
    pub warning: Option<NativeDateArithmeticValueWarning>,
    /// Actual terminal value or next demanded operation.
    pub action: NativeDateArithmeticValueAction,
}
use NativeDateArithmeticValue as Value;
use NativeDateArithmeticValueAction as Action;
use NativeDateArithmeticValueStep as Step;
use NativeDateArithmeticValueWarning as Warning;

#[derive(Clone, Copy, Debug)]
pub(super) enum ValueInput<'a> {
    Null,
    Int(i64),
    UInt(u64),
    Decimal(NativeDecimalParseRef<'a>),
    Real(f64),
    Float32(f64),
    Text,
    Duration(i64),
    Sentinel,
    Other,
}
impl<'a> From<Input<'a>> for ValueInput<'a> {
    fn from(value: Input<'a>) -> Self {
        match value {
            Input::Null => Self::Null,
            Input::Int(v) => Self::Int(v),
            Input::UInt(v) => Self::UInt(v),
            Input::Decimal(v) => Self::Decimal(v),
            Input::Real(v) => Self::Real(v),
            Input::Float32(v) => Self::Float32(v),
            Input::String(_) | Input::Bytes(_) => Self::Text,
            Input::Duration { nanoseconds, .. } => Self::Duration(nanoseconds),
            Input::MinNotNull | Input::MaxValue => Self::Sentinel,
            _ => Self::Other,
        }
    }
}

/// Opaque actual continuation. Borrowed original storage survives no source
/// callback in the canonical owner: that owner detaches bounded operands first.
pub struct NativeDateArithmeticValues<'a> {
    pub(super) inputs: [ValueInput<'a>; 2],
    pub(super) unit: &'a str,
    pub(super) config: NativeDateArithmeticValueConfig,
    pub(super) date_text: Option<Cow<'a, str>>,
    pub(super) amount_text: Option<Cow<'a, str>>,
    pub(super) number: Option<i64>,
    pub(super) prepared: u8,
    pub(super) phase: u8,
    pub(super) maximum: Option<usize>,
    warning: Option<Warning>,
    // Owned coercion response still live before transfer into retained text.
    incoming_bytes: usize,
    composite: Option<bool>,
}
impl<'a> NativeDateArithmeticValues<'a> {
    /// Construct without coercing or inspecting suppressed operand payloads.
    pub fn new(
        inputs: [Input<'a>; 2],
        unit: &'a str,
        config: NativeDateArithmeticValueConfig,
        maximum: usize,
    ) -> Result<Self> {
        Ok(Self::from_values(
            inputs.map(ValueInput::from),
            unit,
            config,
            (maximum != usize::MAX).then_some(maximum),
        ))
    }
    pub(super) fn from_values(
        inputs: [ValueInput<'a>; 2],
        unit: &'a str,
        config: NativeDateArithmeticValueConfig,
        maximum: Option<usize>,
    ) -> Self {
        Self {
            inputs,
            unit,
            config,
            date_text: None,
            amount_text: None,
            number: None,
            prepared: 0,
            phase: 0,
            maximum,
            warning: None,
            incoming_bytes: 0,
            composite: None,
        }
    }
    /// Actual retained owned text, excluding borrowed original operand storage.
    pub fn retained_bytes(&self) -> Option<usize> {
        let bytes = |text: &Option<Cow<'_, str>>| match text {
            Some(Cow::Owned(v)) => v.capacity(),
            _ => 0,
        };
        bytes(&self.date_text)
            .checked_add(bytes(&self.amount_text))?
            .checked_add(self.warning.as_ref().map_or(0, |w| w.message.capacity()))
    }
    /// Finite producer allowance after the continuation's actual owned text.
    pub fn remaining_bytes(&self) -> Result<usize> {
        match self.maximum {
            Some(max) => max
                .checked_sub(
                    self.retained_bytes()
                        .ok_or(Error::ResourceLimit)?
                        .checked_add(self.incoming_bytes)
                        .ok_or(Error::ResourceLimit)?,
                )
                .ok_or(Error::ResourceLimit),
            None => Ok(usize::MAX),
        }
    }
    fn work(&self, bytes: usize) -> Result<()> {
        if self.maximum.is_some() && bytes > self.remaining_bytes()? {
            Err(Error::ResourceLimit)
        } else {
            Ok(())
        }
    }
    fn extent(&self, n: usize, m: usize, extra: usize) -> Result<()> {
        self.work(
            n.checked_mul(m)
                .and_then(|n| n.checked_add(extra))
                .ok_or(Error::ResourceLimit)?,
        )
    }
    fn math_work(&self, length: usize) -> Result<()> {
        let bytes = length
            .checked_mul(32)
            .and_then(|n| {
                self.unit
                    .len()
                    .checked_mul(4)
                    .and_then(|u| n.checked_add(u))
            })
            .and_then(|n| n.checked_add(4096))
            .ok_or(Error::ResourceLimit)?;
        self.work(bytes)
    }
    fn composite(&mut self) -> Result<bool> {
        if let Some(value) = self.composite {
            return Ok(value);
        }
        self.extent(self.unit.len(), 2, 0)?;
        let value = helpers::composite_spec(self.unit).is_some();
        self.composite = Some(value);
        Ok(value)
    }
    fn ready(&mut self, value: Value) -> Step {
        self.phase = 255;
        Step {
            warning: self.warning.take(),
            action: Action::Ready(value),
        }
    }
    fn request(&mut self, phase: u8, kind: Request, index: usize) -> Step {
        self.phase = phase;
        Step {
            warning: self.warning.take(),
            action: Action::Request { kind, index },
        }
    }
    fn warning_null(&mut self, message: String) -> Result<Step> {
        self.work(message.capacity())?;
        // This REPLACES an earlier warning from internal amount preparation.
        self.warning = Some(Warning {
            code: 1292,
            message,
        });
        Ok(self.ready(Value::Null))
    }
    fn numeric_warning(&mut self, number: i64) -> Result<Step> {
        self.work(256)?;
        self.warning_null(format!("Incorrect time value: '{number}'"))
    }
    fn visible_decimal(&self, value: NativeDecimalParseRef<'_>) -> Result<String> {
        let work = crate::native_string_coercion_work(Input::Decimal(value))
            .map_err(|_| Error::ResourceLimit)?;
        self.work(work)?;
        let text = Decimal::native_format_visible(
            value.negative,
            value.digits,
            value.scale,
            value.storage_scale,
        );
        self.work(text.capacity())?;
        Ok(text)
    }
    fn set_number(&mut self, value: i64, second: bool) {
        self.number = Some(value);
        self.prepared = if second { 2 } else { 1 };
        self.amount_text = None;
    }
    fn seconds(&mut self, text: &str) -> bool {
        match helpers::decimal_seconds_to_micros(text) {
            Some(value) => {
                self.set_number(value, true);
                true
            }
            None => false,
        }
    }
    fn prepared(&self) -> Result<helpers::PreparedAmount<'_>> {
        match self.prepared {
            1 => Ok(helpers::PreparedAmount::Whole(
                self.number.ok_or(Error::InvalidResponse)?,
            )),
            2 => Ok(helpers::PreparedAmount::SecondMicros(
                self.number.ok_or(Error::InvalidResponse)?,
            )),
            3 => Ok(helpers::PreparedAmount::Composite(
                self.amount_text.as_deref().ok_or(Error::InvalidResponse)?,
            )),
            _ => Err(Error::InvalidResponse),
        }
    }
    fn accept_date(&mut self, text: Cow<'a, str>) -> Result<Option<Step>> {
        if self.incoming_bytes == 0 {
            self.work(match &text {
                Cow::Owned(v) => v.capacity(),
                _ => 0,
            })?;
        }
        if !helpers::calendar_date_valid(&text) {
            self.extent(text.len(), 3, 256)?;
            let message = match self.inputs[0] {
                ValueInput::Int(n) => format!("Incorrect time value: '{n}'"),
                ValueInput::UInt(n) => format!("Incorrect time value: '{}'", n as i64),
                _ => format!("Incorrect datetime value: '{text}'"),
            };
            return self.warning_null(message).map(Some);
        }
        self.incoming_bytes = 0;
        self.date_text = Some(text);
        Ok(None)
    }
    fn prepare_date(&mut self) -> Result<Option<Step>> {
        let number = match self.inputs[0] {
            ValueInput::Null => return Ok(Some(self.ready(Value::Null))),
            ValueInput::Int(n) => n,
            ValueInput::UInt(n) => match i64::try_from(n) {
                Ok(n) => n,
                Err(_) => return self.numeric_warning(-1).map(Some),
            },
            ValueInput::Decimal(v) => rounded_decimal(v),
            ValueInput::Real(v) => v.trunc() as i64,
            _ => return Ok(Some(self.request(1, Request::CoerceString, 0))),
        };
        self.work(256)?;
        let parsed = match native_parse_time_from_num(
            number,
            TimeType::DateTime,
            0,
            true,
            false,
            true,
            &Utc,
        )
        .into_result()
        {
            Ok(v) => v,
            Err(_) => return self.numeric_warning(number).map(Some),
        };
        let mut text = String::new();
        Time::write_native_core_display(parsed.time.raw, number < 101_000_000, 0, &mut text)
            .expect("String formatting cannot fail");
        self.accept_date(Cow::Owned(text))
    }
    fn prepare_amount(&mut self, composite: bool) -> Result<Option<Step>> {
        let value = self.inputs[1];
        let config = self.config;
        if matches!(value, ValueInput::Null) {
            return Ok(Some(self.ready(Value::Null)));
        }
        if composite {
            let text = match value {
                ValueInput::Int(v) => {
                    self.work(128)?;
                    v.to_string()
                }
                ValueInput::UInt(v) => {
                    self.work(128)?;
                    v.to_string()
                }
                ValueInput::Decimal(v) => {
                    let visible = self.visible_decimal(v)?;
                    self.extent(visible.len(), 4, 256)?;
                    helpers::format_decimal_composite_text(self.unit, &visible)
                }
                ValueInput::Real(v) | ValueInput::Float32(v) if config.duration => {
                    let precision = config.decimal.filter(|p| *p >= 0);
                    // The formatter validates its u16 precision parameter even
                    // for nonfinite values. Keep its original panic when work
                    // is permitted, but refuse oversized work before entering it.
                    let work =
                        if v.is_finite() || precision.is_some_and(|p| p > i64::from(u16::MAX)) {
                            usize::try_from(precision.unwrap_or(0))
                                .ok()
                                .and_then(|n| n.checked_add(512))
                                .ok_or(Error::ResourceLimit)?
                        } else {
                            32
                        };
                    self.work(work)?;
                    match precision {
                        Some(p) => format!("{v:.precision$}", precision = p as usize),
                        None => v.to_string(),
                    }
                }
                ValueInput::Text => return Ok(Some(self.request(2, Request::CoerceString, 1))),
                _ if config.duration => return Ok(Some(self.request(2, Request::CoerceString, 1))),
                _ => {
                    return Ok(Some(
                        self.ready(Value::Unsupported("composite INTERVAL amount")),
                    ));
                }
            };
            self.work(text.capacity())?;
            self.amount_text = Some(Cow::Owned(text));
            self.prepared = 3;
            return Ok(None);
        }
        if self.unit.eq_ignore_ascii_case("SECOND") {
            let text = match value {
                ValueInput::Int(v) => {
                    self.work(128)?;
                    v.to_string()
                }
                ValueInput::UInt(v) => {
                    self.work(128)?;
                    v.to_string()
                }
                ValueInput::Decimal(v) => self.visible_decimal(v)?,
                ValueInput::Text => return Ok(Some(self.request(2, Request::CoerceString, 1))),
                ValueInput::Real(v) => {
                    self.work(512)?;
                    let text = v.to_string();
                    self.work(text.capacity())?;
                    self.amount_text = Some(Cow::Owned(text));
                    return Ok(Some(self.request(4, Request::ParseDecimal, 1)));
                }
                ValueInput::Sentinel => {
                    return Ok(Some(
                        self.ready(Value::Unsupported("range sentinel INTERVAL amount")),
                    ));
                }
                _ => return Ok(Some(self.request(3, Request::ToI64, 1))),
            };
            if !self.seconds(&text) {
                return Ok(Some(self.ready(Value::Null)));
            }
            return Ok(None);
        }
        let number = match value {
            ValueInput::Int(v) => v,
            ValueInput::UInt(v) => i64::try_from(v).unwrap_or(i64::MAX),
            ValueInput::Decimal(v) => rounded_decimal(v),
            ValueInput::Real(v) => v.round() as i64,
            ValueInput::Text => return Ok(Some(self.request(2, Request::CoerceString, 1))),
            ValueInput::Sentinel => {
                return Ok(Some(
                    self.ready(Value::Unsupported("range sentinel INTERVAL amount")),
                ));
            }
            _ => return Ok(Some(self.request(3, Request::ToI64, 1))),
        };
        self.set_number(number, false);
        Ok(None)
    }
    fn drive(&mut self) -> Result<Step> {
        loop {
            if self.config.duration {
                let nanos = match self.inputs[0] {
                    ValueInput::Null => return Ok(self.ready(Value::Null)),
                    ValueInput::Duration(v) => v,
                    _ => return Ok(self.ready(Value::Unsupported("DATE_ADD duration operand"))),
                };
                let composite = self.composite()?;
                if !composite
                    && !["MICROSECOND", "SECOND", "MINUTE", "HOUR"]
                        .iter()
                        .any(|v| self.unit.eq_ignore_ascii_case(v))
                {
                    return Ok(self.ready(Value::Null));
                }
                if self.prepared == 0 {
                    if let Some(step) = self.prepare_amount(composite)? {
                        return Ok(step);
                    }
                    continue;
                }
                self.math_work(self.amount_text.as_ref().map_or(0, |v| v.len()))?;
                return Ok(
                    match helpers::apply_duration(
                        self.unit,
                        nanos,
                        self.prepared()?,
                        self.config.sign,
                        self.config.duration_fsp,
                    ) {
                        Some((nanos, fsp)) => self.ready(Value::Duration { nanos, fsp }),
                        None => self.ready(Value::Null),
                    },
                );
            }
            if matches!(self.inputs, [ValueInput::Null, ValueInput::Null]) {
                return Ok(self.ready(Value::Null));
            }
            let composite = self.composite()?;
            if composite && self.prepared == 0 {
                if let Some(step) = self.prepare_amount(composite)? {
                    return Ok(step);
                }
                continue;
            }
            if self.date_text.is_none() {
                if let Some(step) = self.prepare_date()? {
                    return Ok(step);
                }
                continue;
            }
            if self.prepared == 0 {
                if let Some(step) = self.prepare_amount(composite)? {
                    return Ok(step);
                }
                continue;
            }
            let length = self
                .date_text
                .as_ref()
                .map_or(0, |v| v.len())
                .checked_add(self.amount_text.as_ref().map_or(0, |v| v.len()))
                .ok_or(Error::ResourceLimit)?;
            self.math_work(length)?;
            let outcome = helpers::apply_calendar(
                self.unit,
                self.date_text.as_deref().expect("prepared date"),
                self.prepared()?,
                self.config.sign,
                self.config.fsp,
            );
            return Ok(match outcome {
                helpers::CalendarOutcome::Text(text) => {
                    self.work(text.capacity())?;
                    self.ready(Value::Text(text))
                }
                helpers::CalendarOutcome::Null => self.ready(Value::Null),
                helpers::CalendarOutcome::UnsupportedUnit => {
                    self.ready(Value::Unsupported("INTERVAL unit"))
                }
                helpers::CalendarOutcome::Overflow => {
                    self.phase = 5;
                    Step {
                        warning: self.warning.take(),
                        action: Action::Overflow,
                    }
                }
            });
        }
    }
    /// Start once, without supplying a fabricated child value.
    fn begin_inner(&mut self) -> Result<Step> {
        if self.phase != 0 {
            return Err(Error::InvalidResponse);
        }
        self.drive()
    }
    /// Supply only the actually requested string coercion result.
    pub fn resume_string(&mut self, text: Option<String>) -> Result<Step> {
        self.resume_string_cow(text.map(Cow::Owned))
    }
    fn resume_string_inner(&mut self, text: Option<Cow<'a, str>>) -> Result<Step> {
        if !matches!(self.phase, 1 | 2) {
            return Err(Error::InvalidResponse);
        }
        let Some(text) = text else {
            return Ok(self.ready(Value::Null));
        };
        self.work(match &text {
            Cow::Owned(v) => v.capacity(),
            _ => 0,
        })?;
        self.incoming_bytes = match &text {
            Cow::Owned(v) => v.capacity(),
            _ => 0,
        };
        if self.phase == 1 {
            if let Some(step) = self.accept_date(text)? {
                return Ok(step);
            }
        } else {
            if self.composite()? {
                self.incoming_bytes = 0;
                self.amount_text = Some(text);
                self.prepared = 3;
            } else if self.unit.eq_ignore_ascii_case("SECOND") {
                self.incoming_bytes = 0;
                self.amount_text = Some(text);
                return Ok(self.request(4, Request::ParseDecimal, 1));
            } else {
                let (number, warn) = single_string(&text);
                if warn {
                    self.extent(text.len(), 3, 256)?;
                    self.warning = Some(Warning {
                        code: 1292,
                        message: format!("Truncated incorrect DECIMAL value: '{text}'"),
                    });
                }
                self.set_number(number, false);
                drop(text);
                self.incoming_bytes = 0;
            }
        }
        self.phase = 0;
        self.drive()
    }
    /// Supply only the original operand's requested ToInt64 result.
    fn resume_integer_inner(&mut self, number: i64) -> Result<Step> {
        if self.phase != 3 {
            return Err(Error::InvalidResponse);
        }
        if self.unit.eq_ignore_ascii_case("SECOND") {
            self.work(128)?;
            if !self.seconds(&number.to_string()) {
                return Ok(self.ready(Value::Null));
            }
        } else {
            self.set_number(number, false)
        }
        self.phase = 0;
        self.drive()
    }
    /// Borrow the exact current parse_mysql input only at its actual request.
    pub fn decimal_text(&self) -> Result<&str> {
        if self.phase != 4 {
            return Err(Error::InvalidResponse);
        }
        self.amount_text
            .as_deref()
            .map(str::trim)
            .ok_or(Error::InvalidResponse)
    }
    /// Supply the actual fixed-word parser coefficient, with its disposition
    /// already ignored according to the original policy.
    fn resume_decimal_inner(&mut self, value: NativeDecimalParseRef<'_>) -> Result<Step> {
        if self.phase != 4 {
            return Err(Error::InvalidResponse);
        }
        let number = {
            let text = self.visible_decimal(value)?;
            helpers::decimal_seconds_to_micros(&text)
        };
        match number {
            Some(n) => self.set_number(n, true),
            None => return Ok(self.ready(Value::Null)),
        }
        self.phase = 0;
        self.drive()
    }
    /// Supply the statement policy only after actual overflow; no replay.
    fn resume_overflow_inner(&mut self, mode: NativeDateArithmeticOverflowMode) -> Result<Step> {
        if self.phase != 5 {
            return Err(Error::InvalidResponse);
        }
        match mode {
            NativeDateArithmeticOverflowMode::Ignore => Ok(self.ready(Value::Null)),
            NativeDateArithmeticOverflowMode::Warn => {
                self.work(128)?;
                self.warning = Some(Warning {
                    code: 1441,
                    message: OVERFLOW.into(),
                });
                Ok(self.ready(Value::Null))
            }
            NativeDateArithmeticOverflowMode::Error => Ok(self.ready(Value::Error(OVERFLOW))),
        }
    }
    fn finish_attempt(&mut self, step: Result<Step>) -> Result<Step> {
        if step.is_err() {
            self.phase = 254;
        }
        step
    }
    /// Start once. Every failed advance permanently closes this continuation.
    pub fn begin(&mut self) -> Result<Step> {
        let step = self.begin_inner();
        self.finish_attempt(step)
    }
    pub(super) fn resume_string_cow(&mut self, text: Option<Cow<'a, str>>) -> Result<Step> {
        let step = self.resume_string_inner(text);
        self.incoming_bytes = 0;
        self.finish_attempt(step)
    }
    /// Supply the requested integer once; an earlier failed advance is
    /// terminal.
    pub fn resume_integer(&mut self, value: i64) -> Result<Step> {
        let step = self.resume_integer_inner(value);
        self.finish_attempt(step)
    }
    /// Supply the original parser result once, without reviving a failed step.
    pub fn resume_decimal(&mut self, value: NativeDecimalParseRef<'_>) -> Result<Step> {
        let step = self.resume_decimal_inner(value);
        self.finish_attempt(step)
    }
    /// Supply late overflow policy once, without masking any earlier failure.
    pub fn resume_overflow(&mut self, mode: NativeDateArithmeticOverflowMode) -> Result<Step> {
        let step = self.resume_overflow_inner(mode);
        self.finish_attempt(step)
    }
}
pub(super) fn rounded_decimal(value: NativeDecimalParseRef<'_>) -> i64 {
    let digits = std::str::from_utf8(value.digits).expect("decimal coefficients are ASCII digits");
    let split = digits.len() - value.storage_scale as usize;
    let integer = if split == 0 { "0" } else { &digits[..split] };
    let rounded = (|| {
        let mut n = integer.parse::<u64>().ok()?;
        if value.storage_scale != 0 && digits.as_bytes()[split] >= b'5' {
            n = n.checked_add(1)?
        }
        if value.negative && n == i64::MIN.unsigned_abs() {
            return Some(i64::MIN);
        }
        let n = i64::try_from(n).ok()?;
        Some(if value.negative { -n } else { n })
    })();
    rounded.unwrap_or(if value.negative { i64::MIN } else { i64::MAX })
}
fn single_string(text: &str) -> (i64, bool) {
    let trimmed = text.trim();
    let bytes = trimmed.as_bytes();
    let mut i = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1
    }
    let n = if i == start {
        0
    } else {
        trimmed[..i].parse::<i64>().unwrap_or(i64::MAX)
    };
    (n, i == start || i < bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_actual_date_step_cannot_be_reinterpreted_as_null_or_replayed() {
        let mut state = NativeDateArithmeticValues::new(
            [Input::String(b"2020-01-01"), Input::Int(1)],
            "DAY",
            NativeDateArithmeticValueConfig::calendar(1, None),
            8192,
        )
        .unwrap();
        assert!(matches!(
            state.begin().unwrap().action,
            Action::Request {
                kind: Request::CoerceString,
                index: 0
            }
        ));
        let mut spare = String::with_capacity(16384);
        spare.push_str("2020-01-01");
        assert!(matches!(
            state.resume_string(Some(spare)),
            Err(Error::ResourceLimit)
        ));
        assert!(matches!(
            state.resume_string(None),
            Err(Error::InvalidResponse)
        ));
        assert!(matches!(
            state.resume_string(Some("2020-01-01".into())),
            Err(Error::InvalidResponse)
        ));
    }
    #[test]
    fn date_arithmetic_actual_demand_scale_and_terminal_contracts() {
        let decimal = NativeDecimalParseRef {
            negative: false,
            digits: b"1",
            scale: u32::MAX,
            storage_scale: 0,
            declared_shape: None,
        };
        let mut suppressed = NativeDateArithmeticValues::new(
            [Input::Null, Input::Decimal(decimal)],
            "DAY",
            NativeDateArithmeticValueConfig::calendar(1, None),
            6,
        )
        .unwrap();
        assert!(matches!(
            suppressed.begin().unwrap().action,
            Action::Ready(Value::Null)
        ));
        assert!(matches!(suppressed.begin(), Err(Error::InvalidResponse)));
        let mut demanded = NativeDateArithmeticValues::new(
            [Input::Null, Input::Decimal(decimal)],
            "DAY_SECOND",
            NativeDateArithmeticValueConfig::calendar(1, None),
            8192,
        )
        .unwrap();
        assert!(matches!(demanded.begin(), Err(Error::ResourceLimit)));
        assert!(matches!(
            demanded.resume_integer(0),
            Err(Error::InvalidResponse)
        ));
        assert!(
            NativeDateArithmeticValues::new(
                [Input::Null, Input::Null],
                "DAY",
                NativeDateArithmeticValueConfig::calendar(1, None),
                usize::MAX
            )
            .is_ok()
        );
        let mut wrong = NativeDateArithmeticValues::new(
            [Input::String(b"2020-01-01"), Input::Int(1)],
            "DAY",
            NativeDateArithmeticValueConfig::calendar(1, None),
            8192,
        )
        .unwrap();
        assert!(matches!(
            wrong.begin().unwrap().action,
            Action::Request { index: 0, .. }
        ));
        assert!(matches!(
            wrong.resume_integer(1),
            Err(Error::InvalidResponse)
        ));
        assert!(matches!(
            wrong.resume_string(None),
            Err(Error::InvalidResponse)
        ));
        let mut unit = NativeDateArithmeticValues::new(
            [Input::Null, Input::Int(1)],
            "DAY_SECOND",
            NativeDateArithmeticValueConfig::calendar(1, None),
            0,
        )
        .unwrap();
        assert!(matches!(unit.begin(), Err(Error::ResourceLimit)));
    }
    #[test]
    fn date_arithmetic_nonfinite_keeps_formatter_parameter_panics_after_work_gate() {
        for p in [65536, i64::MAX] {
            let make = |max| {
                NativeDateArithmeticValues::new(
                    [
                        Input::Duration {
                            nanoseconds: 0,
                            fsp: 6,
                        },
                        Input::Real(f64::NAN),
                    ],
                    "HOUR_MINUTE",
                    NativeDateArithmeticValueConfig::duration(1, 6, Some(p)),
                    max,
                )
                .unwrap()
            };
            assert!(matches!(make(8192).begin(), Err(Error::ResourceLimit)));
            assert!(
                std::panic::catch_unwind(|| make((p as usize).checked_add(1024).unwrap()).begin())
                    .is_err()
            );
        }
    }
}
