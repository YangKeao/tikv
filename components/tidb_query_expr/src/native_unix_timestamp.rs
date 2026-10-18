// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! UNIX_TIMESTAMP's native getter stages and separate strict-Time legacy
//! policy.

use chrono::{NaiveDate, NaiveDateTime};
use tidb_query_datatype::codec::mysql::{
    Time, TimeType,
    time::{NativeSessionTimeZone, NativeTemporalValue, native_core_to_datetime},
};

use crate::{
    NativeIdentityFrameError, NativeIdentityRef, decode_native_identity, encode_native_identity,
};

type FrameResult<T> = Result<T, NativeIdentityFrameError>;
use crate::native_epoch_value::{self as actual, Budget};
fn actual_error(e: crate::NativeEpochValueError) -> NativeIdentityFrameError {
    match e {
        crate::NativeEpochValueError::Resource => NativeIdentityFrameError::Capacity,
        crate::NativeEpochValueError::InvalidKind => NativeIdentityFrameError::Invalid,
    }
}
fn encode_actual(value: crate::NativeUnixTimestampValue) -> FrameResult<Option<Vec<u8>>> {
    use crate::NativeUnixTimestampValue as V;
    match value {
        V::Null => Ok(None),
        V::Int(n) => encode_native_identity(NativeIdentityRef::Int(n)).map(Some),
        V::Decimal(value) => {
            let v = value.as_ref();
            encode_native_identity(NativeIdentityRef::Decimal {
                negative: v.negative,
                coefficient: v.digits,
                scale: v.scale,
                storage_scale: v.storage_scale,
                declared_shape: v.declared_shape,
            })
            .map(Some)
        }
        V::NeedZone(state) => {
            let v = state.value;
            encode_native_identity(NativeIdentityRef::Time {
                core: v.raw,
                kind: 1,
                fsp: v.fsp,
            })
            .map(Some)
        }
        V::Warning(message) => {
            let length = message
                .len()
                .checked_add(3)
                .ok_or(NativeIdentityFrameError::Capacity)?;
            let mut frame = Vec::new();
            frame
                .try_reserve_exact(length)
                .map_err(|_| NativeIdentityFrameError::Capacity)?;
            frame.push(17);
            frame.extend_from_slice(&1292u16.to_le_bytes());
            frame.extend_from_slice(message.as_bytes());
            Ok(Some(frame))
        }
    }
}

/// A computed numeric value, actual parsed base demanding the second zone read,
/// or the original parsing warning. SQL NULL is the outer absence of a frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeUnixTimestampResult<'a> {
    Value(NativeIdentityRef<'a>),
    Continue(NativeTemporalValue),
    Warning { code: u16, message: &'a str },
}

pub(crate) fn decode_unix_timestamp_time(bytes: &[u8]) -> Option<NativeTemporalValue> {
    let NativeIdentityRef::Time { core, kind, fsp } = decode_native_identity(bytes).ok()? else {
        return None;
    };
    let kind = match kind {
        0 => TimeType::Date,
        1 => TimeType::DateTime,
        2 => TimeType::Timestamp,
        _ => return None,
    };
    Some(NativeTemporalValue {
        raw: core,
        kind,
        fsp,
    })
}

/// Decode the closed result classes without parsing decimal text or calendar
/// fields.
pub fn decode_native_unix_timestamp_result(bytes: &[u8]) -> Option<NativeUnixTimestampResult<'_>> {
    if bytes.first() == Some(&17) {
        let code = u16::from_le_bytes(bytes.get(1..3)?.try_into().ok()?);
        if code != 1292 {
            return None;
        }
        return Some(NativeUnixTimestampResult::Warning {
            code,
            message: std::str::from_utf8(bytes.get(3..)?).ok()?,
        });
    }
    match decode_native_identity(bytes).ok()? {
        value @ (NativeIdentityRef::Int(_) | NativeIdentityRef::Decimal { .. }) => {
            Some(NativeUnixTimestampResult::Value(value))
        }
        NativeIdentityRef::Time {
            kind: 1,
            fsp: 0..=6,
            ..
        } => decode_unix_timestamp_time(bytes).map(NativeUnixTimestampResult::Continue),
        _ => None,
    }
}

/// Seconds retain their full i64 domain; nanos represent the original u32,
/// including values at or above one billion.
pub fn unix_timestamp_now_native_args_valid(secs: Option<i64>, nanos: Option<i64>) -> bool {
    secs.is_some() && nanos.is_some_and(|value| u32::try_from(value).is_ok())
}

pub fn unix_timestamp_null_native_args_valid(value: Option<&[u8]>) -> bool {
    value.is_none()
}

pub fn unix_timestamp_parse_native_args_valid(text: Option<&[u8]>, is_float: Option<i64>) -> bool {
    text.is_some_and(|bytes| std::str::from_utf8(bytes).is_ok())
        && is_float.is_some_and(|value| matches!(value, 0 | 1))
}

/// Only the continuation's representation and precision are checked here.
pub fn unix_timestamp_value_native_args_valid(frame: Option<&[u8]>) -> bool {
    frame
        .and_then(decode_unix_timestamp_time)
        .is_some_and(|value| value.kind == TimeType::DateTime && value.fsp <= 6)
}

/// Legacy typed Time accepts all three kinds and every raw FSP byte.
pub fn unix_timestamp_legacy_args_valid(frame: Option<&[u8]>) -> bool {
    frame.and_then(decode_unix_timestamp_time).is_some()
}

/// Shared range alone; callers keep their distinct zero/precision/DST policies.
pub(crate) fn unix_timestamp_micros_in_range(micros: i64) -> bool {
    (1_000_000..=32_536_771_199_999_999).contains(&micros)
}

#[cfg(test)]
fn native_result(micros: i64, fsp: usize) -> FrameResult<Vec<u8>> {
    encode_actual(actual::numeric(micros, fsp, Budget(None)).map_err(actual_error)?)?
        .ok_or(NativeIdentityFrameError::Invalid)
}

pub(super) fn naive(value: NativeTemporalValue) -> Option<NaiveDateTime> {
    let [year, month, day, hour, minute, second, micro] = Time::native_core_fields(value.raw);
    NaiveDate::from_ymd_opt(year, month as u32, day as u32)?.and_hms_micro_opt(
        hour as u32,
        minute as u32,
        second as u32,
        micro as u32,
    )
}

pub(crate) fn evaluate_unix_timestamp_now_native(secs: i64, nanos: u32) -> FrameResult<Vec<u8>> {
    encode_actual(
        actual::native_unix_timestamp_now_values_budget(secs, nanos, Budget(None))
            .map_err(actual_error)?,
    )?
    .ok_or(NativeIdentityFrameError::Invalid)
}

pub(crate) fn evaluate_unix_timestamp_parse_native(
    text: &str,
    is_float: bool,
    zone: &NativeSessionTimeZone,
) -> FrameResult<Option<Vec<u8>>> {
    encode_actual(
        actual::native_unix_timestamp_parse_values_budget(text, is_float, zone, Budget(None))
            .map_err(actual_error)?,
    )
}

pub(crate) fn evaluate_unix_timestamp_value_native(
    value: NativeTemporalValue,
    zone: &NativeSessionTimeZone,
) -> FrameResult<Vec<u8>> {
    encode_actual(
        crate::NativeUnixTimestampParsed { value }
            .finish_budget(zone, Budget(None))
            .map_err(actual_error)?,
    )?
    .ok_or(NativeIdentityFrameError::Invalid)
}

pub(super) fn legacy_micros(
    value: NativeTemporalValue,
    zone: &NativeSessionTimeZone,
) -> Option<i64> {
    if value.raw == 0 {
        return None;
    }
    native_core_to_datetime(value.raw, zone, false)
        .ok()
        .map(|instant| instant.timestamp_micros())
        .filter(|micros| unix_timestamp_micros_in_range(*micros))
}

pub(crate) fn evaluate_unix_timestamp_int_legacy(
    value: NativeTemporalValue,
    zone: &NativeSessionTimeZone,
) -> FrameResult<Vec<u8>> {
    encode_actual(
        actual::native_unix_timestamp_legacy_values_budget(Some(value), zone, false, Budget(None))
            .map_err(actual_error)?,
    )?
    .ok_or(NativeIdentityFrameError::Invalid)
}

pub(crate) fn evaluate_unix_timestamp_dec_legacy(
    value: NativeTemporalValue,
    zone: &NativeSessionTimeZone,
) -> FrameResult<Vec<u8>> {
    encode_actual(
        actual::native_unix_timestamp_legacy_values_budget(Some(value), zone, true, Budget(None))
            .map_err(actual_error)?,
    )?
    .ok_or(NativeIdentityFrameError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_timestamp_stages_keep_zone_demand_precision_and_legacy_policy() {
        let utc = NativeSessionTimeZone::utc();
        for (text, numeric) in [("0000-00-00 12:34:56.123", false), ("0.123", true)] {
            assert_eq!(
                evaluate_unix_timestamp_parse_native(text, numeric, &utc).unwrap(),
                None
            );
        }
        let partial = evaluate_unix_timestamp_parse_native("2017-00-02 00:00:00.123", false, &utc)
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_unix_timestamp_result(&partial),
            Some(NativeUnixTimestampResult::Value(
                NativeIdentityRef::Decimal {
                    negative: false,
                    scale: 3,
                    storage_scale: 3,
                    declared_shape: None,
                    coefficient: b"000",
                }
            ))
        );
        let bad = evaluate_unix_timestamp_parse_native("bad-date", false, &utc)
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_unix_timestamp_result(&bad),
            Some(NativeUnixTimestampResult::Warning {
                code: 1292,
                message: "Incorrect datetime value: 'bad-date'",
            })
        );
        let dotted = evaluate_unix_timestamp_parse_native("2024.03.15 12:34:56.1", false, &utc)
            .unwrap()
            .unwrap();
        let Some(NativeUnixTimestampResult::Continue(dotted)) =
            decode_native_unix_timestamp_result(&dotted)
        else {
            panic!("valid date demands second zone")
        };
        assert_eq!(dotted.fsp, 1); // TIME's last-fraction rule, not duration's first dot.
        let base = NativeTemporalValue {
            raw: Time::native_core_from_fields(1970, 1, 2, 0, 0, 1, 0),
            kind: TimeType::DateTime,
            fsp: 0,
        };
        let raw_offset = NativeSessionTimeZone::Fixed {
            name: String::new(),
            offset_secs: 86_400,
        };
        let converted = evaluate_unix_timestamp_value_native(base, &raw_offset).unwrap();
        assert_eq!(
            decode_native_identity(&converted).unwrap(),
            NativeIdentityRef::Int(1)
        );
        let gap = NativeTemporalValue {
            raw: Time::native_core_from_fields(2025, 3, 30, 2, 30, 0, 0),
            kind: TimeType::DateTime,
            fsp: 0,
        };
        let paris = NativeSessionTimeZone::Named("Europe/Paris".parse().unwrap());
        let adjusted = evaluate_unix_timestamp_value_native(gap, &paris).unwrap();
        assert_eq!(
            decode_native_identity(&adjusted).unwrap(),
            NativeIdentityRef::Int(1_743_296_400)
        );
        let strict = evaluate_unix_timestamp_int_legacy(gap, &paris).unwrap();
        assert_eq!(
            decode_native_identity(&strict).unwrap(),
            NativeIdentityRef::Int(0)
        );
        let strict = evaluate_unix_timestamp_dec_legacy(gap, &paris).unwrap();
        assert_eq!(
            decode_native_identity(&strict).unwrap(),
            NativeIdentityRef::Decimal {
                negative: false,
                scale: 0,
                storage_scale: 0,
                declared_shape: None,
                coefficient: b"0",
            }
        );
        let raw = NativeTemporalValue {
            raw: Time::native_core_from_fields(1970, 1, 1, 0, 0, 1, 123_456),
            kind: TimeType::Date,
            fsp: 255,
        };
        let legacy = evaluate_unix_timestamp_dec_legacy(raw, &utc).unwrap();
        assert_eq!(
            decode_native_identity(&legacy).unwrap(),
            NativeIdentityRef::Decimal {
                negative: false,
                scale: 6,
                storage_scale: 6,
                declared_shape: None,
                coefficient: b"1123456",
            }
        );
        let truncated = native_result(1_234_567, 3).unwrap();
        assert_eq!(
            decode_native_identity(&truncated).unwrap(),
            NativeIdentityRef::Decimal {
                negative: false,
                scale: 3,
                storage_scale: 3,
                declared_shape: None,
                coefficient: b"1234",
            }
        );
        let now = evaluate_unix_timestamp_now_native(1, u32::MAX).unwrap();
        assert_eq!(
            decode_native_identity(&now).unwrap(),
            NativeIdentityRef::Int(5)
        );
        assert!(!unix_timestamp_micros_in_range(999_999));
        assert!(unix_timestamp_micros_in_range(1_000_000));
        assert!(unix_timestamp_micros_in_range(32_536_771_199_999_999));
        assert!(!unix_timestamp_micros_in_range(32_536_771_200_000_000));
    }

    #[test]
    fn unix_timestamp_codec_and_validators_keep_their_distinct_raw_domains() {
        for kind in 0..=2 {
            for fsp in [0, 6, 255] {
                let bytes = encode_native_identity(NativeIdentityRef::Time {
                    core: u64::MAX,
                    kind,
                    fsp,
                })
                .unwrap();
                assert!(unix_timestamp_legacy_args_valid(Some(&bytes)));
                assert_eq!(
                    unix_timestamp_value_native_args_valid(Some(&bytes)),
                    kind == 1 && fsp <= 6
                );
                assert_eq!(
                    decode_native_unix_timestamp_result(&bytes).is_some(),
                    kind == 1 && fsp <= 6
                );
                for length in 0..bytes.len() {
                    assert!(!unix_timestamp_legacy_args_valid(Some(&bytes[..length])));
                }
                let mut excess = bytes;
                excess.push(0);
                assert!(!unix_timestamp_legacy_args_valid(Some(&excess)));
            }
        }
        for bytes in [
            vec![],
            vec![17],
            vec![17, 12],
            vec![17, 0, 0],
            vec![17, 12, 5, 255],
        ] {
            assert!(decode_native_unix_timestamp_result(&bytes).is_none());
        }
        let wrong = encode_native_identity(NativeIdentityRef::UInt(1)).unwrap();
        assert!(decode_native_unix_timestamp_result(&wrong).is_none());
        let warning =
            evaluate_unix_timestamp_parse_native("界", false, &NativeSessionTimeZone::utc())
                .unwrap()
                .unwrap();
        assert_eq!(
            decode_native_unix_timestamp_result(&warning),
            Some(NativeUnixTimestampResult::Warning {
                code: 1292,
                message: "Incorrect datetime value: '界'"
            })
        );
        assert!(unix_timestamp_now_native_args_valid(
            Some(i64::MAX),
            Some(i64::from(u32::MAX))
        ));
        assert!(!unix_timestamp_now_native_args_valid(None, Some(0)));
        assert!(!unix_timestamp_now_native_args_valid(Some(0), Some(-1)));
        assert!(!unix_timestamp_now_native_args_valid(
            Some(0),
            Some(i64::from(u32::MAX) + 1)
        ));
        assert!(unix_timestamp_null_native_args_valid(None));
        assert!(!unix_timestamp_null_native_args_valid(Some(b"")));
        assert!(unix_timestamp_parse_native_args_valid(Some(b""), Some(1)));
        assert!(!unix_timestamp_parse_native_args_valid(None, Some(0)));
        assert!(!unix_timestamp_parse_native_args_valid(
            Some(&[255]),
            Some(0)
        ));
        assert!(!unix_timestamp_parse_native_args_valid(Some(b"x"), Some(2)));
    }
}
