// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! INTERVAL's eager value path and lazy expression path are deliberately
//! distinct: eager real conversion consumes the entire suffix before its <=
//! partition, whereas lazy binary search visits only target<boundary probes.

use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::codec::data_type::{
    Bytes, BytesRef, ChunkedVec, EvaluableRef, EvaluableRet,
};

use crate::{
    NativeIdentityFrameError as Error, NativeIdentityRef as Identity, decode_native_identity,
    impl_compare::{
        BasicComparer, CmpOp, CmpOpLe, CmpOpLt, Comparer, IntUintComparer, UintIntComparer,
        UintUintComparer,
    },
};
type FrameResult<T> = std::result::Result<T, Error>;

/// Actual native EvalType metadata, not a precomputed all-integer decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NativeIntervalEvalType {
    Int,
    Real,
    Decimal,
    String,
    Datetime,
    Timestamp,
    Duration,
    Json,
    VectorFloat32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeIntervalFieldType {
    pub eval_type: NativeIntervalEvalType,
    /// Full original u32 flags. Only the original NOT_NULL bit (bit 0) is read.
    pub flags: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeIntervalCast {
    Int,
    Real,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeIntervalResult<'a> {
    IntIndex(i64),
    SentinelsError,
    Request {
        index: usize,
        cast: NativeIntervalCast,
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
    fn index(&mut self) -> Option<usize> {
        usize::try_from(self.word()?).ok()
    }
    fn count(&mut self, width: usize) -> Option<usize> {
        let count = self.index()?;
        (count <= self.bytes.len() / width).then_some(count)
    }
    fn blob(&mut self) -> Option<&'a [u8]> {
        let n = self.index()?;
        self.take(n)
    }
}
fn flag(value: u8) -> Option<bool> {
    match value {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}
fn put_word(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_index(out: &mut Vec<u8>, value: usize) -> FrameResult<()> {
    put_word(out, u64::try_from(value).map_err(|_| Error::Capacity)?);
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
fn eval_type(value: u8) -> Option<NativeIntervalEvalType> {
    use NativeIntervalEvalType::*;
    Some(match value {
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

/// Nullable original identities, in original order. No cast or classification
/// is performed by the input codec; the SDK head owns sentinel/NULL precedence.
pub fn encode_native_interval_eager_head(values: &[Option<&[u8]>]) -> FrameResult<Vec<u8>> {
    let size = values
        .iter()
        .try_fold(9_usize, |size, value| {
            size.checked_add(1)?.checked_add(match value {
                Some(v) => v.len().checked_add(8)?,
                None => 0,
            })
        })
        .ok_or(Error::Capacity)?;
    let mut out = Vec::new();
    out.try_reserve_exact(size).map_err(|_| Error::Capacity)?;
    out.push(0xe2);
    put_index(&mut out, values.len())?;
    for value in values {
        out.push(u8::from(value.is_some()));
        if let Some(value) = value {
            put_index(&mut out, value.len())?;
            out.extend_from_slice(value);
        }
    }
    fresh(&out, size)
}
/// Full actual optional field-type observations. Unknown flag bits are carried
/// untouched; missing types are neither invented nor replaced by value kinds.
pub fn encode_native_interval_lazy_head(
    types: &[Option<NativeIntervalFieldType>],
) -> FrameResult<Vec<u8>> {
    let size = types
        .iter()
        .try_fold(9_usize, |n, t| {
            n.checked_add(if t.is_some() { 6 } else { 1 })
        })
        .ok_or(Error::Capacity)?;
    let mut out = Vec::new();
    out.try_reserve_exact(size).map_err(|_| Error::Capacity)?;
    out.push(0xe3);
    put_index(&mut out, types.len())?;
    for t in types {
        out.push(u8::from(t.is_some()));
        if let Some(t) = t {
            out.push(t.eval_type as u8);
            out.extend_from_slice(&t.flags.to_le_bytes());
        }
    }
    fresh(&out, size)
}
fn eager_head(bytes: &[u8]) -> Option<Vec<Option<Identity<'_>>>> {
    let mut r = Reader { bytes };
    if r.byte()? != 0xe2 {
        return None;
    }
    let count = r.count(1)?;
    if count < 2 {
        return None;
    }
    let mut values = Vec::new();
    for _ in 0..count {
        values.push(if flag(r.byte()?)? {
            Some(decode_native_identity(r.blob()?).ok()?)
        } else {
            None
        });
    }
    r.bytes.is_empty().then_some(values)
}
fn lazy_head(bytes: &[u8]) -> Option<Vec<Option<NativeIntervalFieldType>>> {
    let mut r = Reader { bytes };
    if r.byte()? != 0xe3 {
        return None;
    }
    let count = r.count(1)?;
    if count < 2 {
        return None;
    }
    let mut types = Vec::new();
    for _ in 0..count {
        types.push(if flag(r.byte()?)? {
            Some(NativeIntervalFieldType {
                eval_type: eval_type(r.byte()?)?,
                flags: u32::from_le_bytes(r.take(4)?.try_into().ok()?),
            })
        } else {
            None
        });
    }
    r.bytes.is_empty().then_some(types)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Number {
    Signed(i64),
    Unsigned(u64),
    Real(u64),
}
fn number(value: Identity<'_>) -> Option<Number> {
    match value {
        Identity::Int(v) => Some(Number::Signed(v)),
        Identity::UInt(v) => Some(Number::Unsigned(v)),
        Identity::Real(v) => Some(Number::Real(v)),
        _ => None,
    }
}
fn integer_predicate<F: CmpOp>(left: Number, right: Number) -> bool {
    use Number::*;
    let result = match (left, right) {
        (Signed(a), Signed(b)) => BasicComparer::<i64, F>::compare(Some(&a), Some(&b)),
        (Signed(a), Unsigned(b)) => IntUintComparer::<F>::compare(Some(&a), Some(&(b as i64))),
        (Unsigned(a), Signed(b)) => UintIntComparer::<F>::compare(Some(&(a as i64)), Some(&b)),
        (Unsigned(a), Unsigned(b)) => {
            UintUintComparer::<F>::compare(Some(&(a as i64)), Some(&(b as i64)))
        }
        _ => unreachable!("integer interval state only contains integers"),
    };
    result.expect("integer comparisons cannot fail") == Some(1)
}
fn less(left: Number, right: Number) -> bool {
    match (left, right) {
        (Number::Real(a), Number::Real(b)) => f64::from_bits(a) < f64::from_bits(b),
        _ => integer_predicate::<CmpOpLt>(left, right),
    }
}
fn put_number(out: &mut Vec<u8>, value: Option<Number>) {
    match value {
        None => out.push(0),
        Some(Number::Signed(v)) => {
            out.push(1);
            put_word(out, v as u64);
        }
        Some(Number::Unsigned(v)) => {
            out.push(2);
            put_word(out, v);
        }
        Some(Number::Real(v)) => {
            out.push(3);
            put_word(out, v);
        }
    }
}
fn read_number(r: &mut Reader<'_>) -> Option<Option<Number>> {
    Some(match r.byte()? {
        0 => None,
        1 => Some(Number::Signed(r.word()? as i64)),
        2 => Some(Number::Unsigned(r.word()?)),
        3 => Some(Number::Real(r.word()?)),
        _ => return None,
    })
}

// Mode 0 is eager-real; modes 1 and 2 are lazy-int and lazy-real. Eager-int
// finishes in the head. Opaque computed state never accepts a host answer.
struct State {
    mode: u8,
    count: usize,
    nullable: bool,
    low: usize,
    high: usize,
    target: Option<Number>,
    nulls: Vec<bool>,
    reals: Vec<u64>,
}
impl State {
    fn request(&self) -> (usize, NativeIntervalCast) {
        if self.mode == 0 {
            return (self.low, NativeIntervalCast::Real);
        }
        let index = if self.target.is_none() {
            0
        } else if self.nullable {
            self.low
        } else {
            self.low + (self.high - self.low) / 2
        };
        (
            index,
            if self.mode == 1 {
                NativeIntervalCast::Int
            } else {
                NativeIntervalCast::Real
            },
        )
    }
}
fn state(bytes: &[u8]) -> Option<State> {
    let mut r = Reader { bytes };
    if r.byte()? != 2 || r.byte()? != 1 {
        return None;
    }
    let mode = r.byte()?;
    if mode > 2 {
        return None;
    }
    let nullable = flag(r.byte()?)?;
    let count = r.index()?;
    if count < 2 || count > isize::MAX as usize {
        return None;
    }
    let low = r.index()?;
    let high = r.index()?;
    let target = read_number(&mut r)?;
    let n = r.count(1)?;
    let mut nulls = Vec::new();
    for _ in 0..n {
        nulls.push(flag(r.byte()?)?);
    }
    let n = r.count(8)?;
    let mut reals = Vec::new();
    for _ in 0..n {
        reals.push(r.word()?);
    }
    if !r.bytes.is_empty() {
        return None;
    }
    if mode == 0 {
        if nulls.len() != count
            || nulls[0]
            || low >= count
            || high != count
            || target.is_some()
            || nulls[low]
            || nullable != nulls.iter().any(|v| *v)
            || reals.len() != nulls[..low].iter().filter(|v| !**v).count()
        {
            return None;
        }
    } else {
        if !nulls.is_empty()
            || !reals.is_empty()
            || low < 1
            || low >= high
            || high > count
            || nullable && high != count
        {
            return None;
        }
        match target {
            None if low != 1 || high != count => return None,
            Some(Number::Real(_)) if mode != 2 => return None,
            Some(Number::Signed(_) | Number::Unsigned(_)) if mode != 1 => return None,
            _ => {}
        }
    }
    Some(State {
        mode,
        count,
        nullable,
        low,
        high,
        target,
        nulls,
        reals,
    })
}
fn state_report(s: &State, limit: usize) -> FrameResult<Option<Vec<u8>>> {
    let mut out = vec![2, 1, s.mode, u8::from(s.nullable)];
    put_index(&mut out, s.count)?;
    put_index(&mut out, s.low)?;
    put_index(&mut out, s.high)?;
    put_number(&mut out, s.target);
    put_index(&mut out, s.nulls.len())?;
    for value in &s.nulls {
        out.push(u8::from(*value));
    }
    put_index(&mut out, s.reals.len())?;
    for value in &s.reals {
        put_word(&mut out, *value);
    }
    fresh(&out, limit).map(Some)
}
fn index_result(index: i64, limit: usize) -> FrameResult<Option<Vec<u8>>> {
    let mut out = [0_u8; 9];
    out[1..].copy_from_slice(&index.to_le_bytes());
    fresh(&out, limit).map(Some)
}
pub fn decode_native_interval_result(bytes: &[u8]) -> Option<NativeIntervalResult<'_>> {
    match bytes.first()? {
        0 if bytes.len() == 9 => {
            let index = i64::from_le_bytes(bytes[1..].try_into().ok()?);
            (index >= -1).then_some(NativeIntervalResult::IntIndex(index))
        }
        1 if bytes.len() == 1 => Some(NativeIntervalResult::SentinelsError),
        2 => {
            let s = state(bytes)?;
            let (index, cast) = s.request();
            Some(NativeIntervalResult::Request {
                index,
                cast,
                state: bytes,
            })
        }
        _ => None,
    }
}
/// Bounds retained output capacity, not temporary decoder/collection peaks.
/// Head state adds at most a fixed header; each eager-real step appends 8
/// bytes.
pub(crate) fn native_interval_output_bound(
    first: Option<&[u8]>,
    second: Option<&[u8]>,
) -> Option<usize> {
    first
        .map_or(0, <[u8]>::len)
        .checked_add(second.map_or(0, <[u8]>::len))?
        .checked_add(256)
}
fn bound(a: Option<&[u8]>, b: Option<&[u8]>) -> FrameResult<usize> {
    native_interval_output_bound(a, b).ok_or(Error::Capacity)
}
pub fn interval_eager_head_native_args_valid(packet: Option<&[u8]>) -> bool {
    packet.is_some_and(|p| eager_head(p).is_some())
        && native_interval_output_bound(packet, None).is_some()
}
pub fn interval_lazy_head_native_args_valid(packet: Option<&[u8]>) -> bool {
    packet.is_some_and(|p| lazy_head(p).is_some())
        && native_interval_output_bound(packet, None).is_some()
}
pub fn interval_step_native_args_valid(bytes: Option<&[u8]>, value: Option<&[u8]>) -> bool {
    let Some(s) = bytes.and_then(state) else {
        return false;
    };
    if native_interval_output_bound(bytes, value).is_none() {
        return false;
    }
    let Some(value) = value else {
        return s.mode != 0;
    };
    match decode_native_identity(value).ok().and_then(number) {
        Some(Number::Signed(_) | Number::Unsigned(_)) => s.mode == 1,
        Some(Number::Real(_)) => s.mode != 1,
        None => false,
    }
}

pub(crate) fn evaluate_interval_eager_head_native(
    packet: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(packet, None)?;
    let values = packet.and_then(eager_head).ok_or(Error::Invalid)?;
    if values
        .iter()
        .any(|value| matches!(value, Some(Identity::MinNotNull | Identity::MaxValue)))
    {
        return fresh(&[1], limit).map(Some);
    }
    if values[0].is_none() {
        return index_result(-1, limit);
    }
    let nullable = values.iter().any(Option::is_none);
    if values
        .iter()
        .all(|value| matches!(value, None | Some(Identity::Int(_) | Identity::UInt(_))))
    {
        let target = number(values[0].unwrap()).ok_or(Error::Invalid)?;
        let index = if nullable {
            values[1..]
                .iter()
                .position(|v| {
                    v.and_then(number)
                        .is_some_and(|boundary| integer_predicate::<CmpOpLt>(target, boundary))
                })
                .unwrap_or(values.len() - 1)
        } else {
            values[1..].partition_point(|v| {
                v.and_then(number)
                    .is_some_and(|boundary| integer_predicate::<CmpOpLe>(boundary, target))
            })
        };
        return index_result(index as i64, limit);
    }
    let s = State {
        mode: 0,
        count: values.len(),
        nullable,
        low: 0,
        high: values.len(),
        target: None,
        nulls: values.iter().map(Option::is_none).collect(),
        reals: Vec::new(),
    };
    state_report(&s, limit)
}
pub(crate) fn evaluate_interval_lazy_head_native(
    packet: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(packet, None)?;
    let types = packet.and_then(lazy_head).ok_or(Error::Invalid)?;
    let all_int = types
        .iter()
        .all(|ft| ft.is_some_and(|ft| ft.eval_type == NativeIntervalEvalType::Int));
    let nullable = types.iter().any(|ft| ft.is_none_or(|ft| ft.flags & 1 == 0));
    state_report(
        &State {
            mode: if all_int { 1 } else { 2 },
            count: types.len(),
            nullable,
            low: 1,
            high: types.len(),
            target: None,
            nulls: Vec::new(),
            reals: Vec::new(),
        },
        limit,
    )
}
pub(crate) fn evaluate_interval_step_native(
    bytes: Option<&[u8]>,
    value: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(bytes, value)?;
    if !interval_step_native_args_valid(bytes, value) {
        return Err(Error::Invalid);
    }
    let mut s = state(bytes.unwrap()).ok_or(Error::Invalid)?;
    let value = value
        .map(decode_native_identity)
        .transpose()?
        .and_then(number);
    if s.mode == 0 {
        let Some(Number::Real(bits)) = value else {
            return Err(Error::Invalid);
        };
        s.reals.push(bits);
        s.low += 1;
        while s.low < s.count && s.nulls[s.low] {
            s.low += 1;
        }
        if s.low < s.count {
            return state_report(&s, limit);
        }
        let target = f64::from_bits(s.reals[0]);
        let mut real = s.reals[1..].iter();
        let boundaries: Vec<Option<f64>> = s.nulls[1..]
            .iter()
            .map(|null| {
                if *null {
                    None
                } else {
                    Some(f64::from_bits(
                        *real
                            .next()
                            .expect("one actual conversion per non-NULL boundary"),
                    ))
                }
            })
            .collect();
        let index = if s.nullable {
            boundaries
                .iter()
                .position(|boundary| boundary.is_some_and(|value| target < value))
                .unwrap_or(s.count - 1)
        } else {
            // This is intentionally NOT !(target < boundary): NaN differs.
            boundaries.partition_point(|boundary| boundary.is_some_and(|value| value <= target))
        };
        return index_result(index as i64, limit);
    }
    let Some(target) = s.target else {
        let Some(target) = value else {
            return index_result(-1, limit);
        };
        s.target = Some(target);
        return state_report(&s, limit);
    };
    let (index, _) = s.request();
    let before = value.is_some_and(|boundary| less(target, boundary));
    if s.nullable {
        if before {
            return index_result((index - 1) as i64, limit);
        }
        s.low += 1;
    } else if before {
        s.high = index;
    } else {
        s.low = index + 1;
    }
    if s.low >= s.high {
        index_result((s.low - 1) as i64, limit)
    } else {
        state_report(&s, limit)
    }
}

fn kernel(value: FrameResult<Option<Vec<u8>>>) -> Result<Option<Bytes>> {
    value.map_err(|e| other_err!("Invalid native INTERVAL transport: {:?}", e))
}
#[rpn_fn(nullable)]
fn interval_eager_head_native(packet: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_interval_eager_head_native(packet))
}
#[rpn_fn(nullable)]
fn interval_lazy_head_native(packet: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_interval_lazy_head_native(packet))
}
#[rpn_fn(nullable)]
fn interval_step_native(state: Option<BytesRef>, value: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_interval_step_native(state, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode_native_identity;

    #[test]
    fn interval_retains_eager_precedence_all_casts_and_distinct_lazy_nan_search() {
        let frame = |value| encode_native_identity(value).unwrap();
        let eager = |values: &[Option<&[u8]>]| {
            let packet = encode_native_interval_eager_head(values).unwrap();
            let out = evaluate_interval_eager_head_native(Some(&packet))
                .unwrap()
                .unwrap();
            assert!(out.capacity() <= native_interval_output_bound(Some(&packet), None).unwrap());
            out
        };
        let lazy = |types: &[Option<NativeIntervalFieldType>]| {
            let packet = encode_native_interval_lazy_head(types).unwrap();
            evaluate_interval_lazy_head_native(Some(&packet))
                .unwrap()
                .unwrap()
        };
        let step = |state: &[u8], value: Option<&[u8]>| {
            let out = evaluate_interval_step_native(Some(state), value)
                .unwrap()
                .unwrap();
            assert!(out.capacity() <= native_interval_output_bound(Some(state), value).unwrap());
            out
        };
        let request = |state: &[u8], index, cast| {
            assert_eq!(
                decode_native_interval_result(state),
                Some(NativeIntervalResult::Request { index, cast, state })
            );
        };
        let signed = frame(Identity::Int(-1));
        let zero = frame(Identity::Int(0));
        let maximum = frame(Identity::UInt(u64::MAX));
        let sentinel = frame(Identity::MaxValue);
        assert_eq!(
            decode_native_interval_result(&eager(&[None, Some(&sentinel)])),
            Some(NativeIntervalResult::SentinelsError)
        );
        assert_eq!(
            decode_native_interval_result(&eager(&[None, Some(&zero)])),
            Some(NativeIntervalResult::IntIndex(-1))
        );
        assert_eq!(
            decode_native_interval_result(&eager(&[Some(&signed), Some(&zero), Some(&maximum)])),
            Some(NativeIntervalResult::IntIndex(0))
        );
        assert_eq!(
            decode_native_interval_result(&eager(&[Some(&maximum), Some(&signed), Some(&maximum)])),
            Some(NativeIntervalResult::IntIndex(2))
        );
        assert_eq!(
            decode_native_interval_result(&eager(&[
                Some(&zero),
                None,
                Some(&signed),
                Some(&maximum)
            ])),
            Some(NativeIntervalResult::IntIndex(2))
        );

        let nan = frame(Identity::Real(f64::NAN.to_bits()));
        let one = frame(Identity::Real(1.0_f64.to_bits()));
        let two = frame(Identity::Real(2.0_f64.to_bits()));
        let mut s = eager(&[Some(&nan), Some(&one), Some(&two)]);
        request(&s, 0, NativeIntervalCast::Real);
        s = step(&s, Some(&nan));
        request(&s, 1, NativeIntervalCast::Real);
        s = step(&s, Some(&one));
        request(&s, 2, NativeIntervalCast::Real);
        s = step(&s, Some(&two));
        assert_eq!(
            decode_native_interval_result(&s),
            Some(NativeIntervalResult::IntIndex(0))
        );
        let real = Some(NativeIntervalFieldType {
            eval_type: NativeIntervalEvalType::Real,
            flags: 1,
        });
        let mut s = lazy(&[real; 3]);
        request(&s, 0, NativeIntervalCast::Real);
        s = step(&s, Some(&nan));
        request(&s, 2, NativeIntervalCast::Real);
        s = step(&s, Some(&two));
        assert_eq!(
            decode_native_interval_result(&s),
            Some(NativeIntervalResult::IntIndex(2))
        );
        // NaN boundary: eager <= is false (left), lazy target< is false (right).
        let mut s = eager(&[Some(&one), Some(&nan)]);
        s = step(&s, Some(&one));
        s = step(&s, Some(&nan));
        assert_eq!(
            decode_native_interval_result(&s),
            Some(NativeIntervalResult::IntIndex(0))
        );
        let mut s = lazy(&[real; 2]);
        s = step(&s, Some(&one));
        s = step(&s, Some(&nan));
        assert_eq!(
            decode_native_interval_result(&s),
            Some(NativeIntervalResult::IntIndex(1))
        );

        // Even when the first boundary would decide the answer, eager REAL
        // still requests a later raw operand whose native cast will fail.
        let raw = frame(Identity::Raw(b"not-real"));
        let mut s = eager(&[Some(&one), Some(&two), Some(&raw)]);
        s = step(&s, Some(&one));
        s = step(&s, Some(&two));
        request(&s, 2, NativeIntervalCast::Real);
        assert!(!interval_step_native_args_valid(Some(&s), Some(&raw)));
        let mut long_null_run = vec![Some(one.as_slice())];
        long_null_run.extend(std::iter::repeat_n(None, 1000));
        long_null_run.push(Some(two.as_slice()));
        let s = eager(&long_null_run);
        let s = step(&s, Some(&one));
        request(&s, 1001, NativeIntervalCast::Real);
        let output = step(&s, Some(&two));
        assert_eq!(
            decode_native_interval_result(&output),
            Some(NativeIntervalResult::IntIndex(1000))
        );
        let mut s = eager(&[Some(&one), None, Some(&two)]);
        s = step(&s, Some(&one));
        request(&s, 2, NativeIntervalCast::Real);
        s = step(&s, Some(&two));
        assert_eq!(
            decode_native_interval_result(&s),
            Some(NativeIntervalResult::IntIndex(1))
        );

        let int = Some(NativeIntervalFieldType {
            eval_type: NativeIntervalEvalType::Int,
            flags: 1 | 32 | 0x8000_0000,
        });
        let mut s = lazy(&[int; 4]);
        request(&s, 0, NativeIntervalCast::Int);
        s = step(&s, Some(&signed));
        request(&s, 2, NativeIntervalCast::Int);
        s = step(&s, Some(&maximum));
        request(&s, 1, NativeIntervalCast::Int);
        s = step(&s, Some(&zero));
        assert_eq!(
            decode_native_interval_result(&s),
            Some(NativeIntervalResult::IntIndex(0))
        );
        let s = lazy(&[int; 2]);
        assert_eq!(
            decode_native_interval_result(&step(&s, None)),
            Some(NativeIntervalResult::IntIndex(-1))
        );
        let mut s = lazy(&[int; 2]);
        s = step(&s, Some(&zero));
        s = step(&s, None);
        assert_eq!(
            decode_native_interval_result(&s),
            Some(NativeIntervalResult::IntIndex(1))
        );
        let nullable = Some(NativeIntervalFieldType {
            eval_type: NativeIntervalEvalType::Int,
            flags: 0,
        });
        let mut s = lazy(&[int, nullable, int, int]);
        s = step(&s, Some(&signed));
        request(&s, 1, NativeIntervalCast::Int);
        s = step(&s, None);
        request(&s, 2, NativeIntervalCast::Int);
        s = step(&s, Some(&zero));
        assert_eq!(
            decode_native_interval_result(&s),
            Some(NativeIntervalResult::IntIndex(1))
        );
        let s = lazy(&[int, None, int]);
        request(&s, 0, NativeIntervalCast::Real);
        let s = step(&s, Some(&one));
        request(&s, 1, NativeIntervalCast::Real);
        let s = step(&s, Some(&two));
        assert_eq!(
            decode_native_interval_result(&s),
            Some(NativeIntervalResult::IntIndex(0))
        );
        assert!(decode_native_interval_result(&[1, 0]).is_none());
        let mut trailing = lazy(&[int; 2]);
        trailing.push(0);
        assert!(decode_native_interval_result(&trailing).is_none());
        let packet = encode_native_interval_eager_head(&[Some(&zero)]).unwrap();
        assert!(!interval_eager_head_native_args_valid(Some(&packet)));
    }
}
