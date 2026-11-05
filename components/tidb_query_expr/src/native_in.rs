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
/// Actual already-cast values used by temporal/JSON membership. Other records
/// a non-NULL source kind outside these domains; it never compares equal here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeInTypedValue<'a> {
    /// Raw native time core; source kind/FSP remain outside comparison policy.
    Time(u64),
    /// Actual signed nanoseconds, independent of display FSP.
    Duration(i64),
    /// Native binary JSON identity, borrowed without text parsing or transport.
    Json {
        /// Actual native binary JSON type byte.
        type_code: u8,
        /// Actual native binary JSON payload without the type byte.
        bytes: &'a [u8],
    },
    /// An actual non-NULL datum belonging to another native domain.
    Other,
}
/// Typed value membership requires a real left value and at least one
/// candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeInTypedArityError {
    /// Number of supplied already-evaluated values.
    pub actual: usize,
}
fn typed_view(value: Identity<'_>) -> NativeInTypedValue<'_> {
    match value {
        Identity::Time { core, .. } => NativeInTypedValue::Time(core),
        Identity::Duration { nanos, .. } => NativeInTypedValue::Duration(nanos),
        Identity::Json { type_code, bytes } => NativeInTypedValue::Json { type_code, bytes },
        _ => NativeInTypedValue::Other,
    }
}
fn equal_typed(
    domain: NativeInTypedDomain,
    left: NativeInTypedValue<'_>,
    right: NativeInTypedValue<'_>,
) -> bool {
    match (domain, left, right) {
        (
            NativeInTypedDomain::Datetime | NativeInTypedDomain::Timestamp,
            NativeInTypedValue::Time(left),
            NativeInTypedValue::Time(right),
        ) => Time::native_core_compare(left, right).is_eq(),
        (
            NativeInTypedDomain::Duration,
            NativeInTypedValue::Duration(left),
            NativeInTypedValue::Duration(right),
        ) => left == right,
        (
            NativeInTypedDomain::Json,
            NativeInTypedValue::Json {
                type_code: left_type,
                bytes: left,
            },
            NativeInTypedValue::Json {
                type_code: right_type,
                bytes: right,
            },
        ) => compare_native_binary_json(left_type, left, right_type, right).is_eq(),
        _ => false,
    }
}
/// Computes only already-provided, already-cast views. Callers must complete
/// their original operand evaluation/cast barriers before constructing views;
/// the iterator must not evaluate expressions or repeat source coercion.
/// No frame, input copy, output report or work buffer is allocated here.
pub fn native_in_typed_values<'a>(
    domain: NativeInTypedDomain,
    mut values: impl ExactSizeIterator<Item = Option<NativeInTypedValue<'a>>>,
) -> std::result::Result<Option<bool>, NativeInTypedArityError> {
    let count = values.len();
    if count < 2 {
        return Err(NativeInTypedArityError { actual: count });
    }
    let Some(first) = values
        .next()
        .expect("exact-size typed values retain their first item")
    else {
        return Ok(None);
    };
    let mut saw_null = false;
    for candidate in values {
        match candidate {
            None => saw_null = true,
            Some(candidate) if equal_typed(domain, first, candidate) => return Ok(Some(true)),
            _ => {}
        }
    }
    Ok(if saw_null { None } else { Some(false) })
}

/// A demanded legacy source error remains its original generic type. Only
/// explicit finite retained/comparison allowances create ResourceLimit.
#[derive(Debug)]
pub enum NativeInLegacyError<E> {
    /// The exact error returned by the original integer/byte channel.
    Source(E),
    /// A logical allowance failed, independently of source SQL/error contents.
    ResourceLimit(&'static str),
    /// A staged consumer resumed a closed or unstarted value owner.
    InvalidResponse,
}
/// An actual source request or completed nullable membership value. This owns
/// no expression, context, invocation or borrowed input representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeInLegacyValueStep {
    Source { index: usize },
    Ready(Option<bool>),
}

struct LegacyValuesState<T> {
    count: usize,
    next: usize,
    base: Option<T>,
    saw_null: bool,
    started: bool,
    live: bool,
}
impl<T> LegacyValuesState<T> {
    fn new(count: usize) -> Self {
        Self {
            count,
            next: 0,
            base: None,
            saw_null: false,
            started: false,
            live: true,
        }
    }
    fn begin<E>(&mut self) -> std::result::Result<NativeInLegacyValueStep, NativeInLegacyError<E>> {
        if self.started || !self.live {
            self.live = false;
            return Err(NativeInLegacyError::InvalidResponse);
        }
        self.started = true;
        Ok(NativeInLegacyValueStep::Source { index: 0 })
    }
    fn resume<E>(
        &mut self,
        value: Option<T>,
        mut inspect: impl FnMut(
            Option<&T>,
            Option<&T>,
        ) -> std::result::Result<(), NativeInLegacyError<E>>,
        mut equal: impl FnMut(&T, &T) -> std::result::Result<bool, NativeInLegacyError<E>>,
    ) -> std::result::Result<NativeInLegacyValueStep, NativeInLegacyError<E>> {
        if !self.started || !self.live {
            self.live = false;
            return Err(NativeInLegacyError::InvalidResponse);
        }
        // Failed or unwound advancement stays closed; only a new source
        // request publishes a live continuation again.
        self.live = false;
        if self.next == 0 {
            inspect(None, value.as_ref())?;
            self.saw_null = value.is_none();
            self.base = value;
        } else {
            inspect(self.base.as_ref(), value.as_ref())?;
            match (self.base.as_ref(), value.as_ref()) {
                (Some(left), Some(right)) if equal(left, right)? => {
                    return Ok(NativeInLegacyValueStep::Ready(Some(true)));
                }
                (Some(_), Some(_)) => {}
                _ => self.saw_null = true,
            }
        }
        self.next = self
            .next
            .checked_add(1)
            .ok_or(NativeInLegacyError::InvalidResponse)?;
        if self.next >= self.count {
            Ok(NativeInLegacyValueStep::Ready(legacy_no_match(
                self.saw_null,
            )))
        } else {
            self.live = true;
            Ok(NativeInLegacyValueStep::Source { index: self.next })
        }
    }
}

/// Full-i128 legacy IN continuation over actual typed source results.
pub struct NativeInLegacyIntValues {
    state: LegacyValuesState<i128>,
}
impl NativeInLegacyIntValues {
    pub fn new(count: usize) -> Self {
        Self {
            state: LegacyValuesState::new(count),
        }
    }
    pub fn begin(
        &mut self,
    ) -> std::result::Result<NativeInLegacyValueStep, NativeInLegacyError<std::convert::Infallible>>
    {
        self.state.begin()
    }
    pub fn resume_source(
        &mut self,
        value: Option<i128>,
    ) -> std::result::Result<NativeInLegacyValueStep, NativeInLegacyError<std::convert::Infallible>>
    {
        self.state.resume(
            value,
            |_, _| Ok(()),
            |left, right| Ok(legacy_int_equal(*left, *right)),
        )
    }
}

/// Owned original byte vectors with the same finite retained/comparison policy
/// as the synchronous source entry. Collation resolves after a real pair only.
pub struct NativeInLegacyBytesValues {
    state: LegacyValuesState<Vec<u8>>,
    maximum: usize,
}
impl NativeInLegacyBytesValues {
    pub fn new(
        count: usize,
        maximum: usize,
    ) -> std::result::Result<Self, NativeInLegacyError<std::convert::Infallible>> {
        Ok(Self {
            state: LegacyValuesState::new(count),
            maximum,
        })
    }
    pub fn begin(
        &mut self,
    ) -> std::result::Result<NativeInLegacyValueStep, NativeInLegacyError<std::convert::Infallible>>
    {
        self.state.begin()
    }
    pub fn retained_bytes(&self) -> usize {
        self.state.base.as_ref().map_or(0, Vec::capacity)
    }
    pub fn resume_source(
        &mut self,
        value: Option<Vec<u8>>,
        mut resolve: impl FnMut() -> NativeCollation,
    ) -> std::result::Result<NativeInLegacyValueStep, NativeInLegacyError<std::convert::Infallible>>
    {
        let maximum = self.maximum;
        self.state.resume(
            value,
            |left, right| {
                legacy_bytes_allowance(
                    left.map_or(0, Vec::capacity)
                        .checked_add(right.map_or(0, Vec::capacity)),
                    maximum,
                )
            },
            |left, right| legacy_checked_bytes_equal(left, right, maximum, &mut resolve),
        )
    }
}
fn legacy_no_match(saw_null: bool) -> Option<bool> {
    if saw_null { None } else { Some(false) }
}
fn legacy_int_equal(left: i128, right: i128) -> bool {
    left == right
}
fn legacy_bytes_equal(left: &[u8], right: &[u8], collation: NativeCollation) -> bool {
    collation
        .compare(left, right)
        .expect("raw supported collation comparison cannot fail")
        .is_eq()
}
fn legacy_values<T, E>(
    count: usize,
    mut read: impl FnMut(usize) -> std::result::Result<Option<T>, E>,
    mut inspect: impl FnMut(Option<&T>, Option<&T>) -> std::result::Result<(), NativeInLegacyError<E>>,
    mut equal: impl FnMut(&T, &T) -> std::result::Result<bool, NativeInLegacyError<E>>,
) -> std::result::Result<Option<bool>, NativeInLegacyError<E>> {
    // This is an actual source-channel read, even when no child exists. The
    // caller supplies its original missing-child NULL, not a fabricated base.
    let mut state = LegacyValuesState::new(count);
    let mut step = state.begin()?;
    loop {
        step = match step {
            NativeInLegacyValueStep::Source { index } => state.resume(
                read(index).map_err(NativeInLegacyError::Source)?,
                &mut inspect,
                &mut equal,
            )?,
            NativeInLegacyValueStep::Ready(value) => return Ok(value),
        };
    }
}
/// Legacy integer membership over actual native integer channels. Neither an
/// i64 surrogate nor a value frame is introduced. NULL base still demands the
/// complete list; a genuine equality match alone permits an early return.
/// `read` must be the caller's original channel, not an expression surrogate.
pub fn native_in_legacy_i128_values<E>(
    count: usize,
    read: impl FnMut(usize) -> std::result::Result<Option<i128>, E>,
) -> std::result::Result<Option<bool>, NativeInLegacyError<E>> {
    legacy_values(
        count,
        read,
        |_, _| Ok(()),
        |left, right| Ok(legacy_int_equal(*left, *right)),
    )
}
fn legacy_bytes_allowance<E>(
    bytes: Option<usize>,
    maximum: usize,
) -> std::result::Result<(), NativeInLegacyError<E>> {
    match bytes {
        Some(bytes) if bytes <= maximum => Ok(()),
        _ => Err(NativeInLegacyError::ResourceLimit(
            "legacy IN actual retained/comparison allowance exceeded",
        )),
    }
}

fn legacy_checked_bytes_equal<E>(
    left: &Vec<u8>,
    right: &Vec<u8>,
    maximum: usize,
    resolve: &mut impl FnMut() -> NativeCollation,
) -> std::result::Result<bool, NativeInLegacyError<E>> {
    let collation = resolve();
    if matches!(
        collation,
        NativeCollation::GbkBin | NativeCollation::Gb18030Bin
    ) {
        let work = left
            .len()
            .checked_add(right.len())
            .and_then(|n| n.checked_mul(16))
            .and_then(|n| n.checked_add(128));
        let total = left
            .capacity()
            .checked_add(right.capacity())
            .and_then(|n| n.checked_add(work?));
        legacy_bytes_allowance(total, maximum)?;
    }
    Ok(legacy_bytes_equal(left, right, collation))
}
/// Legacy byte membership owns the actual returned byte vectors, not encoded
/// copies. Resolve the effective collator only after both real operands are
/// present, after the candidate callback. A resolver supplies policy, never a
/// host-computed equality answer. NULL pairs never resolve it.
///
/// The finite allowance bounds actual retained vector capacities and known GB
/// binary comparison buffers before their work. Unknown source allocations are
/// measured only after the source returns; shared immutable table
/// initialization and process-wide allocator recovery are not covered by this
/// logical bound.
pub fn native_in_legacy_bytes_values<E>(
    count: usize,
    maximum: usize,
    read: impl FnMut(usize) -> std::result::Result<Option<Vec<u8>>, E>,
    mut resolve: impl FnMut() -> NativeCollation,
) -> std::result::Result<Option<bool>, NativeInLegacyError<E>> {
    legacy_values(
        count,
        read,
        |left, right| {
            legacy_bytes_allowance(
                left.map_or(0, Vec::capacity)
                    .checked_add(right.map_or(0, Vec::capacity)),
                maximum,
            )
        },
        |left, right| legacy_checked_bytes_equal(left, right, maximum, &mut resolve),
    )
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
pub(crate) fn evaluate_in_typed_values_native(
    value: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let limit = bound(value, None)?;
    if !in_typed_values_native_args_valid(value) {
        return Err(Error::Invalid);
    }
    let (domain, count, mut r) = typed_header(value.unwrap()).ok_or(Error::Invalid)?;
    // Full framing validation above still precedes early NULL/match decisions.
    // This iterator only borrows already-validated identities, without a view
    // vector allocation or a second membership implementation.
    let values = (0..count).map(|_| {
        identity(r.optional().expect("validated typed item framing"))
            .expect("validated native identity")
            .map(typed_view)
    });
    let result = native_in_typed_values(domain, values).map_err(|_| Error::Invalid)?;
    terminal(result, limit)
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
        terminal(legacy_no_match(s.saw_null), limit)
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
        if legacy_bytes_equal(
            s.base.ok_or(Error::Invalid)?,
            s.candidate.ok_or(Error::Invalid)?,
            collator,
        ) {
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
            if legacy_int_equal(left, right) {
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
    fn owned_legacy_in_continuations_keep_full_width_null_demand_and_closed_failures() {
        use NativeInLegacyValueStep as S;
        let mut empty = NativeInLegacyIntValues::new(0);
        assert_eq!(empty.begin().unwrap(), S::Source { index: 0 });
        assert_eq!(empty.resume_source(None).unwrap(), S::Ready(None));
        assert!(matches!(
            empty.resume_source(Some(1)),
            Err(NativeInLegacyError::InvalidResponse)
        ));
        let mut wide = NativeInLegacyIntValues::new(4);
        assert_eq!(wide.begin().unwrap(), S::Source { index: 0 });
        assert_eq!(
            wide.resume_source(Some(i128::MIN)).unwrap(),
            S::Source { index: 1 }
        );
        assert_eq!(
            wide.resume_source(Some(i128::MAX)).unwrap(),
            S::Source { index: 2 }
        );
        assert_eq!(
            wide.resume_source(Some(i128::MIN)).unwrap(),
            S::Ready(Some(true))
        );
        assert!(matches!(
            wide.begin(),
            Err(NativeInLegacyError::InvalidResponse)
        ));
        let mut bytes = NativeInLegacyBytesValues::new(3, 4096).unwrap();
        assert_eq!(bytes.begin().unwrap(), S::Source { index: 0 });
        assert_eq!(
            bytes
                .resume_source(None, || panic!("NULL base resolves no collator"))
                .unwrap(),
            S::Source { index: 1 }
        );
        assert_eq!(
            bytes
                .resume_source(Some(b"a".to_vec()), || panic!(
                    "NULL pair resolves no collator"
                ))
                .unwrap(),
            S::Source { index: 2 }
        );
        assert_eq!(
            bytes
                .resume_source(None, || panic!("NULL pair resolves no collator"))
                .unwrap(),
            S::Ready(None)
        );
        assert!(NativeInLegacyBytesValues::new(0, usize::MAX).is_ok());
        let mut denied = NativeInLegacyBytesValues::new(2, 8).unwrap();
        denied.begin().unwrap();
        assert!(matches!(
            denied.resume_source(Some(Vec::with_capacity(32)), || panic!(
                "allowance precedes collator"
            )),
            Err(NativeInLegacyError::ResourceLimit(_))
        ));
        assert!(matches!(
            denied.resume_source(None, || NativeCollation::Binary),
            Err(NativeInLegacyError::InvalidResponse)
        ));
    }
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

#[cfg(test)]
mod typed_value_tests {
    use super::*;
    #[test]
    fn borrowed_typed_values_keep_real_domains_nullable_progress_and_arity() {
        use NativeInTypedDomain as D;
        use NativeInTypedValue as V;
        let eval = |domain, values: Vec<Option<V<'_>>>| {
            native_in_typed_values(domain, values.into_iter()).unwrap()
        };
        for domain in [D::Datetime, D::Timestamp] {
            assert_eq!(
                eval(domain, vec![Some(V::Time(0)), None, Some(V::Time(0))]),
                Some(true)
            );
            assert_eq!(
                eval(
                    domain,
                    vec![Some(V::Time(0)), Some(V::Time(1u64 << 32)), None]
                ),
                None
            );
            // Existing Time Ord ignores packed type/FSP metadata bits.
            assert_eq!(
                eval(domain, vec![Some(V::Time(0)), Some(V::Time(1))]),
                Some(true)
            );
            assert_eq!(
                eval(domain, vec![Some(V::Other), Some(V::Other)]),
                Some(false)
            );
            assert_eq!(eval(domain, vec![None, Some(V::Time(u64::MAX))]), None);
        }
        assert_eq!(
            eval(
                D::Duration,
                vec![Some(V::Duration(i64::MIN)), Some(V::Duration(i64::MIN))]
            ),
            Some(true)
        );
        assert_eq!(
            eval(
                D::Duration,
                vec![Some(V::Duration(1)), Some(V::Duration(2))]
            ),
            Some(false)
        );
        // Keep the existing native binary-JSON malformed-array comparison;
        // neither this value API nor the wire caller adds a parser/validator.
        assert_eq!(
            eval(
                D::Json,
                vec![
                    Some(V::Json {
                        type_code: 0x03,
                        bytes: &[]
                    }),
                    Some(V::Json {
                        type_code: 0x03,
                        bytes: &[255]
                    })
                ]
            ),
            Some(true)
        );
        for domain in [D::Datetime, D::Timestamp, D::Duration, D::Json] {
            assert_eq!(
                native_in_typed_values(domain, std::iter::empty()),
                Err(NativeInTypedArityError { actual: 0 })
            );
            assert_eq!(
                native_in_typed_values(domain, [None].into_iter()),
                Err(NativeInTypedArityError { actual: 1 })
            );
        }
    }
    #[test]
    fn wire_keeps_full_frame_validation_before_borrowed_value_short_circuit() {
        let valid = crate::encode_native_identity(Identity::Duration {
            nanos: 1,
            fsp: i64::MAX,
        })
        .unwrap();
        let mut input =
            encode_native_in_typed_values(NativeInTypedDomain::Duration, &[None, Some(&valid)])
                .unwrap();
        input.pop();
        assert_eq!(
            evaluate_in_typed_values_native(Some(&input)),
            Err(Error::Invalid)
        );
        let input = encode_native_in_typed_values(
            NativeInTypedDomain::Duration,
            &[Some(&valid), Some(&valid)],
        )
        .unwrap();
        let result = evaluate_in_typed_values_native(Some(&input))
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_in_result(&result),
            Some(NativeInResult::Bool(true))
        );
    }
}

#[cfg(test)]
mod legacy_value_tests {
    use std::cell::{Cell, RefCell};

    use super::*;
    #[test]
    fn actual_legacy_channels_keep_zero_count_wide_integers_nullable_demands_and_source_errors() {
        for count in [0, 1] {
            for base in [None, Some(i128::MIN), Some(i128::MAX)] {
                let seen = RefCell::new(vec![]);
                let actual = native_in_legacy_i128_values::<()>(count, |index| {
                    seen.borrow_mut().push(index);
                    Ok(base)
                })
                .unwrap();
                assert_eq!(actual, base.map(|_| false));
                assert_eq!(*seen.borrow(), vec![0]);
            }
        }
        assert_eq!(
            native_in_legacy_i128_values::<()>(2, |i| Ok(Some(if i == 0 {
                1i128 << 64
            } else {
                0
            })))
            .unwrap(),
            Some(false)
        );
        assert_eq!(
            native_in_legacy_i128_values::<()>(3, |i| {
                assert!(i < 2, "early match demanded a dead source");
                Ok(Some(i128::MAX))
            })
            .unwrap(),
            Some(true)
        );
        let seen = RefCell::new(vec![]);
        assert_eq!(
            native_in_legacy_i128_values::<()>(4, |i| {
                seen.borrow_mut().push(i);
                Ok((i != 0).then_some(1))
            })
            .unwrap(),
            None
        );
        assert_eq!(*seen.borrow(), vec![0, 1, 2, 3]);
        let token = 7;
        let error =
            native_in_legacy_i128_values(
                usize::MAX,
                |i| if i == 0 { Ok(None) } else { Err(&token) },
            )
            .unwrap_err();
        let NativeInLegacyError::Source(actual) = error else {
            panic!("source error folded")
        };
        assert!(std::ptr::eq(actual, &token));
    }
    #[test]
    fn legacy_byte_resources_charge_actual_owners_and_work_without_frames_or_later_reads() {
        let reads = RefCell::new(vec![]);
        let policies = Cell::new(0);
        assert_eq!(
            native_in_legacy_bytes_values::<()>(
                2,
                usize::MAX,
                |_| Ok(Some(b"x".to_vec())),
                || NativeCollation::Binary
            )
            .unwrap(),
            Some(true)
        );
        for count in [0, 1] {
            let actual = native_in_legacy_bytes_values::<()>(
                count,
                0,
                |i| {
                    assert_eq!(i, 0);
                    Ok(Some(Vec::new()))
                },
                || panic!("no pair"),
            )
            .unwrap();
            assert_eq!(actual, Some(false));
        }
        // NULL base still reads every candidate; empty owners require no frame
        // overhead and must not request a policy for any NULL pair.
        assert_eq!(
            native_in_legacy_bytes_values::<()>(
                4,
                0,
                |i| {
                    reads.borrow_mut().push(i);
                    Ok(if i == 0 { None } else { Some(Vec::new()) })
                },
                || panic!("NULL pair resolved a collator")
            )
            .unwrap(),
            None
        );
        assert_eq!(*reads.borrow(), vec![0, 1, 2, 3]);
        for large_index in [0, 1] {
            reads.borrow_mut().clear();
            let error = native_in_legacy_bytes_values::<()>(
                3,
                128,
                |i| {
                    reads.borrow_mut().push(i);
                    let mut value = if i == large_index {
                        Vec::with_capacity(1024)
                    } else {
                        Vec::new()
                    };
                    value.push(b'x');
                    Ok(Some(value))
                },
                || {
                    policies.set(policies.get() + 1);
                    NativeCollation::Binary
                },
            )
            .unwrap_err();
            assert!(matches!(error, NativeInLegacyError::ResourceLimit(_)));
            assert_eq!(*reads.borrow(), (0..=large_index).collect::<Vec<_>>());
            assert_eq!(policies.get(), 0);
        }
        // Exact retained capacity succeeds for a streaming binary comparator.
        reads.borrow_mut().clear();
        assert_eq!(
            native_in_legacy_bytes_values::<()>(
                3,
                2,
                |i| {
                    reads.borrow_mut().push(i);
                    Ok(Some(vec![b'x']))
                },
                || NativeCollation::Binary
            )
            .unwrap(),
            Some(true)
        );
        assert_eq!(*reads.borrow(), vec![0, 1]);
        // GB binary conversion is separately bounded AFTER its actual late
        // policy lookup and BEFORE creating the encoded comparison buffers.
        reads.borrow_mut().clear();
        let error = native_in_legacy_bytes_values::<()>(
            3,
            2,
            |i| {
                reads.borrow_mut().push(i);
                Ok(Some(vec![b'x']))
            },
            || {
                policies.set(policies.get() + 1);
                NativeCollation::GbkBin
            },
        )
        .unwrap_err();
        assert!(matches!(error, NativeInLegacyError::ResourceLimit(_)));
        assert_eq!(*reads.borrow(), vec![0, 1]);
        assert_eq!(policies.get(), 1);
        assert_eq!(
            native_in_legacy_bytes_values::<()>(
                3,
                1024,
                |_| Ok(Some(vec![b'x'])),
                || NativeCollation::GbkBin
            )
            .unwrap(),
            Some(true)
        );
        // Reuse after refusal has no suspended cursor or poisoned global state.
        assert_eq!(
            native_in_legacy_bytes_values::<()>(
                2,
                2,
                |_| Ok(Some(vec![b'x'])),
                || NativeCollation::Binary
            )
            .unwrap(),
            Some(true)
        );
    }
}
