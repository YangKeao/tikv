// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Ordinary DATE_ADD/SUB controller. Calendar and typed-Duration entries keep
//! their original, different demand order; legacy date arithmetic is separate.
use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::codec::data_type::{
    Bytes, BytesRef, ChunkedVec, EvaluableRef, EvaluableRet, Int,
};

pub use crate::NativeIntervalEvalType as NativeDateArithmeticEvalType;
use crate::{
    NativeIdentityFrameError as Error, NativeIdentityRef as Identity, decode_native_identity,
    native_date_arithmetic_helpers as helpers, native_date_arithmetic_value::OVERFLOW,
};
type FrameResult<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeDateArithmeticFieldType {
    pub eval_type: NativeDateArithmeticEvalType,
    pub decimal: i64,
}
pub fn native_date_arithmetic_result_fsp(
    unit: &str,
    date: Option<NativeDateArithmeticFieldType>,
    amount: Option<NativeDateArithmeticFieldType>,
) -> Option<u32> {
    use NativeDateArithmeticEvalType as E;
    let date = date?;
    if !matches!(date.eval_type, E::Datetime | E::Timestamp | E::Duration) {
        return None;
    }
    if matches!(
        unit.to_ascii_uppercase().as_str(),
        "MICROSECOND"
            | "SECOND_MICROSECOND"
            | "MINUTE_MICROSECOND"
            | "HOUR_MICROSECOND"
            | "DAY_MICROSECOND"
    ) {
        return Some(6);
    }
    let extra = if unit.eq_ignore_ascii_case("SECOND") {
        match amount {
            Some(t) if matches!(t.eval_type, E::String | E::Real | E::Json) => 6,
            Some(t) if t.eval_type == E::Decimal => t.decimal.clamp(0, 6) as u32,
            _ => 0,
        }
    } else {
        0
    };
    Some((date.decimal.clamp(0, 6) as u32).max(extra))
}
pub fn native_format_decimal_composite_interval(unit: &str, visible_decimal: &str) -> String {
    helpers::format_decimal_composite_text(unit, visible_decimal)
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDateArithmeticRequest {
    CoerceString,
    ToI64,
    ParseDecimal,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeDateArithmeticWarning<'a> {
    pub code: u16,
    pub message: &'a str,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDateArithmeticOutcome<'a> {
    Null,
    Text(&'a str),
    Duration {
        nanos: i64,
        fsp: i64,
    },
    Unsupported(&'a str),
    Error {
        code: u16,
        message: &'a str,
    },
    Request {
        kind: NativeDateArithmeticRequest,
        index: usize,
        text: Option<&'a str>,
        state: &'a [u8],
    },
    Overflow {
        state: &'a [u8],
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeDateArithmeticResult<'a> {
    pub warning: Option<NativeDateArithmeticWarning<'a>>,
    pub outcome: NativeDateArithmeticOutcome<'a>,
}

struct Reader<'a> {
    bytes: &'a [u8],
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (a, b) = self.bytes.split_at_checked(n)?;
        self.bytes = b;
        Some(a)
    }
    fn byte(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn word(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn int(&mut self) -> Option<i64> {
        Some(self.word()? as i64)
    }
    fn blob(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.word()?).ok()?;
        self.take(n)
    }
    fn text(&mut self) -> Option<&'a str> {
        std::str::from_utf8(self.blob()?).ok()
    }
    fn optional(&mut self) -> Option<Option<&'a [u8]>> {
        Some(match self.byte()? {
            0 => None,
            1 => Some(self.blob()?),
            _ => return None,
        })
    }
}
fn put(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn blob(out: &mut Vec<u8>, v: &[u8]) -> FrameResult<()> {
    out.extend_from_slice(
        &u64::try_from(v.len())
            .map_err(|_| Error::Capacity)?
            .to_le_bytes(),
    );
    out.extend_from_slice(v);
    Ok(())
}
fn optional(out: &mut Vec<u8>, v: Option<&[u8]>) -> FrameResult<()> {
    out.push(u8::from(v.is_some()));
    if let Some(v) = v {
        blob(out, v)?;
    }
    Ok(())
}
fn fresh(bytes: &[u8], limit: usize) -> FrameResult<Vec<u8>> {
    if bytes.len() > limit {
        return Err(Error::Capacity);
    }
    let mut out = Vec::new();
    out.try_reserve_exact(bytes.len())
        .map_err(|_| Error::Capacity)?;
    if out.capacity() > limit {
        return Err(Error::Capacity);
    }
    out.extend_from_slice(bytes);
    Ok(out)
}
#[derive(Clone, Copy)]
struct Metadata {
    duration: bool,
    sign: i64,
    fsp: Option<u32>,
    duration_fsp: i64,
    decimal: Option<i64>,
}
pub fn encode_native_date_arithmetic_metadata(
    sign: i64,
    result_fsp: Option<u32>,
) -> FrameResult<Vec<u8>> {
    let mut out = vec![0];
    put(&mut out, sign);
    out.push(u8::from(result_fsp.is_some()));
    if let Some(fsp) = result_fsp {
        out.extend_from_slice(&fsp.to_le_bytes());
    }
    fresh(&out, 32)
}
pub fn encode_native_date_arithmetic_duration_metadata(
    sign: i64,
    result_fsp: i64,
    amount_decimal: Option<i64>,
) -> FrameResult<Vec<u8>> {
    let mut out = vec![1];
    put(&mut out, sign);
    put(&mut out, result_fsp);
    out.push(u8::from(amount_decimal.is_some()));
    if let Some(decimal) = amount_decimal {
        put(&mut out, decimal);
    }
    fresh(&out, 32)
}
fn metadata(bytes: &[u8]) -> Option<Metadata> {
    let mut r = Reader { bytes };
    let mode = r.byte()?;
    let sign = r.int()?;
    let m = match mode {
        0 => {
            let fsp = match r.byte()? {
                0 => None,
                1 => Some(u32::from_le_bytes(r.take(4)?.try_into().ok()?)),
                _ => return None,
            };
            Metadata {
                duration: false,
                sign,
                fsp,
                duration_fsp: 0,
                decimal: None,
            }
        }
        1 => {
            let duration_fsp = r.int()?;
            let decimal = match r.byte()? {
                0 => None,
                1 => Some(r.int()?),
                _ => return None,
            };
            Metadata {
                duration: true,
                sign,
                fsp: None,
                duration_fsp,
                decimal,
            }
        }
        _ => return None,
    };
    r.bytes.is_empty().then_some(m)
}
#[derive(Clone, Copy)]
struct State<'a> {
    date: Option<&'a [u8]>,
    amount: Option<&'a [u8]>,
    unit: &'a str,
    metadata: &'a [u8],
    phase: u8,
    date_text: Option<&'a str>,
    amount_text: Option<&'a str>,
    number: Option<i64>,
    prepared: u8,
}
// Phase 0 is internal drive; reports expose 1=date string, 2=amount string,
// 3=amount to_i64, 4=parse_mysql, 5=overflow policy. Prepared 1=whole,
// 2=second micros, 3=composite; no numeric result is supplied by native code.
fn read_state(bytes: &[u8]) -> Option<State<'_>> {
    let mut r = Reader { bytes };
    if r.byte()? != 1 {
        return None;
    }
    let phase = r.byte()?;
    if !(1..=5).contains(&phase) {
        return None;
    }
    let date = r.optional()?;
    let amount = r.optional()?;
    if date.is_some_and(|v| decode_native_identity(v).is_err())
        || amount.is_some_and(|v| decode_native_identity(v).is_err())
    {
        return None;
    }
    let unit = r.text()?;
    let raw = r.blob()?;
    metadata(raw)?;
    let date_text = r.optional()?.map(std::str::from_utf8).transpose().ok()?;
    let amount_text = r.optional()?.map(std::str::from_utf8).transpose().ok()?;
    let number = match r.byte()? {
        0 => None,
        1 => Some(r.int()?),
        _ => return None,
    };
    let prepared = r.byte()?;
    if prepared > 3
        || (matches!(prepared, 1 | 2) != number.is_some())
        || prepared == 3 && amount_text.is_none()
        || phase == 4 && amount_text.is_none()
        || !r.bytes.is_empty()
    {
        return None;
    }
    if matches!(phase, 1 | 2 | 3 | 4)
        && ((phase == 1 && date.is_none()) || (phase != 1 && amount.is_none()))
    {
        return None;
    }
    let m = metadata(raw)?;
    if matches!(phase, 2 | 3 | 4) && prepared != 0
        || phase == 1 && (m.duration || date_text.is_some())
        || phase == 4 && !unit.eq_ignore_ascii_case("SECOND")
        || phase == 5 && (m.duration || date_text.is_none() || prepared == 0)
    {
        return None;
    }
    Some(State {
        date,
        amount,
        unit,
        metadata: raw,
        phase,
        date_text,
        amount_text,
        number,
        prepared,
    })
}
fn state_bytes(s: State<'_>) -> FrameResult<Vec<u8>> {
    let mut out = vec![1, s.phase];
    optional(&mut out, s.date)?;
    optional(&mut out, s.amount)?;
    blob(&mut out, s.unit.as_bytes())?;
    blob(&mut out, s.metadata)?;
    optional(&mut out, s.date_text.map(str::as_bytes))?;
    optional(&mut out, s.amount_text.map(str::as_bytes))?;
    out.push(u8::from(s.number.is_some()));
    if let Some(n) = s.number {
        put(&mut out, n);
    }
    out.push(s.prepared);
    Ok(out)
}
fn envelope(bytes: &[u8]) -> Option<(u8, Option<NativeDateArithmeticWarning<'_>>, &[u8])> {
    let mut r = Reader { bytes };
    let tag = r.byte()?;
    let warning = match r.byte()? {
        0 => None,
        1 => {
            let code = u16::from_le_bytes(r.take(2)?.try_into().ok()?);
            if !matches!(code, 1292 | 1441) {
                return None;
            }
            Some(NativeDateArithmeticWarning {
                code,
                message: r.text()?,
            })
        }
        _ => return None,
    };
    Some((tag, warning, r.bytes))
}
fn pending(bytes: &[u8]) -> Option<State<'_>> {
    let (tag, _, body) = envelope(bytes)?;
    let s = read_state(body)?;
    ((tag == 5 && s.phase != 5) || (tag == 6 && s.phase == 5)).then_some(s)
}
pub fn decode_native_date_arithmetic_result(
    bytes: &[u8],
) -> Option<NativeDateArithmeticResult<'_>> {
    use NativeDateArithmeticOutcome as O;
    use NativeDateArithmeticRequest as Q;
    let (tag, warning, body) = envelope(bytes)?;
    let outcome = match tag {
        0 if body.is_empty() => O::Null,
        1 => O::Text(std::str::from_utf8(body).ok()?),
        2 if body.len() == 16 => O::Duration {
            nanos: i64::from_le_bytes(body[..8].try_into().ok()?),
            fsp: i64::from_le_bytes(body[8..].try_into().ok()?),
        },
        3 => {
            let text = std::str::from_utf8(body).ok()?;
            if !matches!(
                text,
                "DATE_ADD duration operand"
                    | "composite INTERVAL amount"
                    | "range sentinel INTERVAL amount"
                    | "INTERVAL unit"
            ) {
                return None;
            }
            O::Unsupported(text)
        }
        4 if body.len() >= 2 => {
            let code = u16::from_le_bytes(body[..2].try_into().ok()?);
            let message = std::str::from_utf8(&body[2..]).ok()?;
            if code != 1441 || message != OVERFLOW {
                return None;
            }
            O::Error { code, message }
        }
        5 => {
            let s = pending(bytes)?;
            let (kind, index, text) = match s.phase {
                1 => (Q::CoerceString, 0, None),
                2 => (Q::CoerceString, 1, None),
                3 => (Q::ToI64, 1, None),
                4 => (Q::ParseDecimal, 1, s.amount_text),
                _ => return None,
            };
            O::Request {
                kind,
                index,
                text,
                state: bytes,
            }
        }
        6 => {
            pending(bytes)?;
            O::Overflow { state: bytes }
        }
        _ => return None,
    };
    Some(NativeDateArithmeticResult { warning, outcome })
}
fn report(
    tag: u8,
    payload: &[u8],
    warning: Option<NativeDateArithmeticWarning<'_>>,
    limit: usize,
) -> FrameResult<Option<Vec<u8>>> {
    let mut out = vec![tag, u8::from(warning.is_some())];
    if let Some(w) = warning {
        out.extend_from_slice(&w.code.to_le_bytes());
        blob(&mut out, w.message.as_bytes())?;
    }
    out.extend_from_slice(payload);
    fresh(&out, limit).map(Some)
}
fn request(
    s: State<'_>,
    warning: Option<NativeDateArithmeticWarning<'_>>,
    limit: usize,
) -> FrameResult<Option<Vec<u8>>> {
    report(
        if s.phase == 5 { 6 } else { 5 },
        &state_bytes(s)?,
        warning,
        limit,
    )
}
fn null(w: Option<NativeDateArithmeticWarning<'_>>, limit: usize) -> FrameResult<Option<Vec<u8>>> {
    report(0, &[], w, limit)
}
fn unsupported(
    text: &str,
    w: Option<NativeDateArithmeticWarning<'_>>,
    limit: usize,
) -> FrameResult<Option<Vec<u8>>> {
    report(3, text.as_bytes(), w, limit)
}
fn fixed_width(s: State<'_>) -> Option<usize> {
    let m = metadata(s.metadata)?;
    if !m.duration || helpers::composite_spec(s.unit).is_none() {
        return Some(0);
    }
    if !matches!(
        s.date.and_then(|v| decode_native_identity(v).ok()),
        Some(Identity::Duration { .. })
    ) {
        return Some(0);
    }
    let Some(Identity::Real(bits) | Identity::Float32(bits)) =
        s.amount.and_then(|v| decode_native_identity(v).ok())
    else {
        return Some(0);
    };
    // Rust fixed formatting ignores fractional precision for NaN/infinity.
    if !f64::from_bits(bits).is_finite() {
        return Some(0);
    }
    m.decimal
        .filter(|p| *p >= 0)
        .map_or(Some(0), |p| usize::try_from(p).ok())
}
/// Retained reply bound, also precharging actual fixed-format precision before
/// producer execution. State can retain both the original identity and its
/// SDK-formatted composite text. Temporary decoder/math peaks are not claimed.
pub(crate) fn native_date_arithmetic_output_bound(
    a: Option<&[u8]>,
    b: Option<&[u8]>,
    c: Option<&[u8]>,
    d: Option<&[u8]>,
) -> Option<usize> {
    let sum = [a, b, c, d]
        .iter()
        .try_fold(0_usize, |n, v| n.checked_add(v.map_or(0, <[u8]>::len)))?;
    let width = if let Some(s) = a.and_then(pending) {
        fixed_width(s)?
    } else if let (Some(unit), Some(meta)) = (c, d) {
        fixed_width(State {
            date: a,
            amount: b,
            unit: std::str::from_utf8(unit).ok()?,
            metadata: meta,
            phase: 0,
            date_text: None,
            amount_text: None,
            number: None,
            prepared: 0,
        })?
    } else {
        0
    };
    sum.checked_mul(2)?.checked_add(width)?.checked_add(1024)
}
fn bound(
    a: Option<&[u8]>,
    b: Option<&[u8]>,
    c: Option<&[u8]>,
    d: Option<&[u8]>,
) -> FrameResult<usize> {
    native_date_arithmetic_output_bound(a, b, c, d).ok_or(Error::Capacity)
}
fn head_valid(
    a: Option<&[u8]>,
    b: Option<&[u8]>,
    c: Option<&[u8]>,
    d: Option<&[u8]>,
    duration: bool,
) -> bool {
    a.is_none_or(|v| decode_native_identity(v).is_ok())
        && b.is_none_or(|v| decode_native_identity(v).is_ok())
        && c.is_some_and(|v| std::str::from_utf8(v).is_ok())
        && d.and_then(metadata).is_some_and(|m| m.duration == duration)
        && native_date_arithmetic_output_bound(a, b, c, d).is_some()
}
pub fn date_arithmetic_head_native_args_valid(
    a: Option<&[u8]>,
    b: Option<&[u8]>,
    c: Option<&[u8]>,
    d: Option<&[u8]>,
) -> bool {
    head_valid(a, b, c, d, false)
}
pub fn date_arithmetic_duration_head_native_args_valid(
    a: Option<&[u8]>,
    b: Option<&[u8]>,
    c: Option<&[u8]>,
    d: Option<&[u8]>,
) -> bool {
    head_valid(a, b, c, d, true)
}
pub fn date_arithmetic_step_native_args_valid(a: Option<&[u8]>, b: Option<&[u8]>) -> bool {
    let Some(s) = a.and_then(pending) else {
        return false;
    };
    let valid = match s.phase {
        1 | 2 => b.is_none_or(|v| std::str::from_utf8(v).is_ok()),
        3 => matches!(
            b.and_then(|v| decode_native_identity(v).ok()),
            Some(Identity::Int(_))
        ),
        4 => matches!(
            b.and_then(|v| decode_native_identity(v).ok()),
            Some(Identity::Decimal { .. })
        ),
        _ => false,
    };
    valid && native_date_arithmetic_output_bound(a, b, None, None).is_some()
}
pub fn date_arithmetic_overflow_native_args_valid(a: Option<&[u8]>, level: Option<i64>) -> bool {
    a.and_then(pending).is_some_and(|s| s.phase == 5)
        && matches!(level, Some(0..=2))
        && native_date_arithmetic_output_bound(a, None, None, None).is_some()
}
fn value_error(error: crate::NativeDateArithmeticValueError) -> Error {
    match error {
        crate::NativeDateArithmeticValueError::InvalidResponse => Error::Invalid,
        crate::NativeDateArithmeticValueError::ResourceLimit => Error::Capacity,
    }
}
fn decimal_value(
    value: Identity<'_>,
) -> Option<tidb_query_datatype::codec::mysql::NativeDecimalParseRef<'_>> {
    let Identity::Decimal {
        negative,
        scale,
        storage_scale,
        declared_shape,
        coefficient,
    } = value
    else {
        return None;
    };
    Some(tidb_query_datatype::codec::mysql::NativeDecimalParseRef {
        negative,
        digits: coefficient,
        scale,
        storage_scale,
        declared_shape,
    })
}
#[cfg(test)]
fn rounded_decimal(value: Identity<'_>) -> i64 {
    crate::native_date_arithmetic_value::rounded_decimal(
        decimal_value(value).expect("decimal identity"),
    )
}
fn value_input(
    bytes: Option<&[u8]>,
) -> FrameResult<crate::native_date_arithmetic_value::ValueInput<'_>> {
    use crate::native_date_arithmetic_value::ValueInput as I;
    let Some(bytes) = bytes else {
        return Ok(I::Null);
    };
    Ok(match decode_native_identity(bytes)? {
        Identity::Int(v) => I::Int(v),
        Identity::UInt(v) => I::UInt(v),
        value @ Identity::Decimal { .. } => I::Decimal(decimal_value(value).ok_or(Error::Invalid)?),
        Identity::Real(bits) => I::Real(f64::from_bits(bits)),
        Identity::Float32(bits) => I::Float32(f64::from_bits(bits)),
        Identity::String { .. } | Identity::Bytes(_) => I::Text,
        Identity::Duration { nanos, .. } => I::Duration(nanos),
        Identity::MinNotNull | Identity::MaxValue => I::Sentinel,
        _ => I::Other,
    })
}
fn value_state(s: State<'_>) -> FrameResult<crate::NativeDateArithmeticValues<'_>> {
    let metadata = metadata(s.metadata).ok_or(Error::Invalid)?;
    let config = if metadata.duration {
        crate::NativeDateArithmeticValueConfig::duration(
            metadata.sign,
            metadata.duration_fsp,
            metadata.decimal,
        )
    } else {
        crate::NativeDateArithmeticValueConfig::calendar(metadata.sign, metadata.fsp)
    };
    // The wire owner retains its original independent transport/work bound.
    // Native callers can construct only a finite actual-value allowance.
    let mut state = crate::NativeDateArithmeticValues::from_values(
        [value_input(s.date)?, value_input(s.amount)?],
        s.unit,
        config,
        None,
    );
    state.phase = s.phase;
    state.date_text = s.date_text.map(std::borrow::Cow::Borrowed);
    state.amount_text = s.amount_text.map(std::borrow::Cow::Borrowed);
    state.number = s.number;
    state.prepared = s.prepared;
    Ok(state)
}
fn value_report(
    s: State<'_>,
    state: &crate::NativeDateArithmeticValues<'_>,
    step: crate::NativeDateArithmeticValueStep,
    limit: usize,
) -> FrameResult<Option<Vec<u8>>> {
    use crate::{NativeDateArithmeticValue as V, NativeDateArithmeticValueAction as A};
    let warning = step.warning.as_ref().map(|w| NativeDateArithmeticWarning {
        code: w.code,
        message: &w.message,
    });
    match step.action {
        A::Ready(V::Null) => null(warning, limit),
        A::Ready(V::Text(text)) => report(1, text.as_bytes(), warning, limit),
        A::Ready(V::Duration { nanos, fsp }) => {
            let mut out = Vec::new();
            put(&mut out, nanos);
            put(&mut out, fsp);
            report(2, &out, warning, limit)
        }
        A::Ready(V::Unsupported(reason)) => unsupported(reason, warning, limit),
        A::Ready(V::Error(message)) => {
            let mut out = 1441u16.to_le_bytes().to_vec();
            out.extend_from_slice(message.as_bytes());
            report(4, &out, warning, limit)
        }
        A::Request { .. } | A::Overflow => request(
            State {
                phase: state.phase,
                date_text: state.date_text.as_deref(),
                amount_text: state
                    .amount_text
                    .as_deref()
                    .map(|v| if state.phase == 4 { v.trim() } else { v }),
                number: state.number,
                prepared: state.prepared,
                ..s
            },
            warning,
            limit,
        ),
    }
}
fn evaluate_head<'a>(
    a: Option<&'a [u8]>,
    b: Option<&'a [u8]>,
    c: Option<&'a [u8]>,
    d: Option<&'a [u8]>,
    duration: bool,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(a, b, c, d)?;
    if !head_valid(a, b, c, d, duration) {
        return Err(Error::Invalid);
    }
    let s = State {
        date: a,
        amount: b,
        unit: std::str::from_utf8(c.unwrap()).map_err(|_| Error::Invalid)?,
        metadata: d.unwrap(),
        phase: 0,
        date_text: None,
        amount_text: None,
        number: None,
        prepared: 0,
    };
    let mut state = value_state(s)?;
    let step = state.begin().map_err(value_error)?;
    value_report(s, &state, step, limit)
}
pub(crate) fn evaluate_date_arithmetic_head_native<'a>(
    a: Option<&'a [u8]>,
    b: Option<&'a [u8]>,
    c: Option<&'a [u8]>,
    d: Option<&'a [u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    evaluate_head(a, b, c, d, false)
}
pub(crate) fn evaluate_date_arithmetic_duration_head_native<'a>(
    a: Option<&'a [u8]>,
    b: Option<&'a [u8]>,
    c: Option<&'a [u8]>,
    d: Option<&'a [u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    evaluate_head(a, b, c, d, true)
}
pub(crate) fn evaluate_date_arithmetic_step_native<'a>(
    a: Option<&'a [u8]>,
    b: Option<&'a [u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(a, b, None, None)?;
    if !date_arithmetic_step_native_args_valid(a, b) {
        return Err(Error::Invalid);
    }
    let s = pending(a.unwrap()).ok_or(Error::Invalid)?;
    // Incoming report warnings were already delivered once by their owner.
    let mut state = value_state(s)?;
    let step = match s.phase {
        1 | 2 => state.resume_string_cow(
            b.map(|text| std::str::from_utf8(text).map(std::borrow::Cow::Borrowed))
                .transpose()
                .map_err(|_| Error::Invalid)?,
        ),
        3 => {
            let Identity::Int(number) = decode_native_identity(b.ok_or(Error::Invalid)?)? else {
                return Err(Error::Invalid);
            };
            state.resume_integer(number)
        }
        4 => state.resume_decimal(
            decimal_value(decode_native_identity(b.ok_or(Error::Invalid)?)?)
                .ok_or(Error::Invalid)?,
        ),
        _ => return Err(Error::Invalid),
    }
    .map_err(value_error)?;
    value_report(s, &state, step, limit)
}
pub(crate) fn evaluate_date_arithmetic_overflow_native(
    a: Option<&[u8]>,
    level: Option<i64>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(a, None, None, None)?;
    if !date_arithmetic_overflow_native_args_valid(a, level) {
        return Err(Error::Invalid);
    }
    let s = pending(a.unwrap()).ok_or(Error::Invalid)?;
    let mut state = value_state(s)?;
    let mode = match level {
        Some(0) => crate::NativeDateArithmeticOverflowMode::Ignore,
        Some(1) => crate::NativeDateArithmeticOverflowMode::Warn,
        Some(2) => crate::NativeDateArithmeticOverflowMode::Error,
        _ => return Err(Error::Invalid),
    };
    let step = state.resume_overflow(mode).map_err(value_error)?;
    value_report(s, &state, step, limit)
}

fn kernel(value: FrameResult<Option<Vec<u8>>>) -> Result<Option<Bytes>> {
    value.map_err(|e| other_err!("Invalid native date arithmetic transport: {:?}", e))
}
#[rpn_fn(nullable)]
fn date_arithmetic_head_native(
    a: Option<BytesRef>,
    b: Option<BytesRef>,
    c: Option<BytesRef>,
    d: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    kernel(evaluate_date_arithmetic_head_native(a, b, c, d))
}
#[rpn_fn(nullable)]
fn date_arithmetic_duration_head_native(
    a: Option<BytesRef>,
    b: Option<BytesRef>,
    c: Option<BytesRef>,
    d: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    kernel(evaluate_date_arithmetic_duration_head_native(a, b, c, d))
}
#[rpn_fn(nullable)]
fn date_arithmetic_step_native(a: Option<BytesRef>, b: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_date_arithmetic_step_native(a, b))
}
#[rpn_fn(nullable)]
fn date_arithmetic_overflow_native(
    a: Option<BytesRef>,
    level: Option<&Int>,
) -> Result<Option<Bytes>> {
    kernel(evaluate_date_arithmetic_overflow_native(a, level.copied()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode_native_identity;
    #[test]
    fn ordinary_date_arithmetic_keeps_order_warnings_precision_and_duration_metadata() {
        use NativeDateArithmeticOutcome as O;
        use NativeDateArithmeticRequest as Q;
        let frame = |v| encode_native_identity(v).unwrap();
        let meta = encode_native_date_arithmetic_metadata(1, None).unwrap();
        let one = frame(Identity::Int(1));
        let date = frame(Identity::Bytes(b"2024-01-31"));
        let head = |a, b, unit: &[u8], m: &[u8]| {
            let out = evaluate_date_arithmetic_head_native(a, b, Some(unit), Some(m))
                .unwrap()
                .unwrap();
            assert!(
                out.capacity()
                    <= native_date_arithmetic_output_bound(a, b, Some(unit), Some(m)).unwrap()
            );
            out
        };
        let step = |s: &[u8], v: Option<&[u8]>| {
            let out = evaluate_date_arithmetic_step_native(Some(s), v)
                .unwrap()
                .unwrap();
            assert!(
                out.capacity()
                    <= native_date_arithmetic_output_bound(Some(s), v, None, None).unwrap()
            );
            out
        };
        let requested = |s: &[u8], kind, index| {
            let report = decode_native_date_arithmetic_result(s).unwrap();
            assert!(
                matches!(report.outcome,O::Request{kind:k,index:i,state,..}if k==kind&&i==index&&state==s)
            );
        };
        let s = head(Some(&date), Some(&one), b"DAY", &meta);
        requested(&s, Q::CoerceString, 0);
        let out = step(&s, Some(b"2024-01-31"));
        assert_eq!(
            decode_native_date_arithmetic_result(&out).unwrap().outcome,
            O::Text("2024-02-01")
        );
        let float = frame(Identity::Real(1.5_f64.to_bits()));
        let out = head(None, Some(&float), b"DAY", &meta);
        assert_eq!(
            decode_native_date_arithmetic_result(&out).unwrap().outcome,
            O::Null
        );
        let out = head(None, Some(&float), b"DAY_SECOND", &meta);
        assert_eq!(
            decode_native_date_arithmetic_result(&out).unwrap().outcome,
            O::Unsupported("composite INTERVAL amount")
        );
        let amount = frame(Identity::Bytes(b"1.5"));
        let s = head(Some(&date), Some(&amount), b"DAY", &meta);
        let s = step(&s, Some(b"2024-01-31"));
        requested(&s, Q::CoerceString, 1);
        let out = step(&s, Some(b"1.5"));
        let report = decode_native_date_arithmetic_result(&out).unwrap();
        assert_eq!(report.outcome, O::Text("2024-02-01"));
        assert_eq!(
            report.warning,
            Some(NativeDateArithmeticWarning {
                code: 1292,
                message: "Truncated incorrect DECIMAL value: '1.5'"
            })
        );
        let unsigned = frame(Identity::UInt(u64::MAX - 1));
        let out = head(Some(&unsigned), Some(&one), b"DAY", &meta);
        assert_eq!(
            decode_native_date_arithmetic_result(&out)
                .unwrap()
                .warning
                .unwrap()
                .message,
            "Incorrect time value: '-1'"
        );
        assert_eq!(
            rounded_decimal(Identity::Decimal {
                negative: false,
                scale: 2,
                storage_scale: 2,
                declared_shape: None,
                coefficient: b"175"
            }),
            2
        );

        let last = frame(Identity::Bytes(b"9999-12-31"));
        let s = head(Some(&last), Some(&amount), b"DAY", &meta);
        let s = step(&s, Some(b"9999-12-31"));
        let s = step(&s, Some(b"1.5"));
        let report = decode_native_date_arithmetic_result(&s).unwrap();
        assert_eq!(report.warning.unwrap().code, 1292);
        assert_eq!(report.outcome, O::Overflow { state: &s });
        for level in 0..=2 {
            let out = evaluate_date_arithmetic_overflow_native(Some(&s), Some(level))
                .unwrap()
                .unwrap();
            let report = decode_native_date_arithmetic_result(&out).unwrap();
            match level {
                0 => {
                    assert_eq!(report.outcome, O::Null);
                    assert!(report.warning.is_none());
                }
                1 => {
                    assert_eq!(report.outcome, O::Null);
                    assert_eq!(report.warning.unwrap().code, 1441);
                }
                _ => {
                    assert!(matches!(report.outcome, O::Error { code: 1441, .. }));
                    assert!(report.warning.is_none());
                }
            }
        }
        let duration = frame(Identity::Duration {
            nanos: 3_600_000_000_000,
            fsp: 0,
        });
        let dm = encode_native_date_arithmetic_duration_metadata(1, 3, None).unwrap();
        let out = evaluate_date_arithmetic_duration_head_native(
            Some(&duration),
            Some(&one),
            Some(b"SECOND"),
            Some(&dm),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            decode_native_date_arithmetic_result(&out).unwrap().outcome,
            O::Duration {
                nanos: 3_601_000_000_000,
                fsp: 3
            }
        );
        let dm = encode_native_date_arithmetic_duration_metadata(1, -77, Some(2000)).unwrap();
        assert!(
            native_date_arithmetic_output_bound(
                Some(&duration),
                Some(&float),
                Some(b"SECOND_MICROSECOND"),
                Some(&dm)
            )
            .unwrap()
                >= 2000
        );
        let out = evaluate_date_arithmetic_duration_head_native(
            None,
            Some(&float),
            Some(b"SECOND_MICROSECOND"),
            Some(&dm),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            decode_native_date_arithmetic_result(&out).unwrap().outcome,
            O::Null
        );
        let out = evaluate_date_arithmetic_duration_head_native(
            Some(&duration),
            Some(&one),
            Some(b"SECOND"),
            Some(&dm),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            decode_native_date_arithmetic_result(&out).unwrap().outcome,
            O::Duration {
                nanos: 3_601_000_000_000,
                fsp: -77
            }
        );

        let amount = frame(Identity::Bytes(b" 1.2345678s "));
        let s = head(Some(&date), Some(&amount), b"SECOND", &meta);
        let s = step(&s, Some(b"2024-01-31"));
        let s = step(&s, Some(b" 1.2345678s "));
        requested(&s, Q::ParseDecimal, 1);
        assert!(matches!(
            decode_native_date_arithmetic_result(&s).unwrap().outcome,
            O::Request {
                text: Some("1.2345678s"),
                ..
            }
        ));
        let parsed = frame(Identity::Decimal {
            negative: false,
            scale: 7,
            storage_scale: 7,
            declared_shape: None,
            coefficient: b"12345678",
        });
        let out = step(&s, Some(&parsed));
        assert_eq!(
            decode_native_date_arithmetic_result(&out).unwrap().outcome,
            O::Text("2024-01-31 00:00:01.234567")
        );
        let wild = encode_native_date_arithmetic_metadata(1, Some(u32::MAX)).unwrap();
        assert!(date_arithmetic_head_native_args_valid(
            Some(&date),
            Some(&one),
            Some(b"DAY"),
            Some(&wild)
        ));
        let ft = NativeDateArithmeticFieldType {
            eval_type: NativeDateArithmeticEvalType::Datetime,
            decimal: 2,
        };
        assert_eq!(
            native_date_arithmetic_result_fsp(
                "SECOND",
                Some(ft),
                Some(NativeDateArithmeticFieldType {
                    eval_type: NativeDateArithmeticEvalType::Real,
                    decimal: -1
                })
            ),
            Some(6)
        );
        let mut broken = s;
        broken.push(0);
        assert!(decode_native_date_arithmetic_result(&broken).is_none());
    }
}
