// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! TIMESTAMP's fixed parse and add stages, preserving RHS coercion demand.

use tidb_query_datatype::codec::mysql::{
    Time, TimeType,
    time::{NativeSessionTimeZone, NativeTemporalValue, native_parse_time},
};

use crate::{
    NativeDurationDateTime, NativeGoDuration, NativeIdentityFrameError, NativeIdentityRef,
    decode_native_identity, encode_native_identity, native_duration_fsp, parse_native_duration,
};

/// Borrowed first-stage output. A Base requests RHS coercion even for year
/// zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeTimestampResult<'a> {
    Value(&'a str),
    Base(NativeTemporalValue),
    Warning { code: u16, message: &'a str },
}

fn decode_base(bytes: &[u8]) -> Option<NativeTemporalValue> {
    match decode_native_identity(bytes).ok()? {
        NativeIdentityRef::Time { core, kind: 1, fsp } => Some(NativeTemporalValue {
            raw: core,
            kind: TimeType::DateTime,
            fsp,
        }),
        _ => None,
    }
}

/// Decode only the three fixed first-stage representations; no calendar or raw
/// FSP normalization is performed on the actual computed base.
pub fn decode_native_timestamp_result(bytes: &[u8]) -> Option<NativeTimestampResult<'_>> {
    match bytes.first()? {
        16 => Some(NativeTimestampResult::Value(
            std::str::from_utf8(&bytes[1..]).ok()?,
        )),
        15 => decode_base(bytes).map(NativeTimestampResult::Base),
        17 => {
            let code = u16::from_le_bytes(bytes.get(1..3)?.try_into().ok()?);
            if code != 1292 {
                return None;
            }
            Some(NativeTimestampResult::Warning {
                code,
                message: std::str::from_utf8(bytes.get(3..)?).ok()?,
            })
        }
        _ => None,
    }
}

/// The head has actual non-NULL text and its original runtime numeric-kind bit.
pub fn timestamp_parse_native_args_valid(value: Option<&[u8]>, is_float: Option<i64>) -> bool {
    value.is_some_and(|bytes| std::str::from_utf8(bytes).is_ok())
        && is_float.is_some_and(|flag| matches!(flag, 0 | 1))
}

/// The actual first-stage base is mandatory even when the actual RHS is NULL.
pub fn timestamp_add_native_args_valid(base: Option<&[u8]>, rhs: Option<&[u8]>) -> bool {
    base.and_then(decode_base).is_some()
        && rhs.map_or(true, |bytes| std::str::from_utf8(bytes).is_ok())
}

/// This recipe witnesses the original SQL-NULL first operand, without a zone.
pub fn timestamp_null_native_args_valid(value: Option<&[u8]>) -> bool {
    value.is_none()
}

fn parse_base(
    text: &str,
    is_float: bool,
    zone: &NativeSessionTimeZone,
) -> Result<NativeTemporalValue, String> {
    // This is the duration family's FIRST-dot precision, not native_get_time_fsp.
    native_parse_time(
        text,
        TimeType::DateTime,
        i64::from(native_duration_fsp(text)),
        is_float,
        true,
        false,
        true,
        zone,
    )
    .map(|parsed| parsed.time)
    .map_err(|_| format!("Incorrect datetime value: '{text}'"))
}

fn duration_datetime(time: NativeTemporalValue) -> NativeDurationDateTime {
    // Only an observational field view; no DATE projection, validation or
    // packed-time conversion is applied to the computed base.
    let [year, month, day, hour, minute, second, micros] = Time::native_core_fields(time.raw);
    NativeDurationDateTime {
        year: i64::from(year),
        month: month as u32,
        day: day as u32,
        hour: hour as u32,
        minute: minute as u32,
        second: second as u32,
        micros: micros as u32,
        fsp: i32::from(time.fsp),
    }
}

fn encode_text(tag: u8, prefix: &[u8], text: &str) -> Result<Vec<u8>, NativeIdentityFrameError> {
    let length = 1usize
        .checked_add(prefix.len())
        .and_then(|len| len.checked_add(text.len()))
        .ok_or(NativeIdentityFrameError::Capacity)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| NativeIdentityFrameError::Capacity)?;
    bytes.push(tag);
    bytes.extend_from_slice(prefix);
    bytes.extend_from_slice(text.as_bytes());
    Ok(bytes)
}

pub(crate) fn evaluate_timestamp1_native(
    text: &str,
    is_float: bool,
    zone: &NativeSessionTimeZone,
) -> Result<Vec<u8>, NativeIdentityFrameError> {
    match parse_base(text, is_float, zone) {
        Ok(base) => encode_text(16, &[], &duration_datetime(base).format()),
        Err(message) => encode_text(17, &1292u16.to_le_bytes(), &message),
    }
}

pub(crate) fn evaluate_timestamp2_base_native(
    text: &str,
    is_float: bool,
    zone: &NativeSessionTimeZone,
) -> Result<Vec<u8>, NativeIdentityFrameError> {
    match parse_base(text, is_float, zone) {
        Ok(base) => encode_native_identity(NativeIdentityRef::Time {
            core: base.raw,
            kind: match base.kind {
                TimeType::Date => 0,
                TimeType::DateTime => 1,
                TimeType::Timestamp => 2,
            },
            fsp: base.fsp,
        }),
        Err(message) => encode_text(17, &1292u16.to_le_bytes(), &message),
    }
}

/// Runs only after the frontend has coerced the demanded RHS. Year zero must
/// not become a first-stage stop disposition: that would hide RHS errors.
pub(crate) fn evaluate_timestamp2_add_native(
    base: NativeTemporalValue,
    rhs: Option<&str>,
) -> Option<String> {
    let base = duration_datetime(base);
    let second = rhs?;
    if base.year == 0 || !NativeGoDuration::is_duration(second) {
        return None;
    }
    let delta = parse_native_duration(second, native_duration_fsp(second)).ok()?;
    match base.add(delta) {
        Some(result) if result.in_range() => Some(
            NativeDurationDateTime {
                fsp: base.fsp.max(delta.fsp),
                ..result
            }
            .format(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_stages_preserve_numeric_kind_first_dot_warning_and_rhs_demand() {
        let zone = NativeSessionTimeZone::utc();
        for (text, numeric, expected) in [
            ("20240315.5", false, "2024-03-15 05:00:00.0"),
            ("20240315.5", true, "2024-03-15 00:00:00.0"),
            ("2024.03.15 12:34:56.1", false, "2024-03-15 12:34:56.100000"),
            ("0.0123", true, "0000-00-00 00:00:00"),
        ] {
            let bytes = evaluate_timestamp1_native(text, numeric, &zone).unwrap();
            assert_eq!(
                decode_native_timestamp_result(&bytes),
                Some(NativeTimestampResult::Value(expected))
            );
        }
        for bytes in [
            evaluate_timestamp1_native("bad-left", false, &zone).unwrap(),
            evaluate_timestamp2_base_native("bad-left", false, &zone).unwrap(),
        ] {
            assert_eq!(
                decode_native_timestamp_result(&bytes),
                Some(NativeTimestampResult::Warning {
                    code: 1292,
                    message: "Incorrect datetime value: 'bad-left'",
                })
            );
        }
        let zero_bytes = evaluate_timestamp2_base_native("0000-01-01", false, &zone).unwrap();
        let Some(NativeTimestampResult::Base(zero)) = decode_native_timestamp_result(&zero_bytes)
        else {
            panic!("zero year still demands RHS coercion")
        };
        assert_eq!(evaluate_timestamp2_add_native(zero, Some("00:00:01")), None);
        let bytes =
            evaluate_timestamp2_base_native("2024-01-01 23:59:59.123", false, &zone).unwrap();
        let base = decode_base(&bytes).unwrap();
        assert_eq!(
            evaluate_timestamp2_add_native(base, Some("00:00:00.456789")).as_deref(),
            Some("2024-01-01 23:59:59.579789")
        );
        assert_eq!(
            evaluate_timestamp2_add_native(base, Some("00:00:01")).as_deref(),
            Some("2024-01-02 00:00:00.123")
        );
        for rhs in [
            None,
            Some("not duration"),
            Some("2017-01-01 01:00:00"),
            Some("839:00:00"),
        ] {
            assert!(evaluate_timestamp2_add_native(base, rhs).is_none());
        }
        let la = NativeSessionTimeZone::Named("America/Los_Angeles".parse().unwrap());
        let carried =
            evaluate_timestamp1_native("2011-03-13 01:59:59.9999999", false, &la).unwrap();
        assert_eq!(
            decode_native_timestamp_result(&carried),
            Some(NativeTimestampResult::Value("2011-03-13 03:00:00.000000"))
        );
    }

    #[test]
    fn timestamp_codec_preserves_base_bits_and_rejects_malformed_stage_inputs() {
        let base = NativeTemporalValue {
            raw: u64::MAX,
            kind: TimeType::DateTime,
            fsp: u8::MAX,
        };
        let frame = encode_native_identity(NativeIdentityRef::Time {
            core: base.raw,
            kind: 1,
            fsp: base.fsp,
        })
        .unwrap();
        assert_eq!(frame.len(), 11);
        assert_eq!(
            decode_native_timestamp_result(&frame),
            Some(NativeTimestampResult::Base(base))
        );
        assert!(timestamp_add_native_args_valid(Some(&frame), None));
        assert!(!timestamp_add_native_args_valid(Some(&frame), Some(&[255])));
        assert!(!timestamp_add_native_args_valid(None, None));
        for kind in [0, 2] {
            let wrong = encode_native_identity(NativeIdentityRef::Time {
                core: 0,
                kind,
                fsp: 0,
            })
            .unwrap();
            assert!(decode_native_timestamp_result(&wrong).is_none());
            assert!(!timestamp_add_native_args_valid(Some(&wrong), None));
        }
        for length in 0..frame.len() {
            assert!(decode_native_timestamp_result(&frame[..length]).is_none());
        }
        let mut extra = frame;
        extra.push(0);
        assert!(decode_native_timestamp_result(&extra).is_none());
        for bytes in [
            vec![],
            vec![16, 255],
            vec![17],
            vec![17, 12],
            vec![17, 0, 0],
            vec![17, 12, 5, 255],
            vec![0],
        ] {
            assert!(decode_native_timestamp_result(&bytes).is_none());
        }
        let message =
            encode_text(17, &1292u16.to_le_bytes(), "Incorrect datetime value: '界'").unwrap();
        assert_eq!(
            decode_native_timestamp_result(&message),
            Some(NativeTimestampResult::Warning {
                code: 1292,
                message: "Incorrect datetime value: '界'",
            })
        );
        assert!(timestamp_parse_native_args_valid(Some(b""), Some(0)));
        assert!(timestamp_parse_native_args_valid(Some(b"0.0"), Some(1)));
        assert!(!timestamp_parse_native_args_valid(None, Some(0)));
        assert!(!timestamp_parse_native_args_valid(Some(&[255]), Some(0)));
        assert!(!timestamp_parse_native_args_valid(Some(b"x"), Some(2)));
        assert!(timestamp_null_native_args_valid(None));
        assert!(!timestamp_null_native_args_valid(Some(b"")));
    }
}
