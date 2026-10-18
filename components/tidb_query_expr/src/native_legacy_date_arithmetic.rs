// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Legacy DATE_ADD/SUB compatibility transport for three original reader
//! domains. Actual control and arithmetic live in
//! NativeLegacyDateArithmeticValues. The DecimalText wire request remains
//! compatible; native callers use the exact shared NativeMyDecimal
//! parse/round/render primitive directly. This is not a complete upstream
//! Go-package transcreation claim.
use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::codec::{
    data_type::{Bytes, BytesRef, ChunkedVec, EvaluableRef, EvaluableRet, Int},
    mysql::{
        TimeType,
        time::{NativeSessionTimeZone, NativeTemporalValue},
    },
};

use crate::{
    NativeIdentityFrameError as Error, NativeIdentityRef as Identity, decode_native_identity,
    encode_native_identity, local::NativeTemporalCallMetadata, types::function::CallBuild,
};
type FrameResult<T> = std::result::Result<T, Error>;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NativeLegacyDateArithmeticDateKind {
    String,
    Int,
    Real,
    Decimal,
    Datetime,
    Duration,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NativeLegacyDateArithmeticIntervalKind {
    String,
    Int,
    Real,
    Decimal,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeLegacyDateArithmeticMetadata {
    pub date: NativeLegacyDateArithmeticDateKind,
    pub interval: NativeLegacyDateArithmeticIntervalKind,
    pub subtract: bool,
}
pub fn encode_native_legacy_date_arithmetic_metadata(
    value: NativeLegacyDateArithmeticMetadata,
) -> FrameResult<Vec<u8>> {
    fresh(
        &[
            1,
            value.date as u8,
            value.interval as u8,
            u8::from(value.subtract),
        ],
        64,
    )
}
fn metadata(bytes: &[u8]) -> Option<NativeLegacyDateArithmeticMetadata> {
    use NativeLegacyDateArithmeticDateKind as D;
    use NativeLegacyDateArithmeticIntervalKind as I;
    if bytes.len() != 4 || bytes[0] != 1 {
        return None;
    }
    let date = match bytes[1] {
        0 => D::String,
        1 => D::Int,
        2 => D::Real,
        3 => D::Decimal,
        4 => D::Datetime,
        5 => D::Duration,
        _ => return None,
    };
    let interval = match bytes[2] {
        0 => I::String,
        1 => I::Int,
        2 => I::Real,
        3 => I::Decimal,
        _ => return None,
    };
    Some(NativeLegacyDateArithmeticMetadata {
        date,
        interval,
        subtract: flag(bytes[3])?,
    })
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeLegacyDateArithmeticChannel {
    Bytes,
    FoldedInt,
    Real,
    Decimal,
    Time,
    Duration,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeLegacyDateArithmeticOutcome<'a> {
    Null,
    Text(&'a [u8]),
    Time(&'a [u8]),
    Duration(&'a [u8]),
    Request {
        index: usize,
        channel: NativeLegacyDateArithmeticChannel,
        state: &'a [u8],
    },
    Parse {
        state: &'a [u8],
    },
    DecimalText {
        round_to_zero: bool,
        text: &'a str,
        state: &'a [u8],
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeLegacyDateArithmeticResult<'a> {
    pub presence: Option<i128>,
    pub outcome: NativeLegacyDateArithmeticOutcome<'a>,
}
fn flag(value: u8) -> Option<bool> {
    match value {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
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
    fn blob(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.word()?).ok()?;
        self.take(n)
    }
    fn optional(&mut self) -> Option<Option<&'a [u8]>> {
        Some(if flag(self.byte()?)? {
            Some(self.blob()?)
        } else {
            None
        })
    }
}
fn blob(out: &mut Vec<u8>, value: &[u8]) -> FrameResult<()> {
    out.extend_from_slice(
        &u64::try_from(value.len())
            .map_err(|_| Error::Capacity)?
            .to_le_bytes(),
    );
    out.extend_from_slice(value);
    Ok(())
}
fn optional(out: &mut Vec<u8>, value: Option<&[u8]>) -> FrameResult<()> {
    out.push(u8::from(value.is_some()));
    if let Some(value) = value {
        blob(out, value)?;
    }
    Ok(())
}
fn fresh(value: &[u8], limit: usize) -> FrameResult<Vec<u8>> {
    if value.len() > limit {
        return Err(Error::Capacity);
    }
    let mut out = Vec::new();
    out.try_reserve_exact(value.len())
        .map_err(|_| Error::Capacity)?;
    if out.capacity() > limit {
        return Err(Error::Capacity);
    }
    out.extend_from_slice(value);
    Ok(out)
}
fn time_kind(code: u8) -> Option<TimeType> {
    match code {
        0 => Some(TimeType::Date),
        1 => Some(TimeType::DateTime),
        2 => Some(TimeType::Timestamp),
        _ => None,
    }
}
fn time_value(value: &[u8]) -> Option<NativeTemporalValue> {
    let Identity::Time { core, kind, fsp } = decode_native_identity(value).ok()? else {
        return None;
    };
    Some(NativeTemporalValue {
        raw: core,
        kind: time_kind(kind)?,
        fsp,
    })
}
fn duration_value(value: &[u8]) -> Option<(i64, i64)> {
    let Identity::Duration { nanos, fsp } = decode_native_identity(value).ok()? else {
        return None;
    };
    Some((nanos, fsp))
}
fn date_channel(meta: NativeLegacyDateArithmeticMetadata) -> NativeLegacyDateArithmeticChannel {
    use NativeLegacyDateArithmeticChannel as C;
    use NativeLegacyDateArithmeticDateKind as D;
    match meta.date {
        D::String => C::Bytes,
        D::Int => C::FoldedInt,
        D::Real => C::Real,
        D::Decimal => C::Decimal,
        D::Datetime => C::Time,
        D::Duration => C::Duration,
    }
}
fn interval_channel(meta: NativeLegacyDateArithmeticMetadata) -> NativeLegacyDateArithmeticChannel {
    use NativeLegacyDateArithmeticChannel as C;
    use NativeLegacyDateArithmeticIntervalKind as I;
    match meta.interval {
        I::String => C::Bytes,
        I::Int => C::FoldedInt,
        I::Real => C::Real,
        I::Decimal => C::Decimal,
    }
}
fn channel_valid(channel: NativeLegacyDateArithmeticChannel, value: &[u8]) -> bool {
    use NativeLegacyDateArithmeticChannel as C;
    match channel {
        C::Bytes => true,
        C::FoldedInt => value.len() == 16,
        C::Real => value.len() == 8,
        C::Decimal => matches!(decode_native_identity(value), Ok(Identity::Decimal { .. })),
        C::Time => time_value(value).is_some(),
        C::Duration => duration_value(value).is_some(),
    }
}
// Phase 0 unit getter; 1 date getter; 2 zone-bound date parser; 3 interval
// getter; 4 MyDecimal from_string/render; 5 MyDecimal
// from_string/HalfUp/render.
#[derive(Clone, Copy)]
struct State<'a> {
    meta: NativeLegacyDateArithmeticMetadata,
    phase: u8,
    unit: Option<&'a str>,
    date: Option<&'a [u8]>,
    time: Option<NativeTemporalValue>,
    duration: Option<(i64, i64)>,
    text: Option<&'a str>,
}
impl State<'_> {
    fn request(&self) -> Option<(usize, NativeLegacyDateArithmeticChannel)> {
        match self.phase {
            0 => Some((2, NativeLegacyDateArithmeticChannel::Bytes)),
            1 => Some((0, date_channel(self.meta))),
            3 => Some((1, interval_channel(self.meta))),
            _ => None,
        }
    }
    fn text_domain(&self) -> bool {
        matches!(
            self.meta.date,
            NativeLegacyDateArithmeticDateKind::String
                | NativeLegacyDateArithmeticDateKind::Int
                | NativeLegacyDateArithmeticDateKind::Real
                | NativeLegacyDateArithmeticDateKind::Decimal
        )
    }
}
fn state(bytes: &[u8]) -> Option<State<'_>> {
    if bytes.get(..2)? != &[4, 0] {
        return None;
    }
    let mut r = Reader { bytes: &bytes[2..] };
    let meta = metadata(r.take(4)?)?;
    let phase = r.byte()?;
    if phase > 5 {
        return None;
    }
    let unit = r.optional()?.map(std::str::from_utf8).transpose().ok()?;
    let date = r.optional()?;
    let time = if flag(r.byte()?)? {
        Some(NativeTemporalValue {
            raw: r.word()?,
            kind: time_kind(r.byte()?)?,
            fsp: r.byte()?,
        })
    } else {
        None
    };
    let duration = if flag(r.byte()?)? {
        Some((r.word()? as i64, r.word()? as i64))
    } else {
        None
    };
    let text = r.optional()?.map(std::str::from_utf8).transpose().ok()?;
    if !r.bytes.is_empty() {
        return None;
    }
    let s = State {
        meta,
        phase,
        unit,
        date,
        time,
        duration,
        text,
    };
    if (matches!(phase, 4 | 5)) != text.is_some() {
        return None;
    }
    if s.text_domain() {
        if duration.is_some() {
            return None;
        }
        match phase {
            0 if unit.is_some() || date.is_some() || time.is_some() => return None,
            1 if unit.is_none() || date.is_some() || time.is_some() => return None,
            2 if unit.is_none()
                || time.is_some()
                || !date.is_some_and(|v| channel_valid(date_channel(meta), v)) =>
            {
                return None;
            }
            3..=5 if unit.is_none() || date.is_some() || time.is_none() => return None,
            _ => {}
        }
    } else {
        if date.is_some() || phase == 2 {
            return None;
        }
        if phase == 1 {
            if unit.is_some() || time.is_some() || duration.is_some() {
                return None;
            }
        } else {
            if meta.date == NativeLegacyDateArithmeticDateKind::Datetime {
                if time.is_none() || duration.is_some() {
                    return None;
                }
            } else if duration.is_none() || time.is_some() {
                return None;
            }
            if (phase == 0) != unit.is_none() {
                return None;
            }
        }
    }
    if phase == 4
        && (meta.interval != NativeLegacyDateArithmeticIntervalKind::String
            || !unit?.eq_ignore_ascii_case("SECOND"))
    {
        return None;
    }
    if phase == 5 && meta.interval != NativeLegacyDateArithmeticIntervalKind::Decimal {
        return None;
    }
    Some(s)
}
fn state_report(s: State<'_>, limit: usize) -> FrameResult<Option<Vec<u8>>> {
    let mut out = vec![4, 0];
    out.extend_from_slice(&encode_native_legacy_date_arithmetic_metadata(s.meta)?);
    out.push(s.phase);
    optional(&mut out, s.unit.map(str::as_bytes))?;
    optional(&mut out, s.date)?;
    out.push(u8::from(s.time.is_some()));
    if let Some(t) = s.time {
        out.extend_from_slice(&t.raw.to_le_bytes());
        out.push(t.kind as u8);
        out.push(t.fsp);
    }
    out.push(u8::from(s.duration.is_some()));
    if let Some((n, f)) = s.duration {
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&f.to_le_bytes());
    }
    optional(&mut out, s.text.map(str::as_bytes))?;
    fresh(&out, limit).map(Some)
}
fn terminal(tag: u8, payload: &[u8], limit: usize) -> FrameResult<Option<Vec<u8>>> {
    let mut out = vec![tag, if tag == 0 { 1 } else { 2 }];
    out.extend_from_slice(payload);
    fresh(&out, limit).map(Some)
}
fn null(limit: usize) -> FrameResult<Option<Vec<u8>>> {
    terminal(0, &[], limit)
}
pub fn decode_native_legacy_date_arithmetic_result(
    bytes: &[u8],
) -> Option<NativeLegacyDateArithmeticResult<'_>> {
    use NativeLegacyDateArithmeticOutcome as O;
    let (&tag, rest) = bytes.split_first()?;
    let (&presence, body) = rest.split_first()?;
    let (presence, outcome) = match (tag, presence) {
        (0, 1) if body.is_empty() => (Some(0), O::Null),
        (1, 2) => (Some(1), O::Text(body)),
        (2, 2) => {
            time_value(body)?;
            (Some(1), O::Time(body))
        }
        (3, 2) => {
            duration_value(body)?;
            (Some(1), O::Duration(body))
        }
        (4, 0) => {
            let s = state(bytes)?;
            let outcome = match s.phase {
                2 => O::Parse { state: bytes },
                4 | 5 => O::DecimalText {
                    round_to_zero: s.phase == 5,
                    text: s.text?,
                    state: bytes,
                },
                _ => {
                    let (index, channel) = s.request()?;
                    O::Request {
                        index,
                        channel,
                        state: bytes,
                    }
                }
            };
            (None, outcome)
        }
        _ => return None,
    };
    Some(NativeLegacyDateArithmeticResult { presence, outcome })
}
fn decimal_scale(value: Option<&[u8]>) -> Option<usize> {
    match value.and_then(|v| decode_native_identity(v).ok()) {
        Some(Identity::Decimal { scale, .. }) => usize::try_from(scale).ok(),
        _ => Some(0),
    }
}
/// Retained reply only. Lossy UTF-8 can triple raw bytes; the source decimal's
/// actual visible precision is additionally charged before its display step.
pub(crate) fn native_legacy_date_arithmetic_output_bound(
    first: Option<&[u8]>,
    second: Option<&[u8]>,
) -> Option<usize> {
    let pending = first.and_then(state);
    let reply_scale = if pending
        .and_then(|s| s.request())
        .is_some_and(|(_, c)| c == NativeLegacyDateArithmeticChannel::Decimal)
    {
        decimal_scale(second)?
    } else {
        0
    };
    let date_scale = match pending {
        Some(s) if s.phase == 2 && s.meta.date == NativeLegacyDateArithmeticDateKind::Decimal => {
            decimal_scale(s.date)?
        }
        _ => 0,
    };
    let extra = reply_scale.checked_add(date_scale)?;
    first
        .map_or(0, <[u8]>::len)
        .checked_add(second.map_or(0, <[u8]>::len))?
        .checked_mul(3)?
        .checked_add(extra)?
        .checked_add(512)
}
fn bound(a: Option<&[u8]>, b: Option<&[u8]>) -> FrameResult<usize> {
    native_legacy_date_arithmetic_output_bound(a, b).ok_or(Error::Capacity)
}
fn head_valid(raw: Option<&[u8]>, domain: u8) -> bool {
    let Some(m) = raw.and_then(metadata) else {
        return false;
    };
    (match domain {
        0 => matches!(
            m.date,
            NativeLegacyDateArithmeticDateKind::String
                | NativeLegacyDateArithmeticDateKind::Int
                | NativeLegacyDateArithmeticDateKind::Real
                | NativeLegacyDateArithmeticDateKind::Decimal
        ),
        1 => m.date == NativeLegacyDateArithmeticDateKind::Datetime,
        2 => m.date == NativeLegacyDateArithmeticDateKind::Duration,
        _ => false,
    }) && native_legacy_date_arithmetic_output_bound(raw, None).is_some()
}
pub fn legacy_date_arithmetic_text_head_native_args_valid(raw: Option<&[u8]>) -> bool {
    head_valid(raw, 0)
}
pub fn legacy_date_arithmetic_time_head_native_args_valid(raw: Option<&[u8]>) -> bool {
    head_valid(raw, 1)
}
pub fn legacy_date_arithmetic_duration_head_native_args_valid(raw: Option<&[u8]>) -> bool {
    head_valid(raw, 2)
}
pub fn legacy_date_arithmetic_step_native_args_valid(
    raw: Option<&[u8]>,
    value: Option<&[u8]>,
) -> bool {
    let Some(s) = raw.and_then(state) else {
        return false;
    };
    let valid = match s.phase {
        4 | 5 => value.is_some(),
        2 => false,
        _ => s
            .request()
            .is_some_and(|(_, c)| value.is_none_or(|v| channel_valid(c, v))),
    };
    valid && native_legacy_date_arithmetic_output_bound(raw, value).is_some()
}
pub fn legacy_date_arithmetic_parse_native_args_valid(
    raw: Option<&[u8]>,
    flags: Option<i64>,
) -> bool {
    flags == Some(0)
        && raw.and_then(state).is_some_and(|s| s.phase == 2)
        && native_legacy_date_arithmetic_output_bound(raw, None).is_some()
}
fn value_error(error: crate::NativeLegacyDateArithmeticValueError) -> Error {
    match error {
        crate::NativeLegacyDateArithmeticValueError::InvalidResponse => Error::Invalid,
        crate::NativeLegacyDateArithmeticValueError::ResourceLimit => Error::Capacity,
    }
}
fn value_input(
    channel: NativeLegacyDateArithmeticChannel,
    value: Option<&[u8]>,
) -> FrameResult<crate::NativeLegacyDateArithmeticInput<'_>> {
    use NativeLegacyDateArithmeticChannel as C;

    use crate::NativeLegacyDateArithmeticInput as V;
    Ok(match channel {
        C::Bytes => V::Bytes(value),
        C::FoldedInt => V::FoldedInt(
            value
                .map(|v| {
                    v.try_into()
                        .map(i128::from_le_bytes)
                        .map_err(|_| Error::Invalid)
                })
                .transpose()?,
        ),
        C::Real => V::Real(
            value
                .map(|v| {
                    v.try_into()
                        .map(u64::from_le_bytes)
                        .map(f64::from_bits)
                        .map_err(|_| Error::Invalid)
                })
                .transpose()?,
        ),
        C::Time => V::Time(
            value
                .map(|v| time_value(v).ok_or(Error::Invalid))
                .transpose()?,
        ),
        C::Duration => V::Duration(
            value
                .map(|v| duration_value(v).ok_or(Error::Invalid))
                .transpose()?,
        ),
        C::Decimal => V::Decimal(
            value
                .map(|v| {
                    let Identity::Decimal {
                        negative,
                        scale,
                        storage_scale,
                        declared_shape,
                        coefficient,
                    } = decode_native_identity(v)?
                    else {
                        return Err(Error::Invalid);
                    };
                    Ok(tidb_query_datatype::codec::mysql::NativeDecimalParseRef {
                        negative,
                        digits: coefficient,
                        scale,
                        storage_scale,
                        declared_shape,
                    })
                })
                .transpose()?,
        ),
    })
}
fn value_state(s: State<'_>) -> FrameResult<crate::NativeLegacyDateArithmeticValues> {
    crate::NativeLegacyDateArithmeticValues::from_compat_parts(
        s.meta,
        s.phase,
        s.unit,
        s.date
            .map(|v| value_input(date_channel(s.meta), Some(v)))
            .transpose()?,
        s.time,
        s.duration,
        s.text,
    )
    .map_err(value_error)
}
fn value_report(
    s: State<'_>,
    actual: &crate::NativeLegacyDateArithmeticValues,
    step: crate::NativeLegacyDateArithmeticValueStep,
    date_reply: Option<&[u8]>,
    limit: usize,
) -> FrameResult<Option<Vec<u8>>> {
    use crate::{NativeLegacyDateArithmeticValue as V, NativeLegacyDateArithmeticValueStep as A};
    match step {
        A::Ready {
            value: V::Null,
            presence: 0,
        } => null(limit),
        A::Ready {
            value: V::Text(text),
            presence: 1,
        } => terminal(1, &text, limit),
        A::Ready {
            value: V::Time(value),
            presence: 1,
        } => terminal(
            2,
            &encode_native_identity(Identity::Time {
                core: value.raw,
                kind: value.kind as u8,
                fsp: value.fsp,
            })?,
            limit,
        ),
        A::Ready {
            value: V::Duration { nanos, fsp },
            presence: 1,
        } => terminal(
            3,
            &encode_native_identity(Identity::Duration { nanos, fsp })?,
            limit,
        ),
        A::Ready { .. } => Err(Error::Invalid),
        A::Source { .. } | A::ParseDate | A::DecimalText { .. } => state_report(
            State {
                meta: s.meta,
                phase: actual.phase,
                unit: actual.unit.as_deref(),
                date: if actual.date.is_some() {
                    date_reply.or(s.date)
                } else {
                    None
                },
                time: actual.time,
                duration: actual.duration,
                text: actual.text.as_deref(),
            },
            limit,
        ),
    }
}
fn head(raw: Option<&[u8]>, domain: u8) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(raw, None)?;
    if !head_valid(raw, domain) {
        return Err(Error::Invalid);
    }
    let meta = metadata(raw.unwrap()).ok_or(Error::Invalid)?;
    let s = State {
        meta,
        phase: if domain == 0 { 0 } else { 1 },
        unit: None,
        date: None,
        time: None,
        duration: None,
        text: None,
    };
    let mut actual = crate::NativeLegacyDateArithmeticValues::compat_initial(meta);
    let step = actual.begin().map_err(value_error)?;
    value_report(s, &actual, step, None, limit)
}
pub(crate) fn evaluate_legacy_date_arithmetic_text_head_native(
    raw: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    head(raw, 0)
}
pub(crate) fn evaluate_legacy_date_arithmetic_time_head_native(
    raw: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    head(raw, 1)
}
pub(crate) fn evaluate_legacy_date_arithmetic_duration_head_native(
    raw: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    head(raw, 2)
}
pub(crate) fn evaluate_legacy_date_arithmetic_step_native(
    raw: Option<&[u8]>,
    value: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(raw, value)?;
    if !legacy_date_arithmetic_step_native_args_valid(raw, value) {
        return Err(Error::Invalid);
    }
    let s = state(raw.unwrap()).ok_or(Error::Invalid)?;
    let mut actual = value_state(s)?;
    let step = if matches!(s.phase, 4 | 5) {
        actual.resume_decimal_text(value.ok_or(Error::Invalid)?)
    } else {
        actual.resume_source(value_input(s.request().ok_or(Error::Invalid)?.1, value)?)
    }
    .map_err(value_error)?;
    value_report(
        s,
        &actual,
        step,
        if s.phase == 1 { value } else { None },
        limit,
    )
}
pub(crate) fn evaluate_legacy_date_arithmetic_parse_native(
    raw: Option<&[u8]>,
    flags: Option<i64>,
    zone: &NativeSessionTimeZone,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(raw, None)?;
    if !legacy_date_arithmetic_parse_native_args_valid(raw, flags) {
        return Err(Error::Invalid);
    }
    let s = state(raw.unwrap()).ok_or(Error::Invalid)?;
    let mut actual = value_state(s)?;
    let step = actual.parse_date(zone).map_err(value_error)?;
    value_report(s, &actual, step, None, limit)
}
fn kernel(value: FrameResult<Option<Vec<u8>>>) -> Result<Option<Bytes>> {
    value.map_err(|e| other_err!("Invalid legacy date arithmetic transport: {:?}", e))
}
#[rpn_fn(nullable)]
fn legacy_date_arithmetic_text_head_native(raw: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_legacy_date_arithmetic_text_head_native(raw))
}
#[rpn_fn(nullable)]
fn legacy_date_arithmetic_time_head_native(raw: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_legacy_date_arithmetic_time_head_native(raw))
}
#[rpn_fn(nullable)]
fn legacy_date_arithmetic_duration_head_native(raw: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_legacy_date_arithmetic_duration_head_native(raw))
}
#[rpn_fn(nullable)]
fn legacy_date_arithmetic_step_native(
    raw: Option<BytesRef>,
    value: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    kernel(evaluate_legacy_date_arithmetic_step_native(raw, value))
}
fn init_temporal(_call: &mut CallBuild) -> Result<NativeTemporalCallMetadata> {
    Ok(NativeTemporalCallMetadata::new())
}
#[rpn_fn(nullable,capture=[metadata],metadata_mapper=init_temporal)]
fn legacy_date_arithmetic_parse_native(
    metadata: &NativeTemporalCallMetadata,
    raw: Option<BytesRef>,
    flags: Option<&Int>,
) -> Result<Option<Bytes>> {
    let zone = metadata
        .zone()
        .map_err(|e| other_err!("Legacy date arithmetic timezone: {:?}", e))?;
    kernel(evaluate_legacy_date_arithmetic_parse_native(
        raw,
        flags.copied(),
        &zone,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_domains_preserve_getter_order_actual_width_generic_decimal_and_presence() {
        use NativeLegacyDateArithmeticChannel as C;
        use NativeLegacyDateArithmeticDateKind as D;
        use NativeLegacyDateArithmeticIntervalKind as I;
        use NativeLegacyDateArithmeticOutcome as O;
        let meta = |date, interval| {
            encode_native_legacy_date_arithmetic_metadata(NativeLegacyDateArithmeticMetadata {
                date,
                interval,
                subtract: false,
            })
            .unwrap()
        };
        let step = |s: &[u8], v: Option<&[u8]>| {
            let out = evaluate_legacy_date_arithmetic_step_native(Some(s), v)
                .unwrap()
                .unwrap();
            assert!(
                out.capacity() <= native_legacy_date_arithmetic_output_bound(Some(s), v).unwrap()
            );
            out
        };
        let request = |s: &[u8], index, channel| {
            let report = decode_native_legacy_date_arithmetic_result(s).unwrap();
            assert_eq!(report.presence, None);
            assert_eq!(
                report.outcome,
                O::Request {
                    index,
                    channel,
                    state: s
                }
            );
        };
        let zone = NativeSessionTimeZone::Fixed {
            name: "UTC".to_owned(),
            offset_secs: 0,
        };
        let parse = |s: &[u8]| {
            assert_eq!(
                decode_native_legacy_date_arithmetic_result(s)
                    .unwrap()
                    .outcome,
                O::Parse { state: s }
            );
            evaluate_legacy_date_arithmetic_parse_native(Some(s), Some(0), &zone)
                .unwrap()
                .unwrap()
        };
        let m = meta(D::String, I::Int);
        let h = evaluate_legacy_date_arithmetic_text_head_native(Some(&m))
            .unwrap()
            .unwrap();
        request(&h, 2, C::Bytes);
        let s = step(&h, Some(b"MONTH"));
        request(&s, 0, C::Bytes);
        let s = step(&s, Some(b"2024-01-31"));
        let s = parse(&s);
        request(&s, 1, C::FoldedInt);
        let parsed = state(&s).unwrap().time.unwrap();
        let out = step(&s, Some(&1_i128.to_le_bytes()));
        let report = decode_native_legacy_date_arithmetic_result(&out).unwrap();
        assert_eq!(report.presence, Some(1));
        assert_eq!(report.outcome, O::Text(b"2024-02-29"));
        let s = step(&h, Some(b"MONTH"));
        let s = step(&s, Some(b"0000-00-00"));
        let out = parse(&s);
        assert_eq!(
            decode_native_legacy_date_arithmetic_result(&out),
            Some(NativeLegacyDateArithmeticResult {
                presence: Some(0),
                outcome: O::Null
            })
        );
        let out = step(&h, None);
        assert_eq!(
            decode_native_legacy_date_arithmetic_result(&out)
                .unwrap()
                .presence,
            Some(0)
        );
        let m = meta(D::Int, I::Int);
        let s = evaluate_legacy_date_arithmetic_text_head_native(Some(&m))
            .unwrap()
            .unwrap();
        let s = step(&s, Some(b"DAY"));
        request(&s, 0, C::FoldedInt);
        let out = step(&s, Some(&i128::MAX.to_le_bytes()));
        assert_eq!(
            decode_native_legacy_date_arithmetic_result(&out)
                .unwrap()
                .outcome,
            O::Null
        );

        let m = meta(D::Duration, I::String);
        let s = evaluate_legacy_date_arithmetic_duration_head_native(Some(&m))
            .unwrap()
            .unwrap();
        request(&s, 0, C::Duration);
        let value = encode_native_identity(Identity::Duration {
            nanos: 3_600_000_000_000,
            fsp: -77,
        })
        .unwrap();
        let s = step(&s, Some(&value));
        request(&s, 2, C::Bytes);
        let s = step(&s, Some(b"SECOND"));
        request(&s, 1, C::Bytes);
        let s = step(&s, Some(b"1e2"));
        assert_eq!(
            decode_native_legacy_date_arithmetic_result(&s)
                .unwrap()
                .outcome,
            O::DecimalText {
                round_to_zero: false,
                text: "1e2",
                state: &s
            }
        );
        let out = step(&s, Some(b"100"));
        let report = decode_native_legacy_date_arithmetic_result(&out).unwrap();
        assert_eq!(report.presence, Some(1));
        let O::Duration(value) = report.outcome else {
            panic!("duration result")
        };
        assert_eq!(duration_value(value), Some((3_700_000_000_000, 0)));

        let m = meta(D::String, I::Decimal);
        let s = evaluate_legacy_date_arithmetic_text_head_native(Some(&m))
            .unwrap()
            .unwrap();
        let s = step(&s, Some(b"DAY"));
        let s = step(&s, Some(b"2024-01-31"));
        let s = parse(&s);
        request(&s, 1, C::Decimal);
        let huge = encode_native_identity(Identity::Decimal {
            negative: false,
            scale: 1_000_000,
            storage_scale: 0,
            declared_shape: None,
            coefficient: b"1",
        })
        .unwrap();
        assert!(
            native_legacy_date_arithmetic_output_bound(Some(&s), Some(&huge)).unwrap() >= 1_000_000
        );
        let decimal = encode_native_identity(Identity::Decimal {
            negative: false,
            scale: 1,
            storage_scale: 1,
            declared_shape: None,
            coefficient: b"15",
        })
        .unwrap();
        let s = step(&s, Some(&decimal));
        assert_eq!(
            decode_native_legacy_date_arithmetic_result(&s)
                .unwrap()
                .outcome,
            O::DecimalText {
                round_to_zero: true,
                text: "1.5",
                state: &s
            }
        );
        let out = step(&s, Some(b"2"));
        assert_eq!(
            decode_native_legacy_date_arithmetic_result(&out)
                .unwrap()
                .outcome,
            O::Text(b"2024-02-02")
        );

        let m = meta(D::Datetime, I::Int);
        let h = evaluate_legacy_date_arithmetic_time_head_native(Some(&m))
            .unwrap()
            .unwrap();
        request(&h, 0, C::Time);
        let zero = encode_native_identity(Identity::Time {
            core: 0,
            kind: 2,
            fsp: 255,
        })
        .unwrap();
        let s = step(&h, Some(&zero));
        request(&s, 2, C::Bytes);
        let s = step(&s, Some(b"DAY"));
        request(&s, 1, C::FoldedInt);
        assert!(legacy_date_arithmetic_step_native_args_valid(
            Some(&s),
            Some(&i128::MAX.to_le_bytes())
        ));
        let value = encode_native_identity(Identity::Time {
            core: parsed.raw,
            kind: 2,
            fsp: 255,
        })
        .unwrap();
        let s = step(&h, Some(&value));
        let s = step(&s, Some(b"DAY"));
        let out = step(&s, Some(&1_i128.to_le_bytes()));
        let report = decode_native_legacy_date_arithmetic_result(&out).unwrap();
        let O::Time(value) = report.outcome else {
            panic!("time result")
        };
        let t = time_value(value).unwrap();
        assert_eq!(t.kind, TimeType::Timestamp);
        assert_eq!(t.fsp, 255);
        assert_eq!(report.presence, Some(1));
        let mut corrupt = out;
        corrupt[1] = 1;
        assert!(decode_native_legacy_date_arithmetic_result(&corrupt).is_none());
        let mut corrupt = s;
        corrupt.push(0);
        assert!(decode_native_legacy_date_arithmetic_result(&corrupt).is_none());
        assert!(!legacy_date_arithmetic_text_head_native_args_valid(Some(
            &m
        )));
    }
}
