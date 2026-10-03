// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native ADDTIME/SUBTIME signature policy over the shared temporal cores.
//! Metadata contains original type categories and row/value-kind facts only;
//! the operation's sign is selected by its fixed worker, never by input data.

use crate::{
    native_duration_parse::{NativeGoDuration, native_duration_fsp, parse_native_duration},
    native_time_parse::{NativeDurationDateTime, parse_native_duration_datetime},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NativeTimeAddKind {
    Datetime = 0,
    Date = 1,
    Duration = 2,
    Other = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeTimeAddMetadata {
    pub left: NativeTimeAddKind,
    pub right: NativeTimeAddKind,
    pub row_path: bool,
    pub right_binary: bool,
}

impl NativeTimeAddMetadata {
    pub fn encode(self) -> i64 {
        self.left as i64
            | (self.right as i64) << 2
            | i64::from(self.row_path) << 4
            | i64::from(self.right_binary) << 5
    }

    pub fn decode(raw: i64) -> Option<Self> {
        if !(0..64).contains(&raw) {
            return None;
        }
        let kind = |bits| match bits {
            0 => NativeTimeAddKind::Datetime,
            1 => NativeTimeAddKind::Date,
            2 => NativeTimeAddKind::Duration,
            _ => NativeTimeAddKind::Other,
        };
        Some(Self {
            left: kind(raw & 3),
            right: kind((raw >> 2) & 3),
            row_path: raw & 16 != 0,
            right_binary: raw & 32 != 0,
        })
    }
}

/// Ordinary signatures accept genuine SQL NULLs and otherwise only validate
/// physical UTF-8. The statically undemanded Datetime-RHS signature is
/// separate.
pub fn native_time_add_args_valid(
    left: Option<&[u8]>,
    right: Option<&[u8]>,
    metadata: Option<i64>,
) -> bool {
    metadata
        .and_then(NativeTimeAddMetadata::decode)
        .is_some_and(|metadata| {
            metadata.right != NativeTimeAddKind::Datetime
                && left.is_none_or(|bytes| std::str::from_utf8(bytes).is_ok())
                && right.is_none_or(|bytes| std::str::from_utf8(bytes).is_ok())
        })
}

/// The metadata-only NULL worker takes no fabricated operand NULL slots.
pub fn native_time_add_null_metadata_valid(metadata: Option<i64>) -> bool {
    metadata
        .and_then(NativeTimeAddMetadata::decode)
        .is_some_and(|metadata| metadata.right == NativeTimeAddKind::Datetime)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeTimeAddWarning {
    TruncatedLeft,
    TruncatedRight,
    IncorrectTimeLeft,
    IncorrectDateTimeLeft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeTimeAddResult<'a> {
    Value(&'a str),
    Warning(NativeTimeAddWarning),
}

/// Actual computed output: tag 0 plus a nonempty UTF-8 formatted value, or
/// exactly one warning tag. Silent NULL is outside the packet, as SQL NULL.
pub fn decode_native_time_add_result(bytes: &[u8]) -> Option<NativeTimeAddResult<'_>> {
    let (&tag, tail) = bytes.split_first()?;
    if tag == 0 {
        return (!tail.is_empty())
            .then(|| std::str::from_utf8(tail).ok())?
            .map(NativeTimeAddResult::Value);
    }
    if !tail.is_empty() {
        return None;
    }
    Some(NativeTimeAddResult::Warning(match tag {
        1 => NativeTimeAddWarning::TruncatedLeft,
        2 => NativeTimeAddWarning::TruncatedRight,
        3 => NativeTimeAddWarning::IncorrectTimeLeft,
        4 => NativeTimeAddWarning::IncorrectDateTimeLeft,
        _ => return None,
    }))
}

pub fn native_time_add_result_valid(bytes: &[u8]) -> bool {
    decode_native_time_add_result(bytes).is_some()
}

fn value_packet(value: String) -> Vec<u8> {
    // These original value formatters produce nonempty text. Allocation/count
    // failures must not be converted into the SQL NULL option below.
    let mut bytes = Vec::with_capacity(value.len() + 1);
    bytes.push(0);
    bytes.extend_from_slice(value.as_bytes());
    bytes
}

fn warning_packet(warning: NativeTimeAddWarning) -> Vec<u8> {
    vec![match warning {
        NativeTimeAddWarning::TruncatedLeft => 1,
        NativeTimeAddWarning::TruncatedRight => 2,
        NativeTimeAddWarning::IncorrectTimeLeft => 3,
        NativeTimeAddWarning::IncorrectDateTimeLeft => 4,
    }]
}

fn second_as_duration(
    text: &str,
    kind: NativeTimeAddKind,
    date_string_fsp: bool,
) -> Result<Option<NativeGoDuration>, NativeTimeAddWarning> {
    if kind != NativeTimeAddKind::Duration && !NativeGoDuration::is_duration(text) {
        return Ok(None);
    }
    let fsp = if date_string_fsp && kind != NativeTimeAddKind::Duration {
        NativeGoDuration::fsp_for_time_add_sub(text)
    } else {
        native_duration_fsp(text)
    };
    parse_native_duration(text, fsp)
        .map(Some)
        .map_err(|_| NativeTimeAddWarning::TruncatedRight)
}

fn datetime_result(text: &str, delta: NativeGoDuration, sign: i64) -> Option<Vec<u8>> {
    let first = parse_native_duration_datetime(text)?;
    if first.is_zero() {
        return None;
    }
    let signed = NativeGoDuration {
        micros: delta.micros * sign,
        ..delta
    };
    let result = first.add(signed)?;
    result.in_range().then(|| value_packet(result.format()))
}

fn string_datetime_result(text: &str, delta: NativeGoDuration, sign: i64) -> Option<Vec<u8>> {
    let Some(first) = parse_native_duration_datetime(text) else {
        let trimmed = text.trim();
        let warning = if !trimmed.is_empty()
            && trimmed.bytes().all(|byte| byte.is_ascii_digit())
            && trimmed
                .parse::<i64>()
                .map_or(true, |value| value > 99_991_231)
        {
            NativeTimeAddWarning::IncorrectTimeLeft
        } else {
            NativeTimeAddWarning::IncorrectDateTimeLeft
        };
        return Some(warning_packet(warning));
    };
    let first = NativeDurationDateTime { fsp: 6, ..first };
    let result = first.add(NativeGoDuration {
        micros: delta.micros * sign,
        ..delta
    })?;
    if !result.in_range() {
        return None;
    }
    let fsp = if result.micros == 0 { 0 } else { 6 };
    Some(value_packet(
        NativeDurationDateTime { fsp, ..result }.format(),
    ))
}

fn trailing_dash_group(text: &str) -> bool {
    let trimmed = text.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let digits = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if digits == 0 {
        return false;
    }
    matches!(trimmed[digits..].strip_prefix('-'), Some(rest) if !rest.is_empty())
}

/// All native signature bodies; operand coercion, warning replay and final
/// typed-return coercion remain at their original frontend boundaries.
pub(crate) fn evaluate_native_time_add(
    left: Option<&str>,
    right: Option<&str>,
    metadata: NativeTimeAddMetadata,
    sign: i64,
) -> Option<Vec<u8>> {
    use NativeTimeAddKind::*;
    if metadata.right == Datetime {
        return None;
    }
    let (Some(left), Some(right)) = (left, right) else {
        return None;
    };
    match metadata.left {
        Datetime | Date => {
            // These signatures demand the RHS parser before looking at lhs.
            let delta = match second_as_duration(right, metadata.right, metadata.left == Date) {
                Ok(Some(delta)) => delta,
                Ok(None) => return None,
                Err(warning) => return Some(warning_packet(warning)),
            };
            let delta =
                if metadata.left == Datetime && !metadata.row_path && metadata.right == Duration {
                    NativeGoDuration { fsp: -1, ..delta }
                } else {
                    delta
                };
            datetime_result(left, delta, sign)
        }
        Duration => {
            let Ok(first) = parse_native_duration(left, native_duration_fsp(left)) else {
                return Some(warning_packet(NativeTimeAddWarning::TruncatedLeft));
            };
            let delta = match second_as_duration(right, metadata.right, false) {
                Ok(Some(delta)) => delta,
                Ok(None) => return None,
                Err(warning) => return Some(warning_packet(warning)),
            };
            Some(value_packet(first.combine(delta, sign).format()))
        }
        Other => {
            let fsp = if metadata.right == Duration {
                6
            } else {
                NativeGoDuration::fsp_for_time_add_sub(right)
            };
            let delta = match parse_native_duration(right, fsp) {
                Ok(delta) => delta,
                Err(_) if metadata.right != Duration && metadata.right_binary => return None,
                Err(_) => return Some(warning_packet(NativeTimeAddWarning::TruncatedRight)),
            };
            if metadata.row_path
                && sign > 0
                && metadata.right != Duration
                && trailing_dash_group(right)
            {
                return None;
            }
            if NativeGoDuration::is_duration(left) {
                let Ok(first) = parse_native_duration(left, 6) else {
                    return Some(warning_packet(NativeTimeAddWarning::TruncatedLeft));
                };
                let sum = first.combine(delta, sign);
                let fsp = if sum.micro_second() == 0 { 0 } else { 6 };
                return Some(value_packet(NativeGoDuration { fsp, ..sum }.format()));
            }
            string_datetime_result(left, delta, sign)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_time_add_matrix_order_row_modes_binary_warnings_and_packets() {
        use NativeTimeAddKind::*;
        let metadata = |left, right, row_path, right_binary| NativeTimeAddMetadata {
            left,
            right,
            row_path,
            right_binary,
        };
        let value = |left, right, meta, sign, expected: &str| {
            let bytes = evaluate_native_time_add(Some(left), Some(right), meta, sign).unwrap();
            assert_eq!(
                decode_native_time_add_result(&bytes),
                Some(NativeTimeAddResult::Value(expected))
            );
        };
        let warning = |left, right, meta, expected| {
            let bytes = evaluate_native_time_add(Some(left), Some(right), meta, 1).unwrap();
            assert_eq!(
                decode_native_time_add_result(&bytes),
                Some(NativeTimeAddResult::Warning(expected))
            );
        };
        for raw in 0..64 {
            let meta = NativeTimeAddMetadata::decode(raw).unwrap();
            assert_eq!(meta.encode(), raw);
            assert_eq!(
                native_time_add_args_valid(None, Some(b""), Some(raw)),
                meta.right != Datetime
            );
            assert_eq!(
                native_time_add_null_metadata_valid(Some(raw)),
                meta.right == Datetime
            );
            assert_eq!(evaluate_native_time_add(None, None, meta, 1), None);
        }
        for raw in [-1, 64, i64::MAX] {
            assert!(NativeTimeAddMetadata::decode(raw).is_none());
            assert!(!native_time_add_null_metadata_valid(Some(raw)));
            assert!(!native_time_add_args_valid(None, None, Some(raw)));
        }
        assert!(!native_time_add_args_valid(None, None, None));
        assert!(!native_time_add_null_metadata_valid(None));
        let ordinary = metadata(Other, Other, true, false);
        assert!(!native_time_add_args_valid(
            Some(&[255]),
            None,
            Some(ordinary.encode())
        ));
        assert!(!native_time_add_args_valid(
            None,
            Some(&[255]),
            Some(ordinary.encode())
        ));
        for left_kind in [Datetime, Date, Duration, Other] {
            let (left, expected) = match left_kind {
                Datetime => ("2020-01-01 10:00:00", "2020-01-01 10:00:01"),
                Date => ("2020-01-01", "2020-01-01 00:00:01"),
                _ => ("10:00:00", "10:00:01"),
            };
            for right_kind in [Datetime, Date, Duration, Other] {
                for row in [false, true] {
                    let meta = metadata(left_kind, right_kind, row, false);
                    if right_kind == Datetime {
                        assert_eq!(
                            evaluate_native_time_add(Some("bad"), Some("bad"), meta, 1),
                            None
                        );
                    } else {
                        // The worker consumes actual coerced text without
                        // inventing extra restrictions from the type category.
                        value(left, "00:00:01", meta, 1, expected);
                    }
                }
            }
        }
        value(
            "2020-01-01 10:00:00.123",
            "00:00:00.456789",
            metadata(Datetime, Duration, true, false),
            1,
            "2020-01-01 10:00:00.579789",
        );
        value(
            "2020-01-01 10:00:00.123",
            "00:00:00.456789",
            metadata(Datetime, Duration, false, false),
            1,
            "2020-01-01 10:00:00.579",
        );
        value(
            "2020-01-01",
            "00:00:00.1",
            metadata(Date, Other, true, false),
            1,
            "2020-01-01 00:00:00.100000",
        );
        let date = "2020-01-01 10:00:00";
        assert_eq!(
            evaluate_native_time_add(Some(date), Some(date), ordinary, 1),
            None
        );
        value(
            date,
            date,
            metadata(Other, Other, false, false),
            1,
            "2020-01-01 20:00:00",
        );
        value(date, date, ordinary, -1, "2020-01-01 00:00:00");
        warning(
            "bad",
            "bad",
            metadata(Duration, Duration, true, false),
            NativeTimeAddWarning::TruncatedLeft,
        );
        warning(
            "bad",
            "bad",
            metadata(Datetime, Duration, true, false),
            NativeTimeAddWarning::TruncatedRight,
        );
        warning(
            "bad",
            "bad",
            metadata(Date, Duration, true, false),
            NativeTimeAddWarning::TruncatedRight,
        );
        warning("bad", "bad", ordinary, NativeTimeAddWarning::TruncatedRight);
        assert_eq!(
            evaluate_native_time_add(
                Some("bad"),
                Some("bad"),
                metadata(Other, Other, true, true),
                1
            ),
            None
        );
        // Binary suppression belongs only to the Other/Other-or-Date failure,
        // not to a right operand evaluated under the Duration signature.
        warning(
            "bad",
            "bad",
            metadata(Other, Duration, true, true),
            NativeTimeAddWarning::TruncatedRight,
        );
        warning(
            "bad",
            "00:00:01",
            ordinary,
            NativeTimeAddWarning::IncorrectDateTimeLeft,
        );
        warning(
            "18446744073709551616",
            "00:00:01",
            ordinary,
            NativeTimeAddWarning::IncorrectTimeLeft,
        );
        assert_eq!(
            evaluate_native_time_add(
                Some("0000-00-00 00:00:00"),
                Some("00:00:01"),
                metadata(Datetime, Duration, true, false),
                1
            ),
            None
        );
        for bytes in [
            vec![],
            vec![0],
            vec![0, 255],
            vec![5],
            vec![1, 0],
            vec![4, b'x'],
        ] {
            assert!(!native_time_add_result_valid(&bytes));
        }
        for tag in 1..=4 {
            assert!(native_time_add_result_valid(&[tag]));
        }
        assert!(native_time_add_result_valid(&[0, b'x']));
    }
}
