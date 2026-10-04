// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! SDK-owned GREATEST/LEAST cursor. Native callers only actuate requested
//! casts, original comparisons and per-item session reads; they never select a
//! winner.

use std::cmp::Ordering;

use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::codec::{
    collation::native::NativeCollation,
    data_type::{Bytes, BytesRef, ChunkedVec, EvaluableRef, EvaluableRet, Int},
    mysql::{
        Decimal, Time, TimeType,
        decimal::NativeDecimalError,
        time::{
            NativeSessionTimeZone, NativeTemporalValue, native_get_time_fsp, native_parse_time,
        },
    },
};

use crate::{
    NativeIdentityFrameError as Error, NativeIdentityRef as Identity, decode_native_identity,
    encode_native_identity, local::NativeTemporalCallMetadata, native_extremum_policy::*,
    types::function::CallBuild,
};
type FrameResult<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExtremumRequest {
    CompareLt,
    CompareGt,
    CastTime,
    CastVector,
    StringBytes,
    TimeText,
    TimeContext,
    Original,
    ToReal,
    ToDecimal,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExtremumResult<'a> {
    Value(&'a [u8]),
    RetagString(&'a [u8]),
    BadArity,
    Request {
        kind: NativeExtremumRequest,
        index: usize,
        best_index: usize,
        state: &'a [u8],
    },
}

struct Reader<'a> {
    bytes: &'a [u8],
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.bytes.split_at_checked(n)?;
        self.bytes = tail;
        Some(head)
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
    fn index(&mut self) -> Option<usize> {
        usize::try_from(self.word()?).ok()
    }
    fn blob(&mut self) -> Option<&'a [u8]> {
        let n = self.index()?;
        self.take(n)
    }
    fn count(&mut self, width: usize) -> Option<usize> {
        let n = self.index()?;
        (n <= self.bytes.len() / width).then_some(n)
    }
}
fn put_word(bytes: &mut Vec<u8>, n: u64) {
    bytes.extend_from_slice(&n.to_le_bytes());
}
fn put_index(bytes: &mut Vec<u8>, n: usize) -> FrameResult<()> {
    put_word(bytes, u64::try_from(n).map_err(|_| Error::Capacity)?);
    Ok(())
}
fn put_blob(bytes: &mut Vec<u8>, value: &[u8]) -> FrameResult<()> {
    put_index(bytes, value.len())?;
    bytes.extend_from_slice(value);
    Ok(())
}
fn owned(bytes: &[u8], limit: usize) -> FrameResult<Vec<u8>> {
    if bytes.len() > limit {
        return Err(Error::Capacity);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(bytes.len())
        .map_err(|_| Error::Capacity)?;
    if output.capacity() > limit {
        return Err(Error::Capacity);
    }
    output.extend_from_slice(bytes);
    Ok(output)
}
fn result(tag: u8, payload: &[u8], limit: usize) -> FrameResult<Option<Vec<u8>>> {
    let n = payload.len().checked_add(1).ok_or(Error::Capacity)?;
    if n > limit {
        return Err(Error::Capacity);
    }
    let mut output = owned(&[], limit)?;
    output.try_reserve_exact(n).map_err(|_| Error::Capacity)?;
    if output.capacity() > limit {
        return Err(Error::Capacity);
    }
    output.push(tag);
    output.extend_from_slice(payload);
    Ok(Some(output))
}
fn kind(value: Option<&[u8]>) -> Option<NativeExtremumValueMeta> {
    let mut meta = NativeExtremumValueMeta {
        kind: 0,
        time_kind: None,
        decimal_scale: None,
    };
    let Some(value) = value else {
        return Some(meta);
    };
    meta.kind = match decode_native_identity(value).ok()? {
        Identity::MinNotNull => 1,
        Identity::MaxValue => 2,
        Identity::Int(_) => 3,
        Identity::UInt(_) => 4,
        Identity::Decimal { scale, .. } => {
            meta.decimal_scale = Some(scale);
            5
        }
        Identity::Real(_) => 6,
        Identity::Float32(_) => 7,
        Identity::String { .. } => 8,
        Identity::Bytes(_) => 9,
        Identity::BinaryLiteral(_) => 10,
        Identity::Duration { .. } => 11,
        Identity::Enum { .. } => 12,
        Identity::Bit(_) => 13,
        Identity::Set { .. } => 14,
        Identity::Time { kind, .. } => {
            meta.time_kind = Some(time_kind(kind)?);
            15
        }
        Identity::Json { .. } => 16,
        Identity::Raw(_) => 17,
        Identity::Vector(_) => 18,
    };
    Some(meta)
}
fn time_kind(kind: u8) -> Option<TimeType> {
    match kind {
        0 => Some(TimeType::Date),
        1 => Some(TimeType::DateTime),
        2 => Some(TimeType::Timestamp),
        _ => None,
    }
}
fn time_code(kind: TimeType) -> u8 {
    match kind {
        TimeType::Date => 0,
        TimeType::DateTime => 1,
        TimeType::Timestamp => 2,
    }
}
fn order(want: i64) -> Option<Ordering> {
    match want {
        -1 => Some(Ordering::Less),
        0 => Some(Ordering::Equal),
        1 => Some(Ordering::Greater),
        _ => None,
    }
}
fn eval_type(code: u8) -> Option<NativeExtremumEvalType> {
    use NativeExtremumEvalType::*;
    Some(match code {
        0 => Int,
        1 => Real,
        2 => Decimal,
        3 => String,
        4 => Datetime,
        5 => Timestamp,
        6 => Duration,
        7 => Json,
        8 => VectorFloat32,
        _ => return None,
    })
}
fn eval_code(value: NativeExtremumEvalType) -> u8 {
    use NativeExtremumEvalType::*;
    match value {
        Int => 0,
        Real => 1,
        Decimal => 2,
        String => 3,
        Datetime => 4,
        Timestamp => 5,
        Duration => 6,
        Json => 7,
        VectorFloat32 => 8,
    }
}
fn mode(code: u8) -> Option<NativeExtremumStringMode> {
    use NativeExtremumStringMode::*;
    match code {
        0 => Some(Directly),
        1 => Some(AsDate),
        2 => Some(AsDatetime),
        _ => None,
    }
}
fn flag(code: u8) -> Option<bool> {
    match code {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/// Encodes actual nullable identities and actual planner metadata, without
/// choosing a domain, scanning for a winner, or converting any operand.
pub fn encode_native_extremum_head(
    values: &[Option<&[u8]>],
    signature: Option<NativeExtremumSignature>,
    arg_decimals: &[i64],
    all_constant: bool,
) -> FrameResult<Vec<u8>> {
    let mut length = 19_usize
        .checked_add(if signature.is_some() { 3 } else { 0 })
        .and_then(|n| n.checked_add(arg_decimals.len().checked_mul(8)?))
        .ok_or(Error::Capacity)?;
    for value in values {
        length = length
            .checked_add(1)
            .and_then(|n| match value {
                Some(value) => n.checked_add(8)?.checked_add(value.len()),
                None => Some(n),
            })
            .ok_or(Error::Capacity)?;
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| Error::Capacity)?;
    bytes.push(0xe1);
    put_index(&mut bytes, values.len())?;
    for value in values {
        bytes.push(u8::from(value.is_some()));
        if let Some(value) = value {
            put_blob(&mut bytes, value)?;
        }
    }
    bytes.push(u8::from(signature.is_some()));
    if let Some(signature) = signature {
        bytes.push(eval_code(signature.arg_type));
        bytes.push(match signature.cmp_string_mode {
            NativeExtremumStringMode::Directly => 0,
            NativeExtremumStringMode::AsDate => 1,
            NativeExtremumStringMode::AsDatetime => 2,
        });
        bytes.push(u8::from(signature.ret_date));
    }
    bytes.push(u8::from(all_constant));
    put_index(&mut bytes, arg_decimals.len())?;
    for scale in arg_decimals {
        put_word(&mut bytes, *scale as u64);
    }
    // The input codec also returns a fresh measured buffer, not geometric slack.
    owned(&bytes, bytes.len())
}
struct Head {
    values: Vec<NativeExtremumValueMeta>,
    signature: Option<NativeExtremumSignature>,
    scales: Vec<i64>,
    constant: bool,
}
fn head(bytes: &[u8]) -> Option<Head> {
    let mut r = Reader { bytes };
    if r.byte()? != 0xe1 {
        return None;
    }
    let n = r.count(1)?;
    let mut values = Vec::new();
    for _ in 0..n {
        let value = if flag(r.byte()?)? {
            Some(r.blob()?)
        } else {
            None
        };
        values.push(kind(value)?);
    }
    let signature = if flag(r.byte()?)? {
        Some(NativeExtremumSignature {
            arg_type: eval_type(r.byte()?)?,
            cmp_string_mode: mode(r.byte()?)?,
            ret_date: flag(r.byte()?)?,
        })
    } else {
        None
    };
    let constant = flag(r.byte()?)?;
    let n = r.count(8)?;
    let mut scales = Vec::new();
    for _ in 0..n {
        scales.push(r.int()?);
    }
    r.bytes.is_empty().then_some(Head {
        values,
        signature,
        scales,
        constant,
    })
}

struct State {
    domain: NativeExtremumDomain,
    want: Ordering,
    collation: i64,
    values: Vec<NativeExtremumValueMeta>,
    scales: Vec<i64>,
    constant: bool,
    cursor: NativeExtremumNumericCursor,
    index: usize,
    best_index: usize,
    best: Option<Vec<u8>>,
    text: Option<Vec<u8>>,
}
impl State {
    fn request(&self) -> Option<(NativeExtremumRequest, usize, usize)> {
        use NativeExtremumDomain as D;
        use NativeExtremumRequest as Q;
        match self.domain {
            D::Numeric => {
                if let Some(request) = self.cursor.request() {
                    return Some((
                        match request.op {
                            NativeExtremumComparisonOp::Lt => Q::CompareLt,
                            NativeExtremumComparisonOp::Gt => Q::CompareGt,
                        },
                        request.candidate_index,
                        request.best_index,
                    ));
                }
                Some(
                    match self
                        .cursor
                        .finish(&self.values, &self.scales, self.constant)
                        .ok()?
                    {
                        NativeExtremumPromotion::KeepWinner { index } => {
                            (Q::Original, index, index)
                        }
                        NativeExtremumPromotion::ToReal { index } => (Q::ToReal, index, index),
                        NativeExtremumPromotion::ToDecimal { index, .. } => {
                            (Q::ToDecimal, index, index)
                        }
                    },
                )
            }
            _ if self.index >= self.values.len() => None,
            D::Time { .. } => Some((Q::CastTime, self.index, self.best_index)),
            D::Vector => Some((Q::CastVector, self.index, self.best_index)),
            D::DirectString => Some((Q::StringBytes, self.index, self.best_index)),
            D::StringAsTime { .. } => Some((
                if self.text.is_some() {
                    Q::TimeContext
                } else {
                    Q::TimeText
                },
                self.index,
                self.best_index,
            )),
        }
    }
    fn precision(&self) -> Option<u32> {
        if self.domain != NativeExtremumDomain::Numeric || self.cursor.request().is_some() {
            return None;
        }
        match self
            .cursor
            .finish(&self.values, &self.scales, self.constant)
            .ok()?
        {
            NativeExtremumPromotion::ToDecimal {
                precision_scale, ..
            } => precision_scale,
            _ => None,
        }
    }
}
fn state(bytes: &[u8]) -> Option<State> {
    let mut r = Reader { bytes };
    if r.byte()? != 3 || r.byte()? != 1 {
        return None;
    }
    use NativeExtremumDomain as D;
    let domain = match r.byte()? {
        0 => D::Numeric,
        1 => D::Time { ret_date: true },
        2 => D::Time { ret_date: false },
        3 => D::Vector,
        4 => D::DirectString,
        5 => D::StringAsTime { as_date: true },
        6 => D::StringAsTime { as_date: false },
        _ => return None,
    };
    let want = order(i64::from(r.byte()?) - 1)?;
    let collation = r.int()?;
    NativeCollation::from_tag(collation)?;
    let constant = flag(r.byte()?)?;
    let n = r.count(7)?;
    if n == 0 {
        return None;
    }
    let mut values = Vec::new();
    for _ in 0..n {
        let kind = r.byte()?;
        let time = r.byte()?;
        let has_scale = flag(r.byte()?)?;
        let scale = u32::from_le_bytes(r.take(4)?.try_into().ok()?);
        if !has_scale && scale != 0 {
            return None;
        }
        values.push(NativeExtremumValueMeta {
            kind,
            time_kind: if time == 255 {
                None
            } else {
                Some(time_kind(time)?)
            },
            decimal_scale: has_scale.then_some(scale),
        });
    }
    // Reuse policy validation, with an explicit neutral signature so the
    // stored computed domain isn't re-inferred from values.
    if !matches!(
        native_extremum_head(
            &values,
            Some(NativeExtremumSignature {
                arg_type: NativeExtremumEvalType::Real,
                cmp_string_mode: NativeExtremumStringMode::Directly,
                ret_date: false
            })
        )
        .ok()?,
        NativeExtremumHead::Domain(_)
    ) {
        return None;
    }
    let n = r.count(8)?;
    let mut scales = Vec::new();
    for _ in 0..n {
        scales.push(r.int()?);
    }
    let next = r.index()?;
    let best_cursor = r.index()?;
    let cursor = NativeExtremumNumericCursor::from_runtime_checkpoint(
        values.len(),
        next,
        best_cursor,
        want,
    )?;
    let index = r.index()?;
    let best_index = r.index()?;
    if index >= values.len() || best_index >= values.len() {
        return None;
    }
    let best = if flag(r.byte()?)? {
        Some(r.blob()?.to_vec())
    } else {
        None
    };
    let text = if flag(r.byte()?)? {
        Some(r.blob()?.to_vec())
    } else {
        None
    };
    if !r.bytes.is_empty() {
        return None;
    }
    match domain {
        D::Numeric if best.is_some() || text.is_some() || index != 0 || best_index != 0 => {
            return None;
        }
        D::Time { .. } => {
            if text.is_some() || best.as_deref().is_some_and(|v| time_value(v).is_none()) {
                return None;
            }
        }
        D::Vector => {
            if text.is_some() || best.as_deref().is_some_and(|v| vector_value(v).is_none()) {
                return None;
            }
        }
        D::DirectString if text.is_some() => return None,
        D::StringAsTime { .. } => {
            if best
                .as_deref()
                .is_some_and(|v| std::str::from_utf8(v).is_err())
                || text
                    .as_deref()
                    .is_some_and(|v| std::str::from_utf8(v).is_err())
            {
                return None;
            }
        }
        _ => {}
    }
    if domain != D::Numeric
        && (next != 1
            || best_cursor != 0
            || (index == 0) != best.is_none()
            || best.is_some() && best_index >= index)
    {
        return None;
    }
    Some(State {
        domain,
        want,
        collation,
        values,
        scales,
        constant,
        cursor,
        index,
        best_index,
        best,
        text,
    })
}
fn state_report(s: &State, limit: usize) -> FrameResult<Option<Vec<u8>>> {
    use NativeExtremumDomain as D;
    let mut out = vec![
        3,
        1,
        match s.domain {
            D::Numeric => 0,
            D::Time { ret_date: true } => 1,
            D::Time { ret_date: false } => 2,
            D::Vector => 3,
            D::DirectString => 4,
            D::StringAsTime { as_date: true } => 5,
            D::StringAsTime { as_date: false } => 6,
        },
    ];
    out.push(match s.want {
        Ordering::Less => 0,
        Ordering::Equal => 1,
        Ordering::Greater => 2,
    });
    put_word(&mut out, s.collation as u64);
    out.push(u8::from(s.constant));
    put_index(&mut out, s.values.len())?;
    for meta in &s.values {
        out.push(meta.kind);
        out.push(meta.time_kind.map_or(255, time_code));
        out.push(u8::from(meta.decimal_scale.is_some()));
        out.extend_from_slice(&meta.decimal_scale.unwrap_or(0).to_le_bytes());
    }
    put_index(&mut out, s.scales.len())?;
    for scale in &s.scales {
        put_word(&mut out, *scale as u64);
    }
    let (_, next, best) = s.cursor.runtime_checkpoint();
    put_index(&mut out, next)?;
    put_index(&mut out, best)?;
    put_index(&mut out, s.index)?;
    put_index(&mut out, s.best_index)?;
    for value in [&s.best, &s.text] {
        out.push(u8::from(value.is_some()));
        if let Some(value) = value {
            put_blob(&mut out, value)?;
        }
    }
    owned(&out, limit).map(Some)
}

pub fn decode_native_extremum_result(bytes: &[u8]) -> Option<NativeExtremumResult<'_>> {
    let (&tag, payload) = bytes.split_first()?;
    match tag {
        0 => {
            decode_native_identity(payload).ok()?;
            Some(NativeExtremumResult::Value(payload))
        }
        1 => Some(NativeExtremumResult::RetagString(payload)),
        2 if payload.is_empty() => Some(NativeExtremumResult::BadArity),
        3 => {
            let s = state(bytes)?;
            let (kind, index, best_index) = s.request()?;
            Some(NativeExtremumResult::Request {
                kind,
                index,
                best_index,
                state: bytes,
            })
        }
        _ => None,
    }
}

/// Retained report only, not transient codec/Decimal allocation peaks. Positive
/// signed-rounding precision is charged before its allocation; wrapped negative
/// u32 precision retains the original rounding behavior and needs no padding.
pub(crate) fn native_extremum_output_bound(
    first: Option<&[u8]>,
    second: Option<&[u8]>,
) -> Option<usize> {
    let precision = first
        .and_then(state)
        .and_then(|s| s.precision())
        .map_or(0, |p| (p as i32).max(0) as usize);
    first
        .map_or(0, <[u8]>::len)
        .checked_add(second.map_or(0, <[u8]>::len))?
        .checked_add(precision)?
        .checked_add(512)
}
fn bound(a: Option<&[u8]>, b: Option<&[u8]>) -> FrameResult<usize> {
    native_extremum_output_bound(a, b).ok_or(Error::Capacity)
}
fn pending(bytes: Option<&[u8]>, accepted: &[NativeExtremumRequest]) -> Option<State> {
    let s = state(bytes?)?;
    accepted.contains(&s.request()?.0).then_some(s)
}
fn actual_identity(value: Option<&[u8]>) -> bool {
    value.is_none_or(|v| decode_native_identity(v).is_ok())
}
fn time_value(bytes: &[u8]) -> Option<NativeTemporalValue> {
    let Identity::Time { core, kind, fsp } = decode_native_identity(bytes).ok()? else {
        return None;
    };
    Some(NativeTemporalValue {
        raw: core,
        kind: time_kind(kind)?,
        fsp,
    })
}
fn vector_value(bytes: &[u8]) -> Option<tidb_query_datatype::codec::mysql::NativeVectorFloat32> {
    let Identity::Vector(raw) = decode_native_identity(bytes).ok()? else {
        return None;
    };
    // Identity payloads are raw LE f32 elements, NOT the storage codec's
    // dimension-prefixed serialization. Preserve every bit (including NaN)
    // and infer only the actual payload dimension, without SQL validation.
    if raw.len() % 4 != 0 {
        return None;
    }
    let mut value = tidb_query_datatype::codec::mysql::NativeVectorFloat32::init(raw.len() / 4);
    for (element, bytes) in value.elements_mut().iter_mut().zip(raw.chunks_exact(4)) {
        *element = f32::from_bits(u32::from_le_bytes(bytes.try_into().ok()?));
    }
    Some(value)
}
pub fn extremum_head_native_args_valid(
    packet: Option<&[u8]>,
    want: Option<i64>,
    collation: Option<i64>,
) -> bool {
    packet.is_some_and(|p| head(p).is_some())
        && want.and_then(order).is_some()
        && collation.and_then(NativeCollation::from_tag).is_some()
        && native_extremum_output_bound(packet, None).is_some()
}
pub fn extremum_numeric_native_args_valid(s: Option<&[u8]>, v: Option<&[u8]>) -> bool {
    pending(
        s,
        &[
            NativeExtremumRequest::CompareLt,
            NativeExtremumRequest::CompareGt,
        ],
    )
    .is_some()
        && actual_identity(v)
        && native_extremum_output_bound(s, v).is_some()
}
pub fn extremum_time_native_args_valid(s: Option<&[u8]>, v: Option<&[u8]>) -> bool {
    pending(s, &[NativeExtremumRequest::CastTime]).is_some()
        && v.is_none_or(|v| time_value(v).is_some())
        && native_extremum_output_bound(s, v).is_some()
}
pub fn extremum_vector_native_args_valid(s: Option<&[u8]>, v: Option<&[u8]>) -> bool {
    pending(s, &[NativeExtremumRequest::CastVector]).is_some()
        && v.is_some_and(|v| vector_value(v).is_some())
        && native_extremum_output_bound(s, v).is_some()
}
pub fn extremum_string_native_args_valid(s: Option<&[u8]>, v: Option<&[u8]>) -> bool {
    pending(s, &[NativeExtremumRequest::StringBytes]).is_some()
        && v.is_some()
        && native_extremum_output_bound(s, v).is_some()
}
pub fn extremum_time_text_native_args_valid(s: Option<&[u8]>, v: Option<&[u8]>) -> bool {
    pending(s, &[NativeExtremumRequest::TimeText]).is_some()
        && v.is_none_or(|v| std::str::from_utf8(v).is_ok())
        && native_extremum_output_bound(s, v).is_some()
}
pub fn extremum_time_context_native_args_valid(s: Option<&[u8]>, m: Option<i64>) -> bool {
    pending(s, &[NativeExtremumRequest::TimeContext]).is_some()
        && matches!(m, Some(0 | 1))
        && native_extremum_output_bound(s, None).is_some()
}
pub fn extremum_finish_native_args_valid(s: Option<&[u8]>, v: Option<&[u8]>) -> bool {
    let Some(sv) = pending(
        s,
        &[
            NativeExtremumRequest::Original,
            NativeExtremumRequest::ToReal,
            NativeExtremumRequest::ToDecimal,
        ],
    ) else {
        return false;
    };
    let Some(Ok(value)) = v.map(decode_native_identity) else {
        return false;
    };
    let role = sv.request().unwrap().0;
    (match role {
        NativeExtremumRequest::Original => kind(v) == Some(sv.values[sv.request().unwrap().1]),
        NativeExtremumRequest::ToReal => matches!(value, Identity::Real(_)),
        NativeExtremumRequest::ToDecimal => matches!(value, Identity::Decimal { .. }),
        _ => false,
    }) && native_extremum_output_bound(s, v).is_some()
}

pub(crate) fn evaluate_extremum_head_native(
    packet: Option<&[u8]>,
    want: Option<i64>,
    collation: Option<i64>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(packet, None)?;
    if !extremum_head_native_args_valid(packet, want, collation) {
        return Err(Error::Invalid);
    }
    let h = head(packet.unwrap()).ok_or(Error::Invalid)?;
    let domain = match native_extremum_head(&h.values, h.signature) {
        Ok(NativeExtremumHead::Null) => return Ok(None),
        Ok(NativeExtremumHead::Domain(domain)) => domain,
        Err(NativeExtremumPolicyError::EmptyArguments) => return result(2, &[], limit),
        _ => return Err(Error::Invalid),
    };
    let want = order(want.unwrap()).ok_or(Error::Invalid)?;
    let s = State {
        domain,
        want,
        collation: collation.unwrap(),
        cursor: NativeExtremumNumericCursor::new(h.values.len(), want)
            .map_err(|_| Error::Invalid)?,
        values: h.values,
        scales: h.scales,
        constant: h.constant,
        index: 0,
        best_index: 0,
        best: None,
        text: None,
    };
    state_report(&s, limit)
}
pub(crate) fn evaluate_extremum_numeric_native(
    bytes: Option<&[u8]>,
    value: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(bytes, value)?;
    if !extremum_numeric_native_args_valid(bytes, value) {
        return Err(Error::Invalid);
    }
    let mut s = state(bytes.unwrap()).ok_or(Error::Invalid)?;
    let actual = match value.map(decode_native_identity).transpose()? {
        Some(Identity::Int(value)) => NativeExtremumComparisonValue::Int(value),
        _ => NativeExtremumComparisonValue::OtherValue,
    };
    s.cursor.observe(actual).map_err(|_| Error::Invalid)?;
    state_report(&s, limit)
}
fn finish_sequence(
    mut s: State,
    value: &[u8],
    wins: bool,
    limit: usize,
) -> FrameResult<Option<Vec<u8>>> {
    if wins {
        s.best = Some(value.to_vec());
        s.best_index = s.index;
    }
    s.index += 1;
    s.text = None;
    if s.index < s.values.len() {
        return state_report(&s, limit);
    }
    let best = s.best.ok_or(Error::Invalid)?;
    match s.domain {
        NativeExtremumDomain::Time { ret_date } => {
            let mut time = time_value(&best).ok_or(Error::Invalid)?;
            time.set_kind(if ret_date {
                TimeType::Date
            } else {
                TimeType::DateTime
            });
            let frame = encode_native_identity(Identity::Time {
                core: time.raw,
                kind: time_code(time.kind),
                fsp: time.fsp,
            })?;
            result(0, &frame, limit)
        }
        NativeExtremumDomain::Vector => result(0, &best, limit),
        NativeExtremumDomain::DirectString | NativeExtremumDomain::StringAsTime { .. } => {
            result(1, &best, limit)
        }
        _ => Err(Error::Invalid),
    }
}
pub(crate) fn evaluate_extremum_time_native(
    bytes: Option<&[u8]>,
    value: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(bytes, value)?;
    if !extremum_time_native_args_valid(bytes, value) {
        return Err(Error::Invalid);
    }
    let Some(value) = value else {
        return Ok(None);
    };
    let s = state(bytes.unwrap()).ok_or(Error::Invalid)?;
    let candidate = time_value(value).ok_or(Error::Invalid)?;
    let wins = s.best.as_deref().is_none_or(|best| {
        Time::native_core_compare(candidate.raw, time_value(best).unwrap().raw) == s.want
    });
    finish_sequence(s, value, wins, limit)
}
pub(crate) fn evaluate_extremum_vector_native(
    bytes: Option<&[u8]>,
    value: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(bytes, value)?;
    if !extremum_vector_native_args_valid(bytes, value) {
        return Err(Error::Invalid);
    }
    let value = value.unwrap();
    let s = state(bytes.unwrap()).ok_or(Error::Invalid)?;
    let candidate = vector_value(value).ok_or(Error::Invalid)?;
    let wins = s
        .best
        .as_deref()
        .is_none_or(|best| candidate.compare(&vector_value(best).unwrap()) == s.want);
    finish_sequence(s, value, wins, limit)
}
pub(crate) fn evaluate_extremum_string_native(
    bytes: Option<&[u8]>,
    value: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(bytes, value)?;
    if !extremum_string_native_args_valid(bytes, value) {
        return Err(Error::Invalid);
    }
    let value = value.unwrap();
    let s = state(bytes.unwrap()).ok_or(Error::Invalid)?;
    let wins = match &s.best {
        None => true,
        Some(best) => {
            NativeCollation::from_tag(s.collation)
                .ok_or(Error::Invalid)?
                .compare(value, best)
                .expect("raw supported collation comparison cannot fail")
                == s.want
        }
    };
    finish_sequence(s, value, wins, limit)
}
pub(crate) fn evaluate_extremum_time_text_native(
    bytes: Option<&[u8]>,
    value: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(bytes, value)?;
    if !extremum_time_text_native_args_valid(bytes, value) {
        return Err(Error::Invalid);
    }
    let Some(value) = value else {
        return Ok(None);
    };
    let mut s = state(bytes.unwrap()).ok_or(Error::Invalid)?;
    s.text = Some(value.to_vec());
    state_report(&s, limit)
}
pub(crate) fn evaluate_extremum_time_context_native(
    bytes: Option<&[u8]>,
    modes: Option<i64>,
    zone: &NativeSessionTimeZone,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(bytes, None)?;
    if !extremum_time_context_native_args_valid(bytes, modes) {
        return Err(Error::Invalid);
    }
    let s = state(bytes.unwrap()).ok_or(Error::Invalid)?;
    let NativeExtremumDomain::StringAsTime { as_date } = s.domain else {
        return Err(Error::Invalid);
    };
    let input = std::str::from_utf8(s.text.as_deref().ok_or(Error::Invalid)?)
        .map_err(|_| Error::Invalid)?;
    let kind = if as_date {
        TimeType::Date
    } else {
        TimeType::DateTime
    };
    let fsp = if as_date {
        0
    } else {
        i64::from(native_get_time_fsp(input))
    };
    let value = match native_parse_time(input, kind, fsp, false, true, modes == Some(1), true, zone)
    {
        Ok(parsed) => {
            let mut out = String::new();
            Time::write_native_core_display(parsed.time.raw, as_date, parsed.time.fsp, &mut out)
                .expect("formatting Time into String cannot fail");
            out.into_bytes()
        }
        Err(_) => input.as_bytes().to_vec(),
    };
    // Parse failures preserve original text, with NO warning in this native policy.
    let wins = s.best.as_deref().is_none_or(|best| {
        if s.want == Ordering::Greater {
            value.as_slice() > best
        } else {
            value.as_slice() < best
        }
    });
    finish_sequence(s, &value, wins, limit)
}
fn decimal_result<T: std::fmt::Debug>(
    value: std::result::Result<T, NativeDecimalError>,
) -> FrameResult<T> {
    match value {
        Ok(value) => Ok(value),
        Err(NativeDecimalError::Resource(_)) => Err(Error::Capacity),
        // Preserve the ordinary native infallible cast's invariant panic;
        // malformed coefficients are not a new SQL error or a silent NULL.
        Err(error) => panic!("shared native decimal precision cast failed: {error:?}"),
    }
}
fn decimal_frame(value: &Decimal, limit: usize) -> FrameResult<Vec<u8>> {
    // Lossless word-to-coefficient representation, preserving both scales.
    let p = value.words();
    let frac = p.storage_frac as usize;
    let integers = p.int_digits.div_ceil(9);
    let n = p.int_digits.checked_add(frac).ok_or(Error::Capacity)?;
    if n > limit {
        return Err(Error::Capacity);
    }
    let mut digits = Vec::new();
    digits.try_reserve_exact(n).map_err(|_| Error::Capacity)?;
    let mut emit = |mut word: u32, width: usize| {
        let mut buffer = [b'0'; 9];
        for byte in buffer[..width].iter_mut().rev() {
            *byte += (word % 10) as u8;
            word /= 10;
        }
        digits.extend_from_slice(&buffer[..width]);
    };
    for (index, word) in p.words[..integers].iter().enumerate() {
        emit(
            *word,
            if index == 0 {
                (p.int_digits - 1) % 9 + 1
            } else {
                9
            },
        );
    }
    let mut remaining = frac;
    for word in &p.words[integers..integers + frac.div_ceil(9)] {
        let width = remaining.min(9);
        emit(*word / 10_u32.pow((9 - width) as u32), width);
        remaining -= width;
    }
    let removable = digits.len().saturating_sub(frac.max(1));
    let leading = digits[..removable]
        .iter()
        .take_while(|d| **d == b'0')
        .count();
    digits.drain(..leading);
    encode_native_identity(Identity::Decimal {
        negative: p.negative,
        scale: p.result_frac,
        storage_scale: p.storage_frac,
        declared_shape: None,
        coefficient: &digits,
    })
}
pub(crate) fn evaluate_extremum_finish_native(
    bytes: Option<&[u8]>,
    value: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(bytes, value)?;
    if !extremum_finish_native_args_valid(bytes, value) {
        return Err(Error::Invalid);
    }
    let s = state(bytes.unwrap()).ok_or(Error::Invalid)?;
    let value = value.unwrap();
    let Some(scale) = s.precision() else {
        return result(0, value, limit);
    };
    let Identity::Decimal {
        negative,
        scale: visible,
        storage_scale,
        coefficient,
        ..
    } = decode_native_identity(value)?
    else {
        return Err(Error::Invalid);
    };
    let decimal = decimal_result(Decimal::try_from_native_digits(
        negative,
        coefficient,
        storage_scale,
        visible,
        limit,
    ))?;
    let rounded = decimal_result(decimal.try_native_cast_to_precision(0, scale, limit))?;
    let output = decimal_frame(&rounded, limit)?;
    result(0, &output, limit)
}

fn kernel(value: FrameResult<Option<Vec<u8>>>) -> Result<Option<Bytes>> {
    value.map_err(|e| other_err!("Invalid native extremum transport: {:?}", e))
}
#[rpn_fn(nullable)]
fn extremum_head_native(
    p: Option<BytesRef>,
    w: Option<&Int>,
    c: Option<&Int>,
) -> Result<Option<Bytes>> {
    kernel(evaluate_extremum_head_native(p, w.copied(), c.copied()))
}
#[rpn_fn(nullable)]
fn extremum_numeric_native(s: Option<BytesRef>, v: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_extremum_numeric_native(s, v))
}
#[rpn_fn(nullable)]
fn extremum_time_native(s: Option<BytesRef>, v: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_extremum_time_native(s, v))
}
#[rpn_fn(nullable)]
fn extremum_vector_native(s: Option<BytesRef>, v: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_extremum_vector_native(s, v))
}
#[rpn_fn(nullable)]
fn extremum_string_native(s: Option<BytesRef>, v: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_extremum_string_native(s, v))
}
#[rpn_fn(nullable)]
fn extremum_time_text_native(s: Option<BytesRef>, v: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_extremum_time_text_native(s, v))
}
#[rpn_fn(nullable)]
fn extremum_finish_native(s: Option<BytesRef>, v: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_extremum_finish_native(s, v))
}
fn init_temporal(_call: &mut CallBuild) -> Result<NativeTemporalCallMetadata> {
    Ok(NativeTemporalCallMetadata::new())
}
#[rpn_fn(nullable,capture=[metadata],metadata_mapper=init_temporal)]
fn extremum_time_context_native(
    metadata: &NativeTemporalCallMetadata,
    s: Option<BytesRef>,
    m: Option<&Int>,
) -> Result<Option<Bytes>> {
    let zone = metadata
        .zone()
        .map_err(|e| other_err!("Native extremum timezone: {:?}", e))?;
    kernel(evaluate_extremum_time_context_native(s, m.copied(), &zone))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_domains_keep_sdk_winners_raw_identity_lazy_context_and_decimal_growth() {
        let frame = |v| encode_native_identity(v).unwrap();
        let start = |values: &[Option<&[u8]>], signature, scales: &[i64], constant, collation| {
            let packet = encode_native_extremum_head(values, signature, scales, constant).unwrap();
            let out =
                evaluate_extremum_head_native(Some(&packet), Some(1), Some(collation)).unwrap();
            if let Some(out) = &out {
                assert!(
                    out.capacity() <= native_extremum_output_bound(Some(&packet), None).unwrap()
                );
            }
            out
        };
        let request = |bytes: &[u8], expected, index| {
            let Some(NativeExtremumResult::Request {
                kind,
                index: actual,
                state,
                ..
            }) = decode_native_extremum_result(bytes)
            else {
                panic!("request expected");
            };
            assert_eq!(kind, expected);
            assert_eq!(actual, index);
            assert_eq!(state, bytes);
        };
        let empty = start(&[], None, &[], false, 0).unwrap();
        assert_eq!(
            decode_native_extremum_result(&empty),
            Some(NativeExtremumResult::BadArity)
        );
        let raw = frame(Identity::Raw(&[0xff]));
        assert!(start(&[Some(&raw), None], None, &[], false, 0).is_none());
        let singleton = start(&[Some(&raw)], None, &[], false, 0).unwrap();
        request(&singleton, NativeExtremumRequest::Original, 0);
        let output = evaluate_extremum_finish_native(Some(&singleton), Some(&raw))
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_extremum_result(&output),
            Some(NativeExtremumResult::Value(&raw))
        );
        let integer = frame(Identity::Int(2));
        let nan = frame(Identity::Real(0x7ff8_0000_0000_0123));
        let mut s = start(&[Some(&nan), Some(&integer)], None, &[], false, 0).unwrap();
        request(&s, NativeExtremumRequest::CompareGt, 1);
        let zero = frame(Identity::Int(0));
        s = evaluate_extremum_numeric_native(Some(&s), Some(&zero))
            .unwrap()
            .unwrap();
        request(&s, NativeExtremumRequest::ToReal, 0);
        let output = evaluate_extremum_finish_native(Some(&s), Some(&nan))
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_extremum_result(&output),
            Some(NativeExtremumResult::Value(&nan))
        );
        // Actual nullable comparison is OtherValue, never SQL NULL output.
        let s = start(&[Some(&integer), Some(&integer)], None, &[], false, 0).unwrap();
        let s = evaluate_extremum_numeric_native(Some(&s), None)
            .unwrap()
            .unwrap();
        request(&s, NativeExtremumRequest::Original, 0);

        let decimal = frame(Identity::Decimal {
            negative: false,
            scale: 2,
            storage_scale: 2,
            declared_shape: Some((4, 2)),
            coefficient: b"125",
        });
        let mut s = start(&[Some(&integer), Some(&decimal)], None, &[0, 1000], true, 0).unwrap();
        s = evaluate_extremum_numeric_native(Some(&s), Some(&zero))
            .unwrap()
            .unwrap();
        request(&s, NativeExtremumRequest::ToDecimal, 0);
        let converted = frame(Identity::Decimal {
            negative: false,
            scale: 0,
            storage_scale: 0,
            declared_shape: None,
            coefficient: b"2",
        });
        let limit = native_extremum_output_bound(Some(&s), Some(&converted)).unwrap();
        assert!(limit >= s.len() + converted.len() + 1000);
        let output = evaluate_extremum_finish_native(Some(&s), Some(&converted))
            .unwrap()
            .unwrap();
        assert!(output.capacity() <= limit);
        let Some(NativeExtremumResult::Value(value)) = decode_native_extremum_result(&output)
        else {
            panic!();
        };
        let Identity::Decimal {
            scale,
            storage_scale,
            declared_shape,
            coefficient,
            ..
        } = decode_native_identity(value).unwrap()
        else {
            panic!();
        };
        assert_eq!((scale, storage_scale, declared_shape), (1000, 1000, None));
        assert_eq!(coefficient.len(), 1001);
        assert_eq!(coefficient[0], b'2');
        assert!(coefficient[1..].iter().all(|b| *b == b'0'));

        let signature = NativeExtremumSignature {
            arg_type: NativeExtremumEvalType::Datetime,
            cmp_string_mode: NativeExtremumStringMode::Directly,
            ret_date: false,
        };
        let time = frame(Identity::Time {
            core: 0x1234_5678_8765_4321,
            kind: 0,
            fsp: 255,
        });
        let s = start(&[Some(&time)], Some(signature), &[], false, 0).unwrap();
        request(&s, NativeExtremumRequest::CastTime, 0);
        let output = evaluate_extremum_time_native(Some(&s), Some(&time))
            .unwrap()
            .unwrap();
        let Some(NativeExtremumResult::Value(value)) = decode_native_extremum_result(&output)
        else {
            panic!();
        };
        assert_eq!(
            decode_native_identity(value),
            Ok(Identity::Time {
                core: 0x1234_5678_8765_4321,
                kind: 1,
                fsp: 255
            })
        );
        assert!(
            evaluate_extremum_time_native(Some(&s), None)
                .unwrap()
                .is_none()
        );

        // Identity Vector carries only elements: [1.0] and [2.0], no length prefix.
        let v1 = frame(Identity::Vector(&[0, 0, 128, 63]));
        let v2 = frame(Identity::Vector(&[0, 0, 0, 64]));
        assert_eq!(
            vector_value(&v1).unwrap().serialize(),
            [1, 0, 0, 0, 0, 0, 128, 63]
        );
        let nan_vector = frame(Identity::Vector(&[1, 0, 192, 127]));
        assert_eq!(
            vector_value(&nan_vector).unwrap().serialize(),
            [1, 0, 0, 0, 1, 0, 192, 127]
        );
        let s = start(&[Some(&v1), Some(&v2)], None, &[], false, 0).unwrap();
        let s = evaluate_extremum_vector_native(Some(&s), Some(&v1))
            .unwrap()
            .unwrap();
        request(&s, NativeExtremumRequest::CastVector, 1);
        let output = evaluate_extremum_vector_native(Some(&s), Some(&v2))
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_extremum_result(&output),
            Some(NativeExtremumResult::Value(&v2))
        );

        let a = frame(Identity::Bytes(b"a"));
        let padded = frame(Identity::Bytes(b"a "));
        let s = start(
            &[Some(&a), Some(&padded)],
            None,
            &[],
            false,
            NativeCollation::Utf8Mb4Bin.tag(),
        )
        .unwrap();
        let s = evaluate_extremum_string_native(Some(&s), Some(b"a"))
            .unwrap()
            .unwrap();
        let output = evaluate_extremum_string_native(Some(&s), Some(b"a "))
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_extremum_result(&output),
            Some(NativeExtremumResult::RetagString(b"a"))
        );

        let signature = NativeExtremumSignature {
            arg_type: NativeExtremumEvalType::String,
            cmp_string_mode: NativeExtremumStringMode::AsDatetime,
            ret_date: false,
        };
        let stamp = b"2020-01-02 03:04:05+00:00";
        let original = frame(Identity::Bytes(stamp));
        let s = start(&[Some(&original)], Some(signature), &[], false, 0).unwrap();
        request(&s, NativeExtremumRequest::TimeText, 0);
        assert!(
            evaluate_extremum_time_text_native(Some(&s), None)
                .unwrap()
                .is_none()
        );
        let s = evaluate_extremum_time_text_native(Some(&s), Some(stamp))
            .unwrap()
            .unwrap();
        request(&s, NativeExtremumRequest::TimeContext, 0);
        let zone = NativeSessionTimeZone::Fixed {
            name: "+08:00".into(),
            offset_secs: 28_800,
        };
        let output = evaluate_extremum_time_context_native(Some(&s), Some(0), &zone)
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_extremum_result(&output),
            Some(NativeExtremumResult::RetagString(b"2020-01-02 11:04:05"))
        );
        let bad = frame(Identity::Bytes(b"not-date"));
        let s = start(&[Some(&bad)], Some(signature), &[], false, 0).unwrap();
        let s = evaluate_extremum_time_text_native(Some(&s), Some(b"not-date"))
            .unwrap()
            .unwrap();
        let output = evaluate_extremum_time_context_native(Some(&s), Some(1), &zone)
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_extremum_result(&output),
            Some(NativeExtremumResult::RetagString(b"not-date"))
        );
        assert!(decode_native_extremum_result(&[2, 0]).is_none());
        assert!(!extremum_numeric_native_args_valid(
            Some(&singleton),
            Some(&zero)
        ));
        assert!(!extremum_finish_native_args_valid(
            Some(&singleton),
            Some(&integer)
        ));
        let mut trailing = singleton;
        trailing.push(0);
        assert!(decode_native_extremum_result(&trailing).is_none());
    }
}
