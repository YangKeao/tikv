// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native EXTRACT selection, two lazy mixed-signature stages, and the distinct
//! broad calendar-composite policy. Casts and session getters stay outside;
//! neither a selected branch nor a computed answer is supplied by the caller.

use chrono::Utc;
use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::codec::{
    data_type::{Bytes, BytesRef, ChunkedVec, EvaluableRef, EvaluableRet, Int},
    mysql::{
        Duration, Time, TimeType,
        duration::{MAX_NANOS, native_parse_mysql_duration},
        time::{
            native_extract_datetime_num, native_extract_duration_num, native_get_time_fsp,
            native_is_clock_unit, native_is_date_unit, native_parse_time,
        },
    },
};

use crate::NativeIdentityFrameError;

type FrameResult<T> = std::result::Result<T, NativeIdentityFrameError>;
const ZERO_WARNING: &str = "Incorrect datetime value: '0000-00-00 00:00:00'";
const INVALID_UNIT_PREFIX: &str = "invalid unit ";
const INVALID_TIME_PREFIX: &str = "Truncated incorrect time value: '";
const STATE_HEADER: usize = 25;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExtractResult<'a> {
    Value(i64),
    /// Complete SDK-owned message; SQL code 1105.
    InvalidUnit(&'a str),
    /// Complete SDK-owned message; hard conversion error 1292, not a warning.
    InvalidTime(&'a str),
    /// Complete SDK-owned message; append warning 1292 and return SQL NULL.
    Warning(&'a str),
    /// Borrow of the WHOLE report. Forward the original bytes unchanged before
    /// supplying the second, separately demanded allow-invalid-date flag.
    NeedMixedDatetime(&'a [u8]),
    NeedDatetimeCast,
    NeedDurationCast,
    NeedMixedStringCast,
}

struct MixedState<'a> {
    unit: &'a str,
    text: &'a str,
    nanos: i64,
}

fn read_state(report: &[u8]) -> Option<MixedState<'_>> {
    if report.len() < STATE_HEADER || report.first() != Some(&4) {
        return None;
    }
    let nanos = i64::from_le_bytes(report[1..9].try_into().ok()?);
    let fsp = i64::from_le_bytes(report[9..17].try_into().ok()?);
    let unit_len = usize::try_from(u64::from_le_bytes(report[17..25].try_into().ok()?)).ok()?;
    let split = STATE_HEADER.checked_add(unit_len)?;
    // The first stage has accepted a microsecond-resolution, non-overflowing
    // native duration and normalized its FSP. Do not parse the input again here.
    if !(0..=6).contains(&fsp) || !(-MAX_NANOS..=MAX_NANOS).contains(&nanos) || nanos % 1_000 != 0 {
        return None;
    }
    Some(MixedState {
        unit: std::str::from_utf8(report.get(STATE_HEADER..split)?).ok()?,
        text: std::str::from_utf8(report.get(split..)?).ok()?,
        nanos,
    })
}

pub fn decode_native_extract_result(report: &[u8]) -> Option<NativeExtractResult<'_>> {
    let (&tag, payload) = report.split_first()?;
    match tag {
        0 if payload.len() == 8 => Some(NativeExtractResult::Value(i64::from_le_bytes(
            payload.try_into().ok()?,
        ))),
        1 => {
            let message = std::str::from_utf8(payload).ok()?;
            message
                .starts_with(INVALID_UNIT_PREFIX)
                .then_some(NativeExtractResult::InvalidUnit(message))
        }
        2 => {
            let message = std::str::from_utf8(payload).ok()?;
            (message.starts_with(INVALID_TIME_PREFIX)
                && message.ends_with('\'')
                && message.len() >= INVALID_TIME_PREFIX.len() + 1)
                .then_some(NativeExtractResult::InvalidTime(message))
        }
        3 if payload == ZERO_WARNING.as_bytes() => Some(NativeExtractResult::Warning(ZERO_WARNING)),
        4 => read_state(report).map(|_| NativeExtractResult::NeedMixedDatetime(report)),
        5 if payload.is_empty() => Some(NativeExtractResult::NeedDatetimeCast),
        6 if payload.is_empty() => Some(NativeExtractResult::NeedDurationCast),
        7 if payload.is_empty() => Some(NativeExtractResult::NeedMixedStringCast),
        _ => None,
    }
}

/// Uniform checked retained-output bound. The caller supplies the actual byte
/// slots for its role (mixed-finish supplies the whole state as the first
/// slot). Composite SQL NULL may conservatively charge this bound but returns
/// no Vec. Codec/parser temporary allocations are not a peak-allocation
/// guarantee.
pub(crate) fn native_extract_output_bound(
    first: Option<&[u8]>,
    second: Option<&[u8]>,
) -> Option<usize> {
    first
        .map_or(0, <[u8]>::len)
        .checked_add(second.map_or(0, <[u8]>::len))?
        .checked_add(64)
}

fn valid_text(bytes: Option<&[u8]>) -> bool {
    bytes.is_some_and(|bytes| std::str::from_utf8(bytes).is_ok())
}
fn valid_flag(flag: Option<i64>) -> bool {
    matches!(flag, Some(0 | 1))
}

/// SOURCECODE is real optional FieldTypeCode metadata: named mysql_type bytes
/// 0..255, Unknown(byte) = -1-byte. KIND is the actual Rust DatumKind enum
/// discriminant 0..18 (Null=0, Duration=11, Time=15), not a chosen-signature
/// flag.
pub fn extract_select_native_args_valid(
    unit: Option<&[u8]>,
    source: Option<i64>,
    kind: Option<i64>,
) -> bool {
    native_extract_output_bound(unit, None).is_some()
        && valid_text(unit)
        && source.is_none_or(|code| (-256..=255).contains(&code))
        && kind.is_some_and(|kind| (0..=18).contains(&kind))
}

pub fn extract_datetime_native_args_valid(core: Option<&[u8]>, unit: Option<&[u8]>) -> bool {
    native_extract_output_bound(core, unit).is_some()
        && core.is_some_and(|core| core.len() == 8)
        && valid_text(unit)
}

pub fn extract_duration_native_args_valid(unit: Option<&[u8]>, nanos: Option<i64>) -> bool {
    native_extract_output_bound(unit, None).is_some() && valid_text(unit) && nanos.is_some()
}

pub fn extract_mixed_duration_native_args_valid(
    unit: Option<&[u8]>,
    text: Option<&[u8]>,
    allow: Option<i64>,
) -> bool {
    native_extract_output_bound(unit, text).is_some()
        && valid_text(unit)
        && valid_text(text)
        && valid_flag(allow)
}

pub fn extract_mixed_finish_native_args_valid(state: Option<&[u8]>, allow: Option<i64>) -> bool {
    native_extract_output_bound(state, None).is_some()
        && state.is_some_and(|state| read_state(state).is_some())
        && valid_flag(allow)
}

pub fn extract_composite_native_args_valid(unit: Option<&[u8]>, text: Option<&[u8]>) -> bool {
    native_extract_output_bound(unit, text).is_some()
        && valid_text(unit)
        && text.is_none_or(|text| std::str::from_utf8(text).is_ok())
}

fn text(bytes: Option<&[u8]>) -> FrameResult<&str> {
    std::str::from_utf8(bytes.ok_or(NativeIdentityFrameError::Invalid)?)
        .map_err(|_| NativeIdentityFrameError::Invalid)
}

fn bound(first: Option<&[u8]>, second: Option<&[u8]>) -> FrameResult<usize> {
    native_extract_output_bound(first, second).ok_or(NativeIdentityFrameError::Capacity)
}

fn report(tag: u8, pieces: &[&[u8]], bound: usize) -> FrameResult<Option<Vec<u8>>> {
    let length = pieces
        .iter()
        .try_fold(1_usize, |length, piece| length.checked_add(piece.len()))
        .ok_or(NativeIdentityFrameError::Capacity)?;
    if length > bound {
        return Err(NativeIdentityFrameError::Capacity);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| NativeIdentityFrameError::Capacity)?;
    if output.capacity() > bound {
        return Err(NativeIdentityFrameError::Capacity);
    }
    output.push(tag);
    for piece in pieces {
        output.extend_from_slice(piece);
    }
    Ok(Some(output))
}

fn number(
    value: std::result::Result<i64, tidb_query_datatype::codec::mysql::time::NativeTimeError>,
    unit: &str,
    bound: usize,
) -> FrameResult<Option<Vec<u8>>> {
    match value {
        Ok(value) => report(0, &[&value.to_le_bytes()], bound),
        Err(_) => report(1, &[INVALID_UNIT_PREFIX.as_bytes(), unit.as_bytes()], bound),
    }
}

fn invalid_time(text: &str, bound: usize) -> FrameResult<Option<Vec<u8>>> {
    report(
        2,
        &[INVALID_TIME_PREFIX.as_bytes(), text.as_bytes(), b"'"],
        bound,
    )
}

pub(crate) fn evaluate_extract_select_native(
    unit: Option<&[u8]>,
    source: Option<i64>,
    kind: Option<i64>,
) -> FrameResult<Option<Vec<u8>>> {
    let bound = bound(unit, None)?;
    if !extract_select_native_args_valid(unit, source, kind) {
        return Err(NativeIdentityFrameError::Invalid);
    }
    let unit = text(unit)?;
    let clock = native_is_clock_unit(unit);
    let date = native_is_date_unit(unit);
    let datetime = matches!(source, Some(7 | 10 | 12)) || kind == Some(15);
    let request = if !clock || (date && datetime) {
        5
    } else if date && kind != Some(11) {
        7
    } else {
        6
    };
    report(request, &[], bound)
}

pub(crate) fn evaluate_extract_datetime_native(
    core: Option<&[u8]>,
    unit: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let bound = bound(core, unit)?;
    if !extract_datetime_native_args_valid(core, unit) {
        return Err(NativeIdentityFrameError::Invalid);
    }
    let core = u64::from_le_bytes(
        core.unwrap()
            .try_into()
            .map_err(|_| NativeIdentityFrameError::Invalid)?,
    );
    let unit = text(unit)?;
    number(native_extract_datetime_num(core, unit), unit, bound)
}

pub(crate) fn evaluate_extract_duration_native(
    unit: Option<&[u8]>,
    nanos: Option<i64>,
) -> FrameResult<Option<Vec<u8>>> {
    let bound = bound(unit, None)?;
    if !extract_duration_native_args_valid(unit, nanos) {
        return Err(NativeIdentityFrameError::Invalid);
    }
    let unit = text(unit)?;
    number(
        native_extract_duration_num(nanos.unwrap(), unit),
        unit,
        bound,
    )
}

pub(crate) fn evaluate_extract_mixed_duration_native(
    unit: Option<&[u8]>,
    input: Option<&[u8]>,
    allow: Option<i64>,
) -> FrameResult<Option<Vec<u8>>> {
    let bound = bound(unit, input)?;
    if !extract_mixed_duration_native_args_valid(unit, input, allow) {
        return Err(NativeIdentityFrameError::Invalid);
    }
    let unit = text(unit)?;
    let input = text(input)?;
    let parsed = match native_parse_mysql_duration(
        input,
        i64::from(native_get_time_fsp(input)),
        &Utc,
        true,
        allow == Some(1),
    ) {
        Ok(parsed) if parsed.overflow.is_none() && !parsed.truncated => parsed,
        _ => return invalid_time(input, bound),
    };
    // Preserve MySqlDuration::from_nanoseconds' final FSP check. It does not
    // impose a second range check or round the parsed nanoseconds.
    let Some(fsp) = Time::native_normalize_fsp(parsed.fsp) else {
        return invalid_time(input, bound);
    };
    let unit_length = u64::try_from(unit.len()).map_err(|_| NativeIdentityFrameError::Capacity)?;
    report(
        4,
        &[
            &parsed.nanos.to_le_bytes(),
            &fsp.to_le_bytes(),
            &unit_length.to_le_bytes(),
            unit.as_bytes(),
            input.as_bytes(),
        ],
        bound,
    )
}

pub(crate) fn evaluate_extract_mixed_finish_native(
    state: Option<&[u8]>,
    allow: Option<i64>,
) -> FrameResult<Option<Vec<u8>>> {
    let bound = bound(state, None)?;
    if !extract_mixed_finish_native_args_valid(state, allow) {
        return Err(NativeIdentityFrameError::Invalid);
    }
    let state = read_state(state.unwrap()).ok_or(NativeIdentityFrameError::Invalid)?;
    // This is a SEPARATELY requested mode value. Recompute datetime FSP from
    // the original text, not duration FSP (an exact zero core resets that to 0).
    if let Ok(parsed) = native_parse_time(
        state.text,
        TimeType::DateTime,
        i64::from(native_get_time_fsp(state.text)),
        false,
        true,
        allow == Some(1),
        true,
        &Utc,
    ) {
        let [year, _, _, hour, minute, second, _] = Time::native_core_fields(parsed.time.raw);
        if year > 0
            && i64::from(hour) == i64::from(Duration::hours_from_nanos(state.nanos))
            && i64::from(minute) == i64::from(Duration::minutes_from_nanos(state.nanos))
            && i64::from(second) == i64::from(Duration::secs_from_nanos(state.nanos))
        {
            return number(
                native_extract_datetime_num(parsed.time.raw, state.unit),
                state.unit,
                bound,
            );
        }
    }
    number(
        native_extract_duration_num(state.nanos, state.unit),
        state.unit,
        bound,
    )
}

pub(crate) fn evaluate_extract_composite_native(
    unit: Option<&[u8]>,
    input: Option<&[u8]>,
) -> FrameResult<Option<Vec<u8>>> {
    let bound = bound(unit, input)?;
    if !extract_composite_native_args_valid(unit, input) {
        return Err(NativeIdentityFrameError::Invalid);
    }
    let unit = text(unit)?;
    let Some(input) = input else {
        return Ok(None);
    };
    let input = std::str::from_utf8(input).map_err(|_| NativeIdentityFrameError::Invalid)?;
    let trimmed = input.trim();
    if let Some((negative, hour, minute, second, microsecond)) = parse_signed_duration_hms(trimmed)
    {
        let value = duration_composite_value(unit, hour, minute, second, microsecond);
        return report(
            0,
            &[&(if negative { -value } else { value }).to_le_bytes()],
            bound,
        );
    }
    let (date, suffix) = trimmed
        .split_once(char::is_whitespace)
        .map_or((trimmed, None), |(date, time)| (date, Some(time)));
    let Some((year, month, day)) = Time::parse_native_date_ymd(date) else {
        return report(3, &[ZERO_WARNING.as_bytes()], bound);
    };
    let (hour, minute, second, microsecond) =
        time_parts_with_micros(suffix).unwrap_or((0, 0, 0, 0));
    let value = datetime_composite_value(unit, year, month, day, hour, minute, second, microsecond);
    report(0, &[&value.to_le_bytes()], bound)
}

// The calendar helper's broad policy is NOT the duration lexer: right-aligned
// groups, unrestricted u32 hour, truncating six-digit fraction, Rust trimming.
fn parse_signed_duration_hms(input: &str) -> Option<(bool, u32, u32, u32, u32)> {
    let (negative, body) = input
        .strip_prefix('-')
        .map_or((false, input), |rest| (true, rest));
    if body.contains('-') {
        return None;
    }
    let mut groups: Vec<&str> = body.split(':').collect();
    let second_field = groups.last_mut()?;
    let (second_digits, fraction) = second_field.split_once('.').unwrap_or((*second_field, ""));
    *second_field = second_digits;
    if groups.iter().any(|group| group.is_empty()) || fraction.is_empty() && groups.is_empty() {
        return None;
    }
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let fraction = fraction.chars().take(6).collect::<String>();
    let microsecond = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u32>().ok()? * 10u32.pow(6 - fraction.len() as u32)
    };
    let parse_field = |field: &str| -> Option<u32> {
        if field.is_empty() {
            return None;
        }
        field.trim().parse().ok()
    };
    let (hour, minute, second) = match groups.len() {
        3 => (
            parse_field(groups[0].trim())?,
            parse_field(groups[1].trim())?,
            parse_field(groups[2].trim())?,
        ),
        2 => (
            0,
            parse_field(groups[0].trim())?,
            parse_field(groups[1].trim())?,
        ),
        1 => (0, 0, parse_field(groups[0].trim())?),
        _ => return None,
    };
    if minute > 59 || second > 59 {
        return None;
    }
    Some((negative, hour, minute, second, microsecond))
}

fn time_parts_with_micros(suffix: Option<&str>) -> Option<(u32, u32, u32, u32)> {
    let Some(suffix) = suffix else {
        return Some((0, 0, 0, 0));
    };
    let (hour, minute, second, fraction) = Time::parse_native_clock_with_fraction(suffix)?;
    let scale = 10u32.pow(6 - fraction.len() as u32);
    let microsecond = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u32>().ok()? * scale
    };
    Some((hour, minute, second, microsecond))
}

// Keep ordinary i64 arithmetic, including the old debug-overflow behavior for
// huge accepted u32 hours; do not force these values into a raw duration/core.
fn duration_composite_value(
    unit: &str,
    hour: u32,
    minute: u32,
    second: u32,
    microsecond: u32,
) -> i64 {
    let (h, mi, sec, microsecond) = (
        i64::from(hour),
        i64::from(minute),
        i64::from(second),
        i64::from(microsecond),
    );
    match unit.to_ascii_uppercase().as_str() {
        "HOUR_MINUTE" => h * 100 + mi,
        "HOUR_SECOND" => h * 10_000 + mi * 100 + sec,
        "HOUR_MICROSECOND" => (h * 10_000 + mi * 100 + sec) * 1_000_000 + microsecond,
        "MINUTE_SECOND" => mi * 100 + sec,
        "MINUTE_MICROSECOND" => (mi * 100 + sec) * 1_000_000 + microsecond,
        "SECOND_MICROSECOND" => sec * 1_000_000 + microsecond,
        "DAY_HOUR" => h,
        "DAY_MINUTE" => h * 100 + mi,
        "DAY_SECOND" => h * 10_000 + mi * 100 + sec,
        "DAY_MICROSECOND" => (h * 10_000 + mi * 100 + sec) * 1_000_000 + microsecond,
        _ => 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn datetime_composite_value(
    unit: &str,
    y: i64,
    m: u32,
    d: u32,
    h: u32,
    mi: u32,
    sec: u32,
    microsecond: u32,
) -> i64 {
    let (d, h, mi, sec, microsecond) = (
        i64::from(d),
        i64::from(h),
        i64::from(mi),
        i64::from(sec),
        i64::from(microsecond),
    );
    match unit.to_ascii_uppercase().as_str() {
        "HOUR_MINUTE" => h * 100 + mi,
        "HOUR_SECOND" => h * 10_000 + mi * 100 + sec,
        "HOUR_MICROSECOND" => (h * 10_000 + mi * 100 + sec) * 1_000_000 + microsecond,
        "MINUTE_SECOND" => mi * 100 + sec,
        "MINUTE_MICROSECOND" => (mi * 100 + sec) * 1_000_000 + microsecond,
        "SECOND_MICROSECOND" => sec * 1_000_000 + microsecond,
        "DAY_HOUR" => d * 100 + h,
        "DAY_MINUTE" => d * 10_000 + h * 100 + mi,
        "DAY_SECOND" => d * 1_000_000 + h * 10_000 + mi * 100 + sec,
        "DAY_MICROSECOND" => {
            (d * 1_000_000 + h * 10_000 + mi * 100 + sec) * 1_000_000 + microsecond
        }
        "YEAR_MONTH" => y * 100 + i64::from(m),
        _ => 0,
    }
}

fn kernel(result: FrameResult<Option<Vec<u8>>>) -> Result<Option<Bytes>> {
    result.map_err(|error| other_err!("Invalid native EXTRACT transport: {:?}", error))
}

#[rpn_fn(nullable)]
fn extract_select_native(
    unit: Option<BytesRef>,
    source: Option<&Int>,
    kind: Option<&Int>,
) -> Result<Option<Bytes>> {
    kernel(evaluate_extract_select_native(
        unit,
        source.copied(),
        kind.copied(),
    ))
}
#[rpn_fn(nullable)]
fn extract_datetime_native(
    core: Option<BytesRef>,
    unit: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    kernel(evaluate_extract_datetime_native(core, unit))
}
#[rpn_fn(nullable)]
fn extract_duration_native(unit: Option<BytesRef>, nanos: Option<&Int>) -> Result<Option<Bytes>> {
    kernel(evaluate_extract_duration_native(unit, nanos.copied()))
}
#[rpn_fn(nullable)]
fn extract_mixed_duration_native(
    unit: Option<BytesRef>,
    text: Option<BytesRef>,
    allow: Option<&Int>,
) -> Result<Option<Bytes>> {
    kernel(evaluate_extract_mixed_duration_native(
        unit,
        text,
        allow.copied(),
    ))
}
#[rpn_fn(nullable)]
fn extract_mixed_finish_native(
    state: Option<BytesRef>,
    allow: Option<&Int>,
) -> Result<Option<Bytes>> {
    kernel(evaluate_extract_mixed_finish_native(state, allow.copied()))
}
#[rpn_fn(nullable)]
fn extract_composite_native(
    unit: Option<BytesRef>,
    text: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    kernel(evaluate_extract_composite_native(unit, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_preserves_selector_mixed_stages_broad_composites_and_reports() {
        fn value(result: FrameResult<Option<Vec<u8>>>) -> i64 {
            let result = result.unwrap().unwrap();
            match decode_native_extract_result(&result).unwrap() {
                NativeExtractResult::Value(value) => value,
                other => panic!("expected value, got {other:?}"),
            }
        }
        for (unit, source, kind, expected) in [
            (
                b"DAY_HOUR".as_slice(),
                None,
                8,
                NativeExtractResult::NeedMixedStringCast,
            ),
            (
                b"DAY_HOUR",
                Some(12),
                8,
                NativeExtractResult::NeedDatetimeCast,
            ),
            (
                b"DAY_HOUR",
                Some(-13),
                8,
                NativeExtractResult::NeedMixedStringCast,
            ),
            (b"DAY_HOUR", None, 15, NativeExtractResult::NeedDatetimeCast),
            (b"DAY_HOUR", None, 11, NativeExtractResult::NeedDurationCast),
            (
                b"DAY_HOUR",
                None,
                0,
                NativeExtractResult::NeedMixedStringCast,
            ),
            (b"HOUR", Some(12), 15, NativeExtractResult::NeedDurationCast),
            (b"hour ", None, 8, NativeExtractResult::NeedDatetimeCast),
        ] {
            let report = evaluate_extract_select_native(Some(unit), source, Some(kind))
                .unwrap()
                .unwrap();
            assert_eq!(decode_native_extract_result(&report), Some(expected));
            assert!(report.capacity() <= native_extract_output_bound(Some(unit), None).unwrap());
        }
        let raw = Time::native_core_from_fields(2020, 1, 2, 3, 4, 5, 123_456).to_le_bytes();
        assert_eq!(
            value(evaluate_extract_datetime_native(
                Some(&raw),
                Some(b"DAY_MICROSECOND")
            )),
            2_030_405_123_456
        );
        assert_eq!(
            value(evaluate_extract_duration_native(
                Some(b"DAY_SECOND"),
                Some(-11_045_000_000_000)
            )),
            -30_405
        );
        let invalid = evaluate_extract_datetime_native(Some(&raw), Some(b"hOuR"))
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_extract_result(&invalid),
            Some(NativeExtractResult::InvalidUnit("invalid unit hOuR"))
        );
        let unit = Some(b"DAY_SECOND".as_slice());
        let input = Some(b"2020-02-31 03:04:05".as_slice());
        let rejected = evaluate_extract_mixed_duration_native(unit, input, Some(0))
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_extract_result(&rejected),
            Some(NativeExtractResult::InvalidTime(
                "Truncated incorrect time value: '2020-02-31 03:04:05'"
            ))
        );
        let state = evaluate_extract_mixed_duration_native(unit, input, Some(1))
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_extract_result(&state),
            Some(NativeExtractResult::NeedMixedDatetime(&state))
        );
        assert!(state.capacity() <= native_extract_output_bound(unit, input).unwrap());
        assert_eq!(
            value(evaluate_extract_mixed_finish_native(Some(&state), Some(0))),
            30_405
        );
        assert_eq!(
            value(evaluate_extract_mixed_finish_native(Some(&state), Some(1))),
            31_030_405
        );
        for input in [b"1x".as_slice(), b"838:59:59.1"] {
            let report = evaluate_extract_mixed_duration_native(unit, Some(input), Some(0))
                .unwrap()
                .unwrap();
            assert!(matches!(
                decode_native_extract_result(&report),
                Some(NativeExtractResult::InvalidTime(_))
            ));
        }
        let short = evaluate_extract_mixed_duration_native(unit, Some(b"1:02"), Some(0))
            .unwrap()
            .unwrap();
        assert_eq!(
            value(evaluate_extract_mixed_finish_native(Some(&short), Some(0))),
            10_200
        );
        assert_eq!(
            value(evaluate_extract_composite_native(unit, Some(b"1:02"))),
            102
        );
        for (unit, input, expected) in [
            (
                b"DAY_MICROSECOND".as_slice(),
                b"-01:02:03.1234567".as_slice(),
                -10_203_123_456,
            ),
            (b"DAY_HOUR", b"4294967295:00:00", 4_294_967_295),
            (b"YEAR_MONTH", b"4294967295-01-02", 429_496_729_501),
            (b"DAY_SECOND", b"2020-01-02 nonsense", 2_000_000),
            (b"unknown", b"12", 0),
            (b"day_second", b"1:02", 102),
        ] {
            let report = evaluate_extract_composite_native(Some(unit), Some(input))
                .unwrap()
                .unwrap();
            assert_eq!(
                decode_native_extract_result(&report),
                Some(NativeExtractResult::Value(expected))
            );
            assert!(
                report.capacity() <= native_extract_output_bound(Some(unit), Some(input)).unwrap()
            );
        }
        let warning = evaluate_extract_composite_native(unit, Some(b"not-date"))
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_extract_result(&warning),
            Some(NativeExtractResult::Warning(ZERO_WARNING))
        );
        assert_eq!(evaluate_extract_composite_native(unit, None).unwrap(), None);
        assert!(!extract_select_native_args_valid(unit, Some(-257), Some(0)));
        assert!(!extract_select_native_args_valid(unit, None, Some(19)));
        assert!(!extract_datetime_native_args_valid(Some(&[0; 7]), unit));
        assert!(!extract_duration_native_args_valid(unit, None));
        assert!(!extract_mixed_duration_native_args_valid(
            unit,
            Some(b"1"),
            Some(2)
        ));
        assert!(!extract_mixed_finish_native_args_valid(Some(&state), None));
        let mut malformed = state.clone();
        malformed[9..17].copy_from_slice(&7_i64.to_le_bytes());
        assert!(decode_native_extract_result(&malformed).is_none());
        malformed = state.clone();
        malformed[17..25].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode_native_extract_result(&malformed).is_none());
        for invalid in [
            b"".as_slice(),
            &[0],
            &[5, 0],
            &[6, 0],
            &[7, 0],
            &[8],
            &[0; 10],
        ] {
            assert!(decode_native_extract_result(invalid).is_none());
        }
        assert_eq!(
            extract_select_native_fn_meta().name,
            "extract_select_native"
        );
        assert_eq!(
            extract_mixed_finish_native_fn_meta().name,
            "extract_mixed_finish_native"
        );
    }
}
