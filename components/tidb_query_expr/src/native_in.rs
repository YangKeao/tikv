// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Partial native IN takeover: already-evaluated/already-cast temporal and
//! JSON lists, plus the two legacy lazy readers. AST, generic/hash-cache IN,
//! values-only IN and the existing wire IN policies are deliberately separate.
use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::codec::{
    collation::native::NativeCollation,
    data_type::{Bytes, BytesRef, ChunkedVec, EvaluableRef, EvaluableRet},
    mysql::{Time, json::compare_native_binary_json},
};

use crate::{
    NativeIdentityFrameError as Error, NativeIdentityRef as Identity, decode_native_identity,
};
type FrameResult<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NativeInTypedDomain {
    Datetime,
    Timestamp,
    Duration,
    Json,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeInRequest {
    Int128 { index: usize },
    Bytes { index: usize },
    Collation { collation_id: i32 },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeInResult<'a> {
    Null,
    Bool(bool),
    Request {
        kind: NativeInRequest,
        state: &'a [u8],
    },
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
    fn count(&mut self) -> Option<usize> {
        usize::try_from(u64::from_le_bytes(self.take(8)?.try_into().ok()?)).ok()
    }
    fn blob(&mut self) -> Option<&'a [u8]> {
        let n = self.count()?;
        self.take(n)
    }
    fn optional(&mut self) -> Option<Option<&'a [u8]>> {
        Some(match self.byte()? {
            0 => None,
            1 => Some(self.blob()?),
            _ => return None,
        })
    }
}
fn put_count(out: &mut Vec<u8>, n: usize) -> FrameResult<()> {
    out.extend_from_slice(&u64::try_from(n).map_err(|_| Error::Capacity)?.to_le_bytes());
    Ok(())
}
fn optional(out: &mut Vec<u8>, value: Option<&[u8]>) -> FrameResult<()> {
    out.push(u8::from(value.is_some()));
    if let Some(value) = value {
        put_count(out, value.len())?;
        out.extend_from_slice(value);
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
fn identity(value: Option<&[u8]>) -> Option<Option<Identity<'_>>> {
    value.map(decode_native_identity).transpose().ok()
}
fn typed_header(bytes: &[u8]) -> Option<(NativeInTypedDomain, usize, Reader<'_>)> {
    let mut r = Reader { bytes };
    if r.byte()? != 1 {
        return None;
    }
    let domain = match r.byte()? {
        0 => NativeInTypedDomain::Datetime,
        1 => NativeInTypedDomain::Timestamp,
        2 => NativeInTypedDomain::Duration,
        3 => NativeInTypedDomain::Json,
        _ => return None,
    };
    let count = r.count()?;
    if count < 2 {
        return None;
    }
    Some((domain, count, r))
}
pub fn encode_native_in_typed_values(
    domain: NativeInTypedDomain,
    values: &[Option<&[u8]>],
) -> FrameResult<Vec<u8>> {
    if values.len() < 2 {
        return Err(Error::Invalid);
    }
    let mut out = vec![1, domain as u8];
    put_count(&mut out, values.len())?;
    for value in values {
        identity(*value).ok_or(Error::Invalid)?;
        optional(&mut out, *value)?;
    }
    let size = out.len();
    fresh(&out, size)
}
pub fn encode_native_in_legacy_int_head(count: usize) -> FrameResult<Vec<u8>> {
    let mut out = vec![1];
    put_count(&mut out, count)?;
    fresh(&out, 32)
}
pub fn encode_native_in_legacy_string_head(
    count: usize,
    collation_id: i32,
) -> FrameResult<Vec<u8>> {
    let mut out = vec![1];
    put_count(&mut out, count)?;
    out.extend_from_slice(&collation_id.to_le_bytes());
    fresh(&out, 32)
}
fn head_metadata(bytes: &[u8], string: bool) -> Option<(usize, Option<i32>)> {
    let mut r = Reader { bytes };
    if r.byte()? != 1 {
        return None;
    }
    let count = r.count()?;
    let collation = if string {
        Some(i32::from_le_bytes(r.take(4)?.try_into().ok()?))
    } else {
        None
    };
    r.bytes.is_empty().then_some((count, collation))
}
#[derive(Clone, Copy)]
struct State<'a> {
    count: usize,
    next: usize,
    collation: Option<i32>,
    phase: u8,
    base: Option<&'a [u8]>,
    candidate: Option<&'a [u8]>,
    saw_null: bool,
}
// Phase 0 asks for index zero even for a missing child; phase 1 reads a
// candidate; phase 2 requests the original collator resolver on a non-NULL
// pair.
fn state(bytes: &[u8]) -> Option<State<'_>> {
    let mut r = Reader { bytes };
    if r.byte()? != 2 || r.byte()? != 1 {
        return None;
    }
    let string = match r.byte()? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let phase = r.byte()?;
    if phase > 2 {
        return None;
    }
    let count = r.count()?;
    let next = r.count()?;
    let collation = if string {
        Some(i32::from_le_bytes(r.take(4)?.try_into().ok()?))
    } else {
        None
    };
    let saw_null = match r.byte()? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let base = r.optional()?;
    let candidate = r.optional()?;
    if !r.bytes.is_empty() {
        return None;
    }
    if !string && (base.is_some_and(|v| v.len() != 16) || candidate.is_some()) {
        return None;
    }
    match phase {
        0 if next != 0 || base.is_some() || candidate.is_some() || saw_null => return None,
        1 if next == 0 || next >= count || candidate.is_some() || base.is_none() && !saw_null => {
            return None;
        }
        2 if !string || next == 0 || next >= count || base.is_none() || candidate.is_none() => {
            return None;
        }
        _ => {}
    }
    Some(State {
        count,
        next,
        collation,
        phase,
        base,
        candidate,
        saw_null,
    })
}
fn request(s: State<'_>, limit: usize) -> FrameResult<Option<Vec<u8>>> {
    let mut out = vec![2, 1, u8::from(s.collation.is_some()), s.phase];
    put_count(&mut out, s.count)?;
    put_count(&mut out, s.next)?;
    if let Some(id) = s.collation {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out.push(u8::from(s.saw_null));
    optional(&mut out, s.base)?;
    optional(&mut out, s.candidate)?;
    fresh(&out, limit).map(Some)
}
fn terminal(value: Option<bool>, limit: usize) -> FrameResult<Option<Vec<u8>>> {
    match value {
        None => fresh(&[0], limit).map(Some),
        Some(v) => fresh(&[1, u8::from(v)], limit).map(Some),
    }
}
pub fn decode_native_in_result(bytes: &[u8]) -> Option<NativeInResult<'_>> {
    match bytes {
        [0] => Some(NativeInResult::Null),
        [1, 0] => Some(NativeInResult::Bool(false)),
        [1, 1] => Some(NativeInResult::Bool(true)),
        _ => {
            let s = state(bytes)?;
            let kind = if s.phase == 2 {
                NativeInRequest::Collation {
                    collation_id: s.collation?,
                }
            } else if s.collation.is_some() {
                NativeInRequest::Bytes { index: s.next }
            } else {
                NativeInRequest::Int128 { index: s.next }
            };
            Some(NativeInResult::Request { kind, state: bytes })
        }
    }
}
/// Bounds retained replies only: a state can retain the old actual base and
/// the new actual candidate while asking for the original collator resolver.
pub(crate) fn native_in_output_bound(first: Option<&[u8]>, second: Option<&[u8]>) -> Option<usize> {
    first
        .map_or(0, <[u8]>::len)
        .checked_add(second.map_or(0, <[u8]>::len))?
        .checked_add(128)
}
fn bound(a: Option<&[u8]>, b: Option<&[u8]>) -> FrameResult<usize> {
    native_in_output_bound(a, b).ok_or(Error::Capacity)
}
pub fn in_typed_values_native_args_valid(value: Option<&[u8]>) -> bool {
    let Some((_, count, mut r)) = value.and_then(typed_header) else {
        return false;
    };
    for _ in 0..count {
        let Some(v) = r.optional() else {
            return false;
        };
        if identity(v).is_none() {
            return false;
        }
    }
    r.bytes.is_empty() && native_in_output_bound(value, None).is_some()
}
pub fn in_legacy_int_head_native_args_valid(value: Option<&[u8]>) -> bool {
    value.and_then(|v| head_metadata(v, false)).is_some()
        && native_in_output_bound(value, None).is_some()
}
pub fn in_legacy_string_head_native_args_valid(value: Option<&[u8]>) -> bool {
    value.and_then(|v| head_metadata(v, true)).is_some()
        && native_in_output_bound(value, None).is_some()
}
fn collation_reply(value: Option<&[u8]>) -> Option<NativeCollation> {
    NativeCollation::from_tag(i64::from_le_bytes(value?.try_into().ok()?))
}
pub fn in_legacy_step_native_args_valid(raw: Option<&[u8]>, value: Option<&[u8]>) -> bool {
    let Some(s) = raw.and_then(state) else {
        return false;
    };
    let valid = if s.phase == 2 {
        collation_reply(value).is_some()
    } else {
        s.collation.is_some() || value.is_none_or(|v| v.len() == 16)
    };
    valid && native_in_output_bound(raw, value).is_some()
}
fn equal(domain: NativeInTypedDomain, left: Identity<'_>, right: Identity<'_>) -> bool {
    match (domain, left, right) {
        (
            NativeInTypedDomain::Datetime | NativeInTypedDomain::Timestamp,
            Identity::Time { core: left, .. },
            Identity::Time { core: right, .. },
        ) => Time::native_core_compare(left, right).is_eq(),
        (
            NativeInTypedDomain::Duration,
            Identity::Duration { nanos: left, .. },
            Identity::Duration { nanos: right, .. },
        ) => left == right,
        (
            NativeInTypedDomain::Json,
            Identity::Json {
                type_code: left_type,
                bytes: left,
            },
            Identity::Json {
                type_code: right_type,
                bytes: right,
            },
        ) => compare_native_binary_json(left_type, left, right_type, right).is_eq(),
        _ => false,
    }
}
pub(crate) fn evaluate_in_typed_values_native(
    value: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(value, None)?;
    if !in_typed_values_native_args_valid(value) {
        return Err(Error::Invalid);
    }
    let (domain, count, mut r) = typed_header(value.unwrap()).ok_or(Error::Invalid)?;
    let first = identity(r.optional().ok_or(Error::Invalid)?).ok_or(Error::Invalid)?;
    let Some(first) = first else {
        return terminal(None, limit);
    };
    let mut saw_null = false;
    for _ in 1..count {
        match identity(r.optional().ok_or(Error::Invalid)?).ok_or(Error::Invalid)? {
            None => saw_null = true,
            Some(candidate) if equal(domain, first, candidate) => {
                return terminal(Some(true), limit);
            }
            _ => {}
        }
    }
    terminal(if saw_null { None } else { Some(false) }, limit)
}
fn head(value: Option<&[u8]>, string: bool) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(value, None)?;
    let (count, collation) = value
        .and_then(|v| head_metadata(v, string))
        .ok_or(Error::Invalid)?;
    request(
        State {
            count,
            next: 0,
            collation,
            phase: 0,
            base: None,
            candidate: None,
            saw_null: false,
        },
        limit,
    )
}
pub(crate) fn evaluate_in_legacy_int_head_native(
    value: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    head(value, false)
}
pub(crate) fn evaluate_in_legacy_string_head_native(
    value: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    head(value, true)
}
fn advance(mut s: State<'_>, limit: usize) -> FrameResult<Option<Vec<u8>>> {
    s.next = s.next.checked_add(1).ok_or(Error::Invalid)?;
    s.phase = 1;
    s.candidate = None;
    if s.next >= s.count {
        terminal(if s.saw_null { None } else { Some(false) }, limit)
    } else {
        request(s, limit)
    }
}
pub(crate) fn evaluate_in_legacy_step_native<'a>(
    raw: Option<&'a [u8]>,
    value: Option<&'a [u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(raw, value)?;
    if !in_legacy_step_native_args_valid(raw, value) {
        return Err(Error::Invalid);
    }
    let s = state(raw.unwrap()).ok_or(Error::Invalid)?;
    if s.phase == 0 {
        return advance(
            State {
                base: value,
                saw_null: value.is_none(),
                ..s
            },
            limit,
        );
    }
    if s.phase == 2 {
        let collator = collation_reply(value).ok_or(Error::Invalid)?;
        if collator
            .compare(
                s.base.ok_or(Error::Invalid)?,
                s.candidate.ok_or(Error::Invalid)?,
            )
            .expect("raw supported collation comparison cannot fail")
            .is_eq()
        {
            return terminal(Some(true), limit);
        }
        return advance(s, limit);
    }
    match (s.base, value) {
        (Some(_), Some(right)) if s.collation.is_some() => request(
            State {
                candidate: Some(right),
                phase: 2,
                ..s
            },
            limit,
        ),
        (Some(left), Some(right)) => {
            let left = i128::from_le_bytes(left.try_into().map_err(|_| Error::Invalid)?);
            let right = i128::from_le_bytes(right.try_into().map_err(|_| Error::Invalid)?);
            if left == right {
                terminal(Some(true), limit)
            } else {
                advance(s, limit)
            }
        }
        _ => advance(
            State {
                saw_null: true,
                ..s
            },
            limit,
        ),
    }
}
fn kernel(value: FrameResult<Option<Vec<u8>>>) -> Result<Option<Bytes>> {
    value.map_err(|e| other_err!("Invalid native IN transport: {:?}", e))
}
#[rpn_fn(nullable)]
fn in_typed_values_native(value: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_in_typed_values_native(value))
}
#[rpn_fn(nullable)]
fn in_legacy_int_head_native(value: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_in_legacy_int_head_native(value))
}
#[rpn_fn(nullable)]
fn in_legacy_string_head_native(value: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_in_legacy_string_head_native(value))
}
#[rpn_fn(nullable)]
fn in_legacy_step_native(raw: Option<BytesRef>, value: Option<BytesRef>) -> Result<Option<Bytes>> {
    kernel(evaluate_in_legacy_step_native(raw, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode_native_identity;
    #[test]
    fn typed_and_legacy_membership_keep_raw_domains_null_demand_and_late_collator() {
        let frame = |value| encode_native_identity(value).unwrap();
        let typed = |domain, values: &[Option<&[u8]>]| {
            let input = encode_native_in_typed_values(domain, values).unwrap();
            let out = evaluate_in_typed_values_native(Some(&input))
                .unwrap()
                .unwrap();
            assert!(out.capacity() <= native_in_output_bound(Some(&input), None).unwrap());
            out
        };
        let t1 = frame(Identity::Time {
            core: 0,
            kind: 0,
            fsp: 0,
        });
        let t2 = frame(Identity::Time {
            core: 0,
            kind: 2,
            fsp: 255,
        });
        for domain in [
            NativeInTypedDomain::Datetime,
            NativeInTypedDomain::Timestamp,
        ] {
            let out = typed(domain, &[Some(&t1), None, Some(&t2)]);
            assert_eq!(
                decode_native_in_result(&out),
                Some(NativeInResult::Bool(true))
            );
        }
        let d1 = frame(Identity::Duration {
            nanos: 1,
            fsp: i64::MIN,
        });
        let d2 = frame(Identity::Duration {
            nanos: 1,
            fsp: i64::MAX,
        });
        let different = frame(Identity::Duration { nanos: 2, fsp: 0 });
        let out = typed(NativeInTypedDomain::Duration, &[Some(&d1), Some(&d2)]);
        assert_eq!(
            decode_native_in_result(&out),
            Some(NativeInResult::Bool(true))
        );
        let out = typed(
            NativeInTypedDomain::Duration,
            &[Some(&d1), Some(&different), None],
        );
        assert_eq!(decode_native_in_result(&out), Some(NativeInResult::Null));
        let out = typed(NativeInTypedDomain::Duration, &[None, Some(&d1)]);
        assert_eq!(decode_native_in_result(&out), Some(NativeInResult::Null));
        // The existing native JSON comparison accepts these malformed array
        // payloads as equal. IN must not impose a new JSON parser/validator.
        let json1 = frame(Identity::Json {
            type_code: 0x03,
            bytes: &[],
        });
        let json2 = frame(Identity::Json {
            type_code: 0x03,
            bytes: &[255],
        });
        let out = typed(NativeInTypedDomain::Json, &[Some(&json1), Some(&json2)]);
        assert_eq!(
            decode_native_in_result(&out),
            Some(NativeInResult::Bool(true))
        );
        assert!(encode_native_in_typed_values(NativeInTypedDomain::Json, &[]).is_err());
        let step = |s: &[u8], v: Option<&[u8]>| {
            let out = evaluate_in_legacy_step_native(Some(s), v).unwrap().unwrap();
            assert!(out.capacity() <= native_in_output_bound(Some(s), v).unwrap());
            out
        };
        let request = |s: &[u8], kind| {
            assert_eq!(
                decode_native_in_result(s),
                Some(NativeInResult::Request { kind, state: s })
            )
        };
        let int_head = |count| {
            let m = encode_native_in_legacy_int_head(count).unwrap();
            evaluate_in_legacy_int_head_native(Some(&m))
                .unwrap()
                .unwrap()
        };
        let h = int_head(0);
        request(&h, NativeInRequest::Int128 { index: 0 });
        let out = step(&h, None);
        assert_eq!(decode_native_in_result(&out), Some(NativeInResult::Null));
        let h = int_head(1);
        let out = step(&h, Some(&0_i128.to_le_bytes()));
        assert_eq!(
            decode_native_in_result(&out),
            Some(NativeInResult::Bool(false))
        );
        let h = int_head(4);
        let s = step(&h, None);
        request(&s, NativeInRequest::Int128 { index: 1 });
        let s = step(&s, Some(&1_i128.to_le_bytes()));
        request(&s, NativeInRequest::Int128 { index: 2 });
        let s = step(&s, None);
        request(&s, NativeInRequest::Int128 { index: 3 });
        let out = step(&s, Some(&2_i128.to_le_bytes()));
        assert_eq!(decode_native_in_result(&out), Some(NativeInResult::Null));
        let h = int_head(5);
        let s = step(&h, Some(&i128::MAX.to_le_bytes()));
        let s = step(&s, None);
        // A 64-bit reply is never accepted as the original i128 getter result.
        assert!(!in_legacy_step_native_args_valid(
            Some(&s),
            Some(&u64::MAX.to_le_bytes())
        ));
        let s = step(&s, Some(&i128::from(u64::MAX).to_le_bytes()));
        request(&s, NativeInRequest::Int128 { index: 3 });
        let out = step(&s, Some(&i128::MAX.to_le_bytes()));
        assert_eq!(
            decode_native_in_result(&out),
            Some(NativeInResult::Bool(true))
        ); // no index 4 request
        let metadata = encode_native_in_legacy_string_head(3, 46).unwrap();
        let h = evaluate_in_legacy_string_head_native(Some(&metadata))
            .unwrap()
            .unwrap();
        request(&h, NativeInRequest::Bytes { index: 0 });
        let s = step(&h, Some(b"a "));
        let s = step(&s, Some(b"a"));
        request(&s, NativeInRequest::Collation { collation_id: 46 });
        let s = step(&s, Some(&NativeCollation::Binary.tag().to_le_bytes()));
        request(&s, NativeInRequest::Bytes { index: 2 });
        let s = step(&s, Some(b"a"));
        request(&s, NativeInRequest::Collation { collation_id: 46 });
        let out = step(&s, Some(&NativeCollation::Utf8Mb4Bin.tag().to_le_bytes()));
        assert_eq!(
            decode_native_in_result(&out),
            Some(NativeInResult::Bool(true))
        ); // resolver changed between actual pairs
        let s = step(&h, None);
        let s = step(&s, Some(b"a"));
        request(&s, NativeInRequest::Bytes { index: 2 });
        let out = step(&s, None);
        assert_eq!(decode_native_in_result(&out), Some(NativeInResult::Null)); // no collator for NULL pairs
        let metadata = encode_native_in_legacy_string_head(1, i32::MIN).unwrap();
        let h = evaluate_in_legacy_string_head_native(Some(&metadata))
            .unwrap()
            .unwrap();
        let out = step(&h, Some(b""));
        assert_eq!(
            decode_native_in_result(&out),
            Some(NativeInResult::Bool(false))
        );
        let mut broken = h;
        broken.push(0);
        assert!(decode_native_in_result(&broken).is_none());
    }
}
