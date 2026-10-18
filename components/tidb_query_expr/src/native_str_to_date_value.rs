// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Actual ordinary STR_TO_DATE parse states, independently compared before
//! retiring the native caller's report programs. Wire grammar remains separate.
use tidb_query_datatype::codec::{
    mysql::Time, native_sql_string::NativeSqlStringInput as Input,
    native_type_name::NativeTypeNameCode as Target,
};

use crate::native_str_to_date::{ParsedDateTime, clock_needs_prefix, render_clock, scan};
/// Actual terminal result, including the SDK's original warning text.
#[derive(Debug, PartialEq, Eq)]
pub enum NativeStrToDateValue {
    /// Actual source NULL or typed clock suppressed by NO_ZERO_DATE.
    Null,
    /// Computed date/clock text before the original caller's temporal
    /// conversion.
    Text(String),
    /// Original statement warning followed by SQL NULL.
    Warning { code: u16, message: String },
}
/// Actual string admission remains distinct from finite resource refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeStrToDateValueError {
    /// Original coerce_str error.
    Unsupported(&'static str),
    /// Demanded owned text/scanner/formatter exceeds its finite allowance.
    ResourceLimit(&'static str),
}
use NativeStrToDateValue as Value;
use NativeStrToDateValueError as Error;
fn resource() -> Error {
    Error::ResourceLimit("STR_TO_DATE actual text/work allowance exceeded")
}
#[derive(Clone, Copy, Debug)]
struct Allowance(Option<usize>);
impl From<usize> for Allowance {
    fn from(value: usize) -> Self {
        Self(Some(value))
    }
}
impl Allowance {
    fn remaining(self, owned: usize) -> Result<Self, Error> {
        self.0
            .map(|maximum| maximum.checked_sub(owned).ok_or_else(resource))
            .transpose()
            .map(Self)
    }
}
fn check(bytes: usize, maximum: impl Into<Allowance>) -> Result<(), Error> {
    if let Some(maximum) = maximum.into().0 {
        if maximum == usize::MAX || bytes > maximum {
            return Err(resource());
        }
    }
    Ok(())
}
fn owned(value: String, maximum: impl Into<Allowance>) -> Result<String, Error> {
    check(value.capacity(), maximum)?;
    Ok(value)
}
fn checked_extent(n: usize, m: usize, extra: usize) -> Result<usize, Error> {
    n.checked_mul(m)
        .and_then(|n| n.checked_add(extra))
        .ok_or_else(resource)
}
fn coerce(input: Input<'_>, maximum: usize) -> Result<String, Error> {
    // Preserve byte-kind UTF-8 failures BEFORE copying and later format work.
    let bytes = match input {
        Input::String(v) => Some((v, "invalid UTF-8 string datum")),
        Input::Bytes(v) => Some((v, "invalid UTF-8 byte datum")),
        Input::BinaryLiteral(v) | Input::Bit(v) => Some((v, "invalid UTF-8 binary literal")),
        Input::Enum(v) => Some((v, "invalid UTF-8 ENUM name")),
        Input::Set(v) => Some((v, "invalid UTF-8 SET name")),
        Input::Raw(v) => Some((v, "invalid UTF-8 raw datum")),
        _ => None,
    };
    let work = if let Some((bytes, error)) = bytes {
        std::str::from_utf8(bytes).map_err(|_| Error::Unsupported(error))?;
        bytes.len()
    } else {
        match input {
            Input::MinNotNull | Input::MaxValue => {
                return Err(Error::Unsupported("range sentinel string coercion"));
            }
            Input::Null => 0,
            Input::Int(_) | Input::UInt(_) => 128,
            Input::Real(_) | Input::Float32(_) => 4096,
            Input::Decimal(v) => checked_extent(
                v.digits
                    .len()
                    .checked_add(v.scale.max(v.storage_scale) as usize)
                    .ok_or_else(resource)?,
                16,
                1024,
            )?,
            Input::Json { value, .. } => checked_extent(value.len(), 128, 1024)?,
            Input::VectorFloat32(v) => checked_extent(v.len(), 512, 1024)?,
            Input::Duration { fsp, .. } => checked_extent(
                usize::try_from(fsp.max(64)).map_err(|_| resource())?,
                16,
                1024,
            )?,
            Input::Time(_) => 1024,
            _ => unreachable!("byte inputs handled above"),
        }
    };
    check(work, maximum)?;
    let text = crate::native_coerce_string(input)
        .map_err(Error::Unsupported)?
        .unwrap_or_default();
    owned(text, maximum)
}
/// Owned actual coerce_str results. No input-store borrow survives this value.
#[derive(Debug)]
pub struct NativeStrToDatePrepared {
    input: String,
    format: String,
}
/// Observe both actual source values before NULL suppression; then coerce input
/// before format. No later format or Decimal work runs after an earlier error.
pub fn native_prepare_str_to_date(
    input: Input<'_>,
    format: Input<'_>,
    maximum: usize,
) -> Result<Option<NativeStrToDatePrepared>, Error> {
    if matches!(input, Input::Null) || matches!(format, Input::Null) {
        return Ok(None);
    }
    let input = coerce(input, maximum)?;
    let remaining = maximum.checked_sub(input.capacity()).ok_or_else(resource)?;
    let format = coerce(format, remaining)?;
    Ok(Some(NativeStrToDatePrepared { input, format }))
}
/// Actual parser continuation, not an encoded header or precomputed host flag.
#[derive(Debug)]
pub enum NativeStrToDateStep<'a> {
    /// Actual terminal result needs no date_modes getter.
    Ready(Value),
    /// Actual parsed calendar fields need the statement's date modes now.
    Date(NativeStrToDateDate<'a>),
    /// Actual typed clock needs the later zero-date-prefix decision.
    TypedClock(NativeStrToDateClock),
}
/// Opaque actual parsed calendar state; only the scanner can construct it.
#[derive(Debug)]
pub struct NativeStrToDateDate<'a> {
    value: ParsedDateTime,
    input: &'a str,
    maximum: Allowance,
}
/// Opaque actual clock text, produced by the scanner.
#[derive(Debug)]
pub struct NativeStrToDateClock {
    text: String,
    maximum: Allowance,
}
fn warning(month_zero: bool, input: &str, maximum: Allowance) -> Result<Value, Error> {
    let (code, prefix, text, suffix) = if month_zero {
        (
            1411,
            "Incorrect datetime value: '",
            input,
            "' for function str_to_date",
        )
    } else {
        (
            1292,
            "",
            "Incorrect datetime value: '0000-00-00 00:00:00'",
            "",
        )
    };
    warning_text(code, prefix, text, suffix, maximum)
}
fn warning_text(
    code: u16,
    prefix: &str,
    text: &str,
    suffix: &str,
    maximum: Allowance,
) -> Result<Value, Error> {
    let length = prefix
        .len()
        .checked_add(text.len())
        .and_then(|n| n.checked_add(suffix.len()))
        .ok_or_else(resource)?;
    check(length, maximum)?;
    let mut message = String::new();
    message.try_reserve_exact(length).map_err(|_| resource())?;
    check(message.capacity(), maximum)?;
    message.push_str(prefix);
    message.push_str(text);
    message.push_str(suffix);
    Ok(Value::Warning { code, message })
}
impl NativeStrToDatePrepared {
    /// Actual retained string capacities, independent of retired report
    /// framing.
    pub fn retained_bytes(&self) -> Option<usize> {
        self.input.capacity().checked_add(self.format.capacity())
    }
    /// Scan actual text under the native value owner's finite allowance.
    pub fn begin(
        &self,
        target: Option<Target>,
        maximum: usize,
    ) -> Result<NativeStrToDateStep<'_>, Error> {
        let remaining = maximum
            .checked_sub(self.retained_bytes().ok_or_else(resource)?)
            .ok_or_else(resource)?;
        begin_actual(&self.input, &self.format, target, Some(remaining))
    }
}
// None belongs ONLY to the original ordinary wire owner, retaining its own
// transport bounds. Native public producers always supply Some(finite).
pub(crate) fn begin_actual<'a>(
    input: &'a str,
    format: &str,
    target: Option<Target>,
    maximum: Option<usize>,
) -> Result<NativeStrToDateStep<'a>, Error> {
    let maximum = Allowance(maximum);
    let extent = input.len().checked_add(format.len()).ok_or_else(resource)?;
    check(checked_extent(extent, 16, 0)?, maximum)?;
    let value = match scan(input, format) {
        Ok(value) => value,
        Err(month_zero) => {
            return warning(month_zero, input, maximum).map(NativeStrToDateStep::Ready);
        }
    };
    if value.saw_date {
        return Ok(NativeStrToDateStep::Date(NativeStrToDateDate {
            value,
            input,
            maximum,
        }));
    }
    if value.saw_time {
        check(256, maximum)?;
        let text = owned(render_clock(&value), maximum)?;
        return if target == Some(Target::Known(12)) && clock_needs_prefix(&text) {
            Ok(NativeStrToDateStep::TypedClock(NativeStrToDateClock {
                text,
                maximum,
            }))
        } else {
            Ok(NativeStrToDateStep::Ready(Value::Text(text)))
        };
    }
    warning(false, input, maximum).map(NativeStrToDateStep::Ready)
}
impl NativeStrToDateDate<'_> {
    /// Consume actual date modes only after the scanner demands them.
    pub fn finish(self, no_zero_date: bool, allow_invalid_dates: bool) -> Result<Value, Error> {
        finish_date(
            &self.value,
            self.input,
            no_zero_date,
            allow_invalid_dates,
            self.maximum.0,
        )
    }
    pub(crate) fn wire_parts(&self) -> (&ParsedDateTime, &str) {
        (&self.value, self.input)
    }
}
pub(crate) fn finish_date(
    v: &ParsedDateTime,
    input: &str,
    no_zero_date: bool,
    allow_invalid_dates: bool,
    maximum: Option<usize>,
) -> Result<Value, Error> {
    let maximum = Allowance(maximum);
    if v.month == 0 {
        return warning(true, input, maximum);
    }
    let max_day = if allow_invalid_dates || v.month == 0 {
        31
    } else {
        Time::native_days_in_month(v.year, v.month)
    };
    if v.month > 12 || v.day > max_day {
        if v.month == 0 {
            return warning(true, input, maximum);
        }
        check(512, maximum)?;
        let rendered = format!(
            "{:04}-{:02}-{:02} {}",
            v.year,
            v.month,
            v.day,
            render_clock(v)
        );
        check(rendered.capacity(), maximum)?;
        return warning_text(
            1292,
            "Incorrect datetime value: '",
            &rendered,
            "'",
            maximum.remaining(rendered.capacity())?,
        );
    }
    if no_zero_date && (v.year == 0 || v.month == 0 || v.day == 0) {
        return warning(v.saw_date && v.month == 0, input, maximum);
    }
    if v.year == 0 && v.month == 0 && v.day == 0 {
        return warning(true, input, maximum);
    }
    check(512, maximum)?;
    let date = format!("{:04}-{:02}-{:02}", v.year, v.month, v.day);
    let text = if v.saw_time || v.saw_fraction {
        format!("{date} {}", render_clock(v))
    } else {
        date
    };
    owned(text, maximum).map(Value::Text)
}
impl NativeStrToDateClock {
    /// Consume the original later NO_ZERO_DATE decision.
    pub fn finish(self, no_zero_date: bool) -> Result<Value, Error> {
        if no_zero_date {
            return Ok(Value::Null);
        }
        finish_clock(
            &self.text,
            false,
            self.maximum.remaining(self.text.capacity())?.0,
        )
    }
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}
pub(crate) fn finish_clock(
    text: &str,
    no_zero_date: bool,
    maximum: Option<usize>,
) -> Result<Value, Error> {
    if no_zero_date {
        return Ok(Value::Null);
    }
    let maximum = Allowance(maximum);
    check(text.len().checked_add(64).ok_or_else(resource)?, maximum)?;
    owned(format!("0000-00-00 {text}"), maximum).map(Value::Text)
}
/// Only an actual source NULL prepass selects this primitive.
pub const fn native_str_to_date_null_value() -> Value {
    Value::Null
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_parse_states_keep_late_modes_and_named_target_identity() {
        let prepared = native_prepare_str_to_date(
            Input::String(b"2020-02-29"),
            Input::String(b"%Y-%m-%d"),
            8192,
        )
        .unwrap()
        .unwrap();
        let NativeStrToDateStep::Date(date) = prepared.begin(None, 8192).unwrap() else {
            panic!("actual calendar demand")
        };
        assert_eq!(
            date.finish(false, false).unwrap(),
            Value::Text("2020-02-29".into())
        );
        let prepared = native_prepare_str_to_date(
            Input::String(b"12:34:56"),
            Input::String(b"%H:%i:%s"),
            8192,
        )
        .unwrap()
        .unwrap();
        let NativeStrToDateStep::TypedClock(clock) =
            prepared.begin(Some(Target::Known(12)), 8192).unwrap()
        else {
            panic!("typed clock demand")
        };
        assert_eq!(clock.finish(true).unwrap(), Value::Null);
        let NativeStrToDateStep::TypedClock(clock) =
            prepared.begin(Some(Target::Known(12)), 8192).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            clock.finish(false).unwrap(),
            Value::Text("0000-00-00 12:34:56".into())
        );
        let NativeStrToDateStep::Ready(value) =
            prepared.begin(Some(Target::Unknown(12)), 8192).unwrap()
        else {
            panic!("unknown is not named Datetime")
        };
        assert_eq!(value, Value::Text("12:34:56".into()));
    }
    #[test]
    fn actual_coercion_and_scanner_work_is_finite_and_skips_later_operands() {
        use tidb_query_datatype::codec::mysql::NativeDecimalParseRef;
        let huge = Input::Decimal(NativeDecimalParseRef {
            negative: false,
            digits: b"1",
            scale: u32::MAX,
            storage_scale: 0,
            declared_shape: None,
        });
        assert!(
            native_prepare_str_to_date(Input::Null, huge, 0)
                .unwrap()
                .is_none()
        );
        assert!(
            native_prepare_str_to_date(huge, Input::Null, 0)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            native_prepare_str_to_date(Input::Bytes(&[255]), huge, 0),
            Err(Error::Unsupported("invalid UTF-8 byte datum"))
        ));
        assert!(matches!(
            native_prepare_str_to_date(Input::MinNotNull, huge, 0),
            Err(Error::Unsupported("range sentinel string coercion"))
        ));
        assert!(matches!(
            native_prepare_str_to_date(huge, Input::String(b"%Y"), 8192),
            Err(Error::ResourceLimit(_))
        ));
        let long = vec![b'1'; 16384];
        assert!(matches!(
            native_prepare_str_to_date(Input::String(&long), Input::String(b"%Y"), 8192),
            Err(Error::ResourceLimit(_))
        ));
        let prepared = native_prepare_str_to_date(
            Input::String(b"2020-02-29"),
            Input::String(b"%Y-%m-%d"),
            8192,
        )
        .unwrap()
        .unwrap();
        assert!(matches!(
            prepared.begin(None, 64),
            Err(Error::ResourceLimit(_))
        ));
        let warning = "Incorrect datetime value: '0000-00-00 00:00:00'";
        let prepared =
            native_prepare_str_to_date(Input::String(b""), Input::String(b""), warning.len())
                .unwrap()
                .unwrap();
        let NativeStrToDateStep::Ready(value) = prepared.begin(None, warning.len()).unwrap() else {
            panic!()
        };
        assert_eq!(
            value,
            Value::Warning {
                code: 1292,
                message: warning.into()
            }
        );
    }
}
