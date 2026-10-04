// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Typed temporal literal policy, including its construction-time hard errors.

use std::sync::OnceLock;

use regex::Regex;
use tidb_query_datatype::codec::mysql::{
    Time, TimeType,
    time::{NativeSessionTimeZone, NativeTemporalValue, native_get_time_fsp, native_parse_time},
};

use crate::{
    NativeIdentityFrameError, NativeIdentityRef, decode_native_identity, encode_native_identity,
};

/// A borrowed view of the actual temporal value or the original hard error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeTemporalLiteralResult<'a> {
    Value(NativeTemporalValue),
    WrongValue { code: u16, message: &'a str },
}

/// Decode only a Time identity frame or the disjoint literal-error frame.
/// Calendar bits and raw FSP are not normalized by this representation view.
pub fn decode_native_temporal_literal_result(
    bytes: &[u8],
) -> Option<NativeTemporalLiteralResult<'_>> {
    if bytes.first() == Some(&0) {
        let code = u16::from_le_bytes(bytes.get(1..3)?.try_into().ok()?);
        if !matches!(code, 1292 | 1525) {
            return None;
        }
        return Some(NativeTemporalLiteralResult::WrongValue {
            code,
            message: std::str::from_utf8(bytes.get(3..)?).ok()?,
        });
    }
    let NativeIdentityRef::Time { core, kind, fsp } = decode_native_identity(bytes).ok()? else {
        return None;
    };
    let kind = match kind {
        0 => TimeType::Date,
        1 => TimeType::DateTime,
        2 => TimeType::Timestamp,
        _ => return None,
    };
    Some(NativeTemporalLiteralResult::Value(NativeTemporalValue {
        raw: core,
        kind,
        fsp,
    }))
}

/// Only transport presence, UTF-8 and the three actual SQL-mode bits are
/// checked.
pub fn temporal_literal_native_args_valid(value: Option<&[u8]>, modes: Option<i64>) -> bool {
    value.is_some_and(|bytes| std::str::from_utf8(bytes).is_ok())
        && modes.is_some_and(|bits| (0..=7).contains(&bits))
}

fn timestamp_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(concat!(
            r"^",
            r"[\t\n\x0C\r ]*0*",
            r"[0-9]{1,4}",
            r"([^0-9]0*[0-9]{1,2}){2}",
            r"[\t\n\x0C\r ]+",
            r"0*[0-9]{1,2}",
            r"([^0-9]0*[0-9]{1,2}){0,2}",
            r"(\.[0-9]*)?",
            r"([+-][0-9]{2}[:][0-9]{2})?",
            r"[\t\n\x0C\r ]*$",
        ))
        .expect("timestampPattern is a valid regex")
    })
}

fn date_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"^[\t\n\x0C\r ]*((0*[0-9]{1,4}([^0-9]0*[0-9]{1,2}){2})|([0-9]{2,4}([0-9]{2}){2}))[\t\n\x0C\r ]*$",
        )
        .expect("datePattern is a valid regex")
    })
}

fn wrong_value(code: u16, kind: &str, text: &str) -> (u16, String) {
    (code, format!("Incorrect {kind} value: '{text}'"))
}

fn unpadded_datetime_message(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let parts: Vec<&str> = trimmed.splitn(3, '-').collect();
    if parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    {
        return Some(format!(
            "Incorrect datetime value: '{}-{}-{}'",
            parts[0].parse::<i64>().unwrap_or(0),
            parts[1].parse::<i64>().unwrap_or(0),
            parts[2].parse::<i64>().unwrap_or(0)
        ));
    }
    None
}

fn date_value(
    text: &str,
    zone: &NativeSessionTimeZone,
    modes: i64,
) -> Result<NativeTemporalValue, (u16, String)> {
    if !date_pattern().is_match(text) {
        return Err(wrong_value(1292, "date", text));
    }
    let time = native_parse_time(
        text,
        TimeType::Date,
        0,
        false,
        true,
        modes & 1 != 0,
        true,
        zone,
    )
    .map(|parsed| parsed.time)
    .map_err(|_| {
        (
            1292,
            unpadded_datetime_message(text)
                .unwrap_or_else(|| format!("Incorrect datetime value: '{text}'")),
        )
    })?;
    if modes & 2 != 0 && time.raw == 0 {
        return Err(wrong_value(1292, "date", text));
    }
    if modes & 4 != 0
        && (Time::month_from_core_bits(time.raw) == 0 || Time::day_from_core_bits(time.raw) == 0)
        && time.raw != 0
    {
        return Err(wrong_value(1292, "date", text));
    }
    Ok(time)
}

fn timestamp_value(
    text: &str,
    zone: &NativeSessionTimeZone,
    modes: i64,
) -> Result<NativeTemporalValue, (u16, String)> {
    if !timestamp_pattern().is_match(text) {
        return Err(wrong_value(1525, "datetime", text));
    }
    native_parse_time(
        text,
        TimeType::DateTime,
        i64::from(native_get_time_fsp(text)),
        false,
        true,
        modes & 1 != 0,
        true,
        zone,
    )
    .map(|parsed| parsed.time)
    .map_err(|_| wrong_value(1292, "datetime", text))
}

fn encode_result(
    result: Result<NativeTemporalValue, (u16, String)>,
) -> Result<Vec<u8>, NativeIdentityFrameError> {
    match result {
        Ok(time) => encode_native_identity(NativeIdentityRef::Time {
            core: time.raw,
            kind: match time.kind {
                TimeType::Date => 0,
                TimeType::DateTime => 1,
                TimeType::Timestamp => 2,
            },
            fsp: time.fsp,
        }),
        Err((code, message)) => {
            if !matches!(code, 1292 | 1525) {
                return Err(NativeIdentityFrameError::Invalid);
            }
            let length = message
                .len()
                .checked_add(3)
                .ok_or(NativeIdentityFrameError::Capacity)?;
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(length)
                .map_err(|_| NativeIdentityFrameError::Capacity)?;
            bytes.push(0);
            bytes.extend_from_slice(&code.to_le_bytes());
            bytes.extend_from_slice(message.as_bytes());
            Ok(bytes)
        }
    }
}

pub(crate) fn evaluate_native_date_literal(
    text: &str,
    zone: &NativeSessionTimeZone,
    modes: i64,
) -> Result<Vec<u8>, NativeIdentityFrameError> {
    encode_result(date_value(text, zone, modes))
}

pub(crate) fn evaluate_native_timestamp_literal(
    text: &str,
    zone: &NativeSessionTimeZone,
    modes: i64,
) -> Result<Vec<u8>, NativeIdentityFrameError> {
    encode_result(timestamp_value(text, zone, modes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_core_preserves_gates_modes_errors_precision_and_session_carry() {
        let utc = NativeSessionTimeZone::utc();
        assert_eq!(
            date_value("2020-02-30", &utc, 0),
            Err((1292, "Incorrect datetime value: '2020-2-30'".to_owned()))
        );
        assert!(date_value("2020-02-30", &utc, 1).is_ok());
        assert_eq!(
            date_value("2020-01-01 01:02:03", &utc, 0),
            Err(wrong_value(1292, "date", "2020-01-01 01:02:03"))
        );
        assert!(date_value("0000-00-00", &utc, 4).is_ok());
        assert_eq!(
            date_value("0000-00-00", &utc, 2),
            Err(wrong_value(1292, "date", "0000-00-00"))
        );
        assert!(date_value("2007-10-00", &utc, 2).is_ok());
        assert_eq!(
            date_value("2007-10-00", &utc, 4),
            Err(wrong_value(1292, "date", "2007-10-00"))
        );
        assert_eq!(
            timestamp_value("2024-01-01", &utc, 0),
            Err(wrong_value(1525, "datetime", "2024-01-01"))
        );
        assert_eq!(
            timestamp_value("2024-01-01 14:00:00+14:01", &utc, 0),
            Err(wrong_value(1292, "datetime", "2024-01-01 14:00:00+14:01"))
        );
        assert!(timestamp_value("0000-00-00 00:00:00", &utc, 6).is_ok());
        let precise = timestamp_value("2024-01-01 14:00:00.010", &utc, 0).unwrap();
        assert_eq!((precise.kind, precise.fsp), (TimeType::DateTime, 3));
        assert_eq!(
            precise.raw,
            Time::native_core_from_fields(2024, 1, 1, 14, 0, 0, 10_000)
        );
        let la = NativeSessionTimeZone::Named("America/Los_Angeles".parse().unwrap());
        let carried = timestamp_value("2011-03-13 01:59:59.9999999", &la, 0).unwrap();
        assert_eq!(
            (carried.raw, carried.fsp),
            (Time::native_core_from_fields(2011, 3, 13, 3, 0, 0, 0), 6)
        );
        let offset = timestamp_value("2024-01-01 14:00:00+02:00", &utc, 0).unwrap();
        assert_eq!(
            offset.raw,
            Time::native_core_from_fields(2024, 1, 1, 12, 0, 0, 0)
        );
        assert!(!timestamp_pattern().is_match("\u{000b}2024-01-01 14:00:00"));
        assert!(!date_pattern().is_match("２０２４-01-01"));
    }

    #[test]
    fn literal_result_codec_keeps_actual_time_and_checked_borrowed_errors() {
        for kind in [TimeType::Date, TimeType::DateTime, TimeType::Timestamp] {
            let value = NativeTemporalValue {
                raw: u64::MAX,
                kind,
                fsp: u8::MAX,
            };
            let bytes = encode_result(Ok(value)).unwrap();
            assert_eq!(bytes.len(), 11);
            assert_eq!(bytes[0], 15);
            assert_eq!(
                decode_native_temporal_literal_result(&bytes),
                Some(NativeTemporalLiteralResult::Value(value))
            );
            for length in 0..bytes.len() {
                assert!(decode_native_temporal_literal_result(&bytes[..length]).is_none());
            }
            let mut excess = bytes;
            excess.push(0);
            assert!(decode_native_temporal_literal_result(&excess).is_none());
        }
        for code in [1292, 1525] {
            let bytes =
                encode_result(Err((code, "Incorrect datetime value: '界'".to_owned()))).unwrap();
            assert_eq!(
                decode_native_temporal_literal_result(&bytes),
                Some(NativeTemporalLiteralResult::WrongValue {
                    code,
                    message: "Incorrect datetime value: '界'",
                })
            );
        }
        for bytes in [
            vec![],
            vec![0],
            vec![0, 12],
            vec![0, 0, 0],
            vec![0, 12, 5, 255],
            vec![15; 11],
        ] {
            assert!(decode_native_temporal_literal_result(&bytes).is_none());
        }
        let other = encode_native_identity(NativeIdentityRef::Int(42)).unwrap();
        assert!(decode_native_temporal_literal_result(&other).is_none());
        assert!(temporal_literal_native_args_valid(Some(b""), Some(7)));
        assert!(!temporal_literal_native_args_valid(None, Some(0)));
        assert!(!temporal_literal_native_args_valid(Some(b"x"), None));
        assert!(!temporal_literal_native_args_valid(Some(&[255]), Some(0)));
        assert!(!temporal_literal_native_args_valid(Some(b"x"), Some(8)));
        let utc = NativeSessionTimeZone::utc();
        assert!(matches!(
            decode_native_temporal_literal_result(
                &evaluate_native_date_literal("2024-01-01", &utc, 0).unwrap()
            ),
            Some(NativeTemporalLiteralResult::Value(_))
        ));
        assert!(matches!(
            decode_native_temporal_literal_result(
                &evaluate_native_timestamp_literal("2024-01-01", &utc, 0).unwrap()
            ),
            Some(NativeTemporalLiteralResult::WrongValue { code: 1525, .. })
        ));
    }
}
