// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! FROM_UNIXTIME's computed epoch handoff, native local rendering and distinct
//! legacy typed-Time policy. No caller performs numeric preparation or
//! rounding.

use chrono::{Datelike, NaiveDateTime, Timelike, Utc};
use tidb_query_datatype::codec::{
    mysql::{NativeDecimalParseRef, time::NativeSessionTimeZone},
    native_sql_string::NativeSqlStringInput as Input,
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
fn actual_input(value: NativeIdentityRef<'_>) -> FrameResult<Input<'_>> {
    Ok(match value {
        NativeIdentityRef::Int(n) => Input::Int(n),
        NativeIdentityRef::UInt(n) => Input::UInt(n),
        NativeIdentityRef::Real(n) => Input::Real(f64::from_bits(n)),
        NativeIdentityRef::Float32(n) => Input::Float32(f64::from_bits(n)),
        NativeIdentityRef::Decimal {
            negative,
            coefficient,
            scale,
            storage_scale,
            declared_shape,
        } => Input::Decimal(NativeDecimalParseRef {
            negative,
            digits: coefficient,
            scale,
            storage_scale,
            declared_shape,
        }),
        _ => return Err(NativeIdentityFrameError::Invalid),
    })
}
fn encode_actual(value: crate::NativeFromUnixTimeValue) -> FrameResult<Option<Vec<u8>>> {
    use crate::NativeFromUnixTimeValue as V;
    match value {
        V::Null => Ok(None),
        V::Ready(v) => encode_epoch(v.epoch, None).map(Some),
        V::Truncate { value, message } => encode_epoch(value.epoch, Some(&message)).map(Some),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeFromUnixTimeEpoch {
    pub seconds: i64,
    pub micros: u32,
    pub fsp: u8,
}

/// The warning variant retains the actual computed epoch, so the original frame
/// can be forwarded after the caller's HandleTruncate policy succeeds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeFromUnixTimeResult<'a> {
    Continue(NativeFromUnixTimeEpoch),
    Truncate {
        epoch: NativeFromUnixTimeEpoch,
        message: &'a str,
    },
}

pub fn decode_native_from_unixtime_result(bytes: &[u8]) -> Option<NativeFromUnixTimeResult<'_>> {
    let tag = *bytes.first()?;
    if !matches!(tag, 0 | 1) || bytes.len() < 14 || (tag == 0 && bytes.len() != 14) {
        return None;
    }
    let epoch = NativeFromUnixTimeEpoch {
        seconds: i64::from_le_bytes(bytes[1..9].try_into().ok()?),
        micros: u32::from_le_bytes(bytes[9..13].try_into().ok()?),
        fsp: bytes[13],
    };
    if epoch.micros >= 1_000_000 || epoch.fsp > 6 {
        return None;
    }
    Some(if tag == 0 {
        NativeFromUnixTimeResult::Continue(epoch)
    } else {
        NativeFromUnixTimeResult::Truncate {
            epoch,
            message: std::str::from_utf8(&bytes[14..]).ok()?,
        }
    })
}

pub fn from_unixtime_numeric_native_args_valid(value: Option<&[u8]>) -> bool {
    matches!(
        value.and_then(|bytes| decode_native_identity(bytes).ok()),
        Some(
            NativeIdentityRef::Int(_)
                | NativeIdentityRef::UInt(_)
                | NativeIdentityRef::Decimal { .. }
                | NativeIdentityRef::Real(_)
                | NativeIdentityRef::Float32(_)
        )
    )
}

pub fn from_unixtime_text_native_args_valid(value: Option<&[u8]>) -> bool {
    value.is_some_and(|bytes| std::str::from_utf8(bytes).is_ok())
}

pub fn from_unixtime_local_native_args_valid(value: Option<&[u8]>) -> bool {
    value.and_then(decode_native_from_unixtime_result).is_some()
}

pub fn from_unixtime_legacy_args_valid(value: Option<&[u8]>) -> bool {
    matches!(
        value.and_then(|bytes| decode_native_identity(bytes).ok()),
        Some(NativeIdentityRef::Decimal { .. })
    )
}

pub fn from_unixtime_null_native_args_valid(value: Option<&[u8]>) -> bool {
    value.is_none()
}

fn encode_epoch(epoch: NativeFromUnixTimeEpoch, message: Option<&str>) -> FrameResult<Vec<u8>> {
    let length = 14usize
        .checked_add(message.map_or(0, str::len))
        .ok_or(NativeIdentityFrameError::Capacity)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| NativeIdentityFrameError::Capacity)?;
    bytes.push(u8::from(message.is_some()));
    bytes.extend_from_slice(&epoch.seconds.to_le_bytes());
    bytes.extend_from_slice(&epoch.micros.to_le_bytes());
    bytes.push(epoch.fsp);
    if let Some(message) = message {
        bytes.extend_from_slice(message.as_bytes());
    }
    Ok(bytes)
}

pub(super) fn rounded_epoch(total_nanos: i128, fsp: usize) -> Option<NativeFromUnixTimeEpoch> {
    let integral = total_nanos / 1_000_000_000;
    if integral > 32_536_771_199 {
        return None;
    }
    let factor = 10_i128.pow(9 - fsp as u32);
    let rounded = (total_nanos + factor / 2) / factor * factor;
    Some(NativeFromUnixTimeEpoch {
        seconds: (rounded / 1_000_000_000) as i64,
        micros: ((rounded % 1_000_000_000) / 1000) as u32,
        fsp: fsp as u8,
    })
}

pub(super) fn parse_epoch(text: &str, fsp: usize) -> Option<NativeFromUnixTimeEpoch> {
    let text = text.trim();
    let (int_part, frac_part) = text.split_once('.').unwrap_or((text, ""));
    let int_part = int_part.parse::<i64>().ok()?;
    if int_part < 0 || frac_part.starts_with('-') {
        return None;
    }
    let frac_digits: String = frac_part.chars().take(9).collect();
    if !frac_digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let frac_nanos: i128 = if frac_digits.is_empty() {
        0
    } else {
        format!("{frac_digits:0<9}").parse().unwrap()
    };
    rounded_epoch(i128::from(int_part) * 1_000_000_000 + frac_nanos, fsp)
}

pub(crate) fn evaluate_from_unixtime_numeric_native(
    value: NativeIdentityRef<'_>,
) -> FrameResult<Option<Vec<u8>>> {
    encode_actual(
        actual::native_from_unixtime_numeric_values_budget(actual_input(value)?, Budget(None))
            .map_err(actual_error)?,
    )
}

pub(crate) fn evaluate_from_unixtime_text_native(text: &str) -> FrameResult<Option<Vec<u8>>> {
    encode_actual(
        actual::native_from_unixtime_text_values_budget(text, Budget(None))
            .map_err(actual_error)?,
    )
}

/// The original native instant-to-local policy, including raw fixed offsets and
/// ordinary u32 microsecond multiplication. This is not wire timezone coercion.
pub fn native_from_unixtime_instant_to_local(
    secs: i64,
    micros: u32,
    zone: &NativeSessionTimeZone,
) -> Option<NaiveDateTime> {
    let utc = chrono::DateTime::<Utc>::from_timestamp(secs, micros * 1000)?;
    Some(match zone {
        NativeSessionTimeZone::Local => utc.with_timezone(&chrono::Local).naive_local(),
        NativeSessionTimeZone::Fixed { offset_secs, .. } => {
            (utc + chrono::Duration::seconds(i64::from(*offset_secs))).naive_utc()
        }
        NativeSessionTimeZone::Named(tz) => utc.with_timezone(tz).naive_local(),
    })
}

pub(super) fn format_local(local: NaiveDateTime, fsp: usize) -> String {
    let mut out = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        local.year(),
        local.month(),
        local.day(),
        local.hour(),
        local.minute(),
        local.second()
    );
    if fsp > 0 {
        let micros = local.and_utc().timestamp_subsec_micros();
        let shown = micros / 10_u32.pow(6 - fsp as u32);
        out.push('.');
        out.push_str(&format!("{shown:0fsp$}"));
    }
    out
}

pub(crate) fn evaluate_from_unixtime_local_native(
    result: NativeFromUnixTimeResult<'_>,
    zone: &NativeSessionTimeZone,
) -> Option<String> {
    let epoch = match result {
        NativeFromUnixTimeResult::Continue(v)
        | NativeFromUnixTimeResult::Truncate { epoch: v, .. } => v,
    };
    crate::NativeFromUnixTimeValues { epoch }
        .local_budget(zone, Budget(None))
        .expect("wire compatibility has no actual-work limit")
}

pub(crate) fn evaluate_from_unixtime_legacy(
    value: NativeIdentityRef<'_>,
    zone: &NativeSessionTimeZone,
) -> FrameResult<Option<Vec<u8>>> {
    let Input::Decimal(value) = actual_input(value)? else {
        return Err(NativeIdentityFrameError::Invalid);
    };
    let result = actual::native_from_unixtime_legacy_values_budget(Some(value), zone, Budget(None))
        .map_err(actual_error)?;
    result
        .map(|t| {
            encode_native_identity(NativeIdentityRef::Time {
                core: t.raw,
                kind: 1,
                fsp: t.fsp,
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use tidb_query_datatype::codec::mysql::Time;

    use super::*;

    #[test]
    fn from_unixtime_core_keeps_source_kinds_warning_epoch_and_legacy_fraction() {
        let integer = evaluate_from_unixtime_numeric_native(NativeIdentityRef::Int(42))
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_from_unixtime_result(&integer),
            Some(NativeFromUnixTimeResult::Continue(
                NativeFromUnixTimeEpoch {
                    seconds: 42,
                    micros: 0,
                    fsp: 0
                }
            ))
        );
        assert!(
            evaluate_from_unixtime_numeric_native(NativeIdentityRef::UInt(u64::MAX))
                .unwrap()
                .is_none()
        );
        let float = evaluate_from_unixtime_numeric_native(NativeIdentityRef::Float32(
            1.00000051_f64.to_bits(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(
            decode_native_from_unixtime_result(&float),
            Some(NativeFromUnixTimeResult::Continue(
                NativeFromUnixTimeEpoch {
                    seconds: 1,
                    micros: 1,
                    fsp: 6
                }
            ))
        );
        for (text, seconds, micros) in [
            ("-0.5", 0, 500_000),
            ("1.123456789junk", 1, 123_457),
            ("32536771199.9999999", 32_536_771_200, 0),
        ] {
            let bytes = evaluate_from_unixtime_text_native(text).unwrap().unwrap();
            assert_eq!(
                decode_native_from_unixtime_result(&bytes),
                Some(NativeFromUnixTimeResult::Continue(
                    NativeFromUnixTimeEpoch {
                        seconds,
                        micros,
                        fsp: 6
                    }
                ))
            );
        }
        assert!(
            evaluate_from_unixtime_text_native("1.0e2")
                .unwrap()
                .is_none()
        );
        let warning = evaluate_from_unixtime_text_native(" 1e2 ")
            .unwrap()
            .unwrap();
        let decoded = decode_native_from_unixtime_result(&warning).unwrap();
        assert_eq!(
            decoded,
            NativeFromUnixTimeResult::Truncate {
                epoch: NativeFromUnixTimeEpoch {
                    seconds: 0,
                    micros: 0,
                    fsp: 0
                },
                message: "Truncated incorrect DECIMAL value: '1e2'"
            }
        );
        let raw_zone = NativeSessionTimeZone::Fixed {
            name: String::new(),
            offset_secs: 86_400,
        };
        assert_eq!(
            evaluate_from_unixtime_local_native(decoded, &raw_zone).as_deref(),
            Some("1970-01-02 00:00:00")
        );
        let hidden = NativeIdentityRef::Decimal {
            negative: false,
            coefficient: b"1255",
            scale: 2,
            storage_scale: 3,
            declared_shape: Some((30, 9)),
        };
        let hidden = evaluate_from_unixtime_numeric_native(hidden)
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_native_from_unixtime_result(&hidden),
            Some(NativeFromUnixTimeResult::Continue(
                NativeFromUnixTimeEpoch {
                    seconds: 1,
                    micros: 260_000,
                    fsp: 2
                }
            ))
        );
        let utc = NativeSessionTimeZone::utc();
        // This raw coefficient is shorter than its storage scale; source Display
        // panics rather than admitting it to a numeric/math validator.
        assert!(
            std::panic::catch_unwind(|| evaluate_from_unixtime_legacy(
                NativeIdentityRef::Decimal {
                    negative: false,
                    coefficient: b"1",
                    scale: 6,
                    storage_scale: 6,
                    declared_shape: None,
                },
                &utc
            ))
            .is_err()
        );
        // Valid legacy input below retains the source's extra nanos*1000.
        let legacy = evaluate_from_unixtime_legacy(
            NativeIdentityRef::Decimal {
                negative: false,
                coefficient: b"000001",
                scale: 6,
                storage_scale: 6,
                declared_shape: None,
            },
            &utc,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            decode_native_identity(&legacy).unwrap(),
            NativeIdentityRef::Time {
                core: Time::native_core_from_fields(1970, 1, 1, 0, 0, 0, 1000),
                kind: 1,
                fsp: 0
            }
        );
    }

    #[test]
    fn from_unixtime_epoch_codec_and_admission_keep_actual_raw_domains() {
        let epoch = NativeFromUnixTimeEpoch {
            seconds: i64::MIN,
            micros: 999_999,
            fsp: 6,
        };
        let plain = encode_epoch(epoch, None).unwrap();
        assert_eq!(plain.len(), 14);
        assert_eq!(
            decode_native_from_unixtime_result(&plain),
            Some(NativeFromUnixTimeResult::Continue(epoch))
        );
        let warned = encode_epoch(epoch, Some("界\0")).unwrap();
        assert_eq!(
            decode_native_from_unixtime_result(&warned),
            Some(NativeFromUnixTimeResult::Truncate {
                epoch,
                message: "界\0"
            })
        );
        for length in 0..14 {
            assert!(decode_native_from_unixtime_result(&plain[..length]).is_none());
        }
        let mut extra = plain.clone();
        extra.push(0);
        assert!(decode_native_from_unixtime_result(&extra).is_none());
        for bad in [
            encode_epoch(
                NativeFromUnixTimeEpoch {
                    micros: 1_000_000,
                    ..epoch
                },
                None,
            )
            .unwrap(),
            encode_epoch(NativeFromUnixTimeEpoch { fsp: 7, ..epoch }, None).unwrap(),
        ] {
            assert!(decode_native_from_unixtime_result(&bad).is_none());
        }
        let mut invalid_utf8 = encode_epoch(epoch, Some("")).unwrap();
        invalid_utf8.push(255);
        assert!(decode_native_from_unixtime_result(&invalid_utf8).is_none());
        let raw = encode_native_identity(NativeIdentityRef::Decimal {
            negative: true,
            coefficient: &[255],
            scale: 1,
            storage_scale: 0,
            declared_shape: None,
        })
        .unwrap();
        assert!(from_unixtime_numeric_native_args_valid(Some(&raw)));
        assert!(from_unixtime_legacy_args_valid(Some(&raw)));
        assert!(
            std::panic::catch_unwind(|| evaluate_from_unixtime_numeric_native(
                decode_native_identity(&raw).unwrap()
            ))
            .is_err()
        );
        assert!(from_unixtime_local_native_args_valid(Some(&warned)));
        assert!(!from_unixtime_numeric_native_args_valid(None));
        assert!(!from_unixtime_text_native_args_valid(None));
        assert!(!from_unixtime_text_native_args_valid(Some(&[255])));
        assert!(from_unixtime_text_native_args_valid(Some(b"")));
        assert!(from_unixtime_null_native_args_valid(None));
        assert!(!from_unixtime_null_native_args_valid(Some(b"")));
    }
}
