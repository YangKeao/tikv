// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native datatype duration conversion, without expression warning policy.

use std::{fmt, str::Utf8Error};

use chrono::TimeZone;

use super::{
    mysql::{
        Time,
        duration::{
            NativeDurationParseError, NativeDurationParseEvent, NativeDurationValueError,
            native_duration_from_time, native_parse_mysql_duration,
        },
        json::{NativeJsonError, native_unquote_binary_json},
        time::{
            NativeFspError, NativeTemporalValue, NativeTimeError, TimeType, native_parse_time,
            native_parse_time_from_num,
        },
    },
    native_sql_string::{NativeSqlStringError, NativeSqlStringInput, native_sql_string},
};

/// Native TIME's whole-second boundary, shared by conversion and datum bounds.
pub const NATIVE_MAX_DURATION_NANOS: i64 = 3_020_399_000_000_000;

/// Raw native duration storage, not the range-checked wire Duration.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct NativeDurationParts {
    pub nanoseconds: i64,
    pub fsp: i64,
}

impl NativeDurationParts {
    /// Constructs raw native duration storage without FSP or range validation.
    pub const fn from_raw_parts(nanoseconds: i64, fsp: i64) -> Self {
        Self { nanoseconds, fsp }
    }

    pub const fn nanoseconds(self) -> i64 {
        self.nanoseconds
    }

    pub const fn fsp(self) -> i64 {
        self.fsp
    }
}

/// Original nonfatal conversion event. Overflow retains its original subject.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeDurationConvertEvent {
    Truncated,
    Overflow(String),
}

/// Value and diagnostic event remain independent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeDurationConverted<T> {
    pub value: T,
    pub event: Option<NativeDurationConvertEvent>,
}

/// StrToDuration deliberately preserves its successful datetime alternative.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDurationOrTime {
    Duration(NativeDurationParts),
    Time(NativeTemporalValue),
}

/// Source-compatible duration rounding failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeDurationRoundError {
    InvalidFsp(NativeFspError),
    Overflow,
}

impl fmt::Display for NativeDurationRoundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFsp(error) => error.fmt(formatter),
            Self::Overflow => formatter.write_str("rounded duration is out of range"),
        }
    }
}

impl std::error::Error for NativeDurationRoundError {}

/// Preserve native datatype error categories before expression policy applies.
#[derive(Debug)]
pub enum NativeDurationTargetError {
    InvalidUtf8(Utf8Error),
    Unsupported,
    Time(NativeTimeError),
    Round(NativeDurationRoundError),
    Duration(NativeDurationValueError),
    JsonInvalidBinary,
    JsonInvalidText,
    SqlString(NativeSqlStringError),
}

fn normalize_fsp(fsp: i64) -> Result<i64, NativeFspError> {
    Time::native_normalize_fsp(fsp).ok_or(NativeFspError::InvalidFsp(fsp))
}

fn exact<T>(value: T) -> NativeDurationConverted<T> {
    NativeDurationConverted { value, event: None }
}

/// Round using the native positive-infinity tie rule, without SQL range
/// clamping. Normalize the target before the equal-FSP raw-value early return.
pub fn native_round_duration_fsp(
    nanoseconds: i64,
    current_fsp: i64,
    target_fsp: i64,
) -> Result<NativeDurationParts, NativeDurationRoundError> {
    let fsp = normalize_fsp(target_fsp).map_err(NativeDurationRoundError::InvalidFsp)?;
    if current_fsp == fsp {
        return Ok(NativeDurationParts { nanoseconds, fsp });
    }
    let unit = 10_i128.pow((9 - fsp) as u32);
    let half = unit / 2;
    let value = i128::from(nanoseconds);
    let rounded_units = if value >= 0 {
        (value + half) / unit
    } else {
        let magnitude = -value;
        -((magnitude + half - 1) / unit)
    };
    let rounded = rounded_units * unit;
    let nanoseconds = i64::try_from(rounded).map_err(|_| NativeDurationRoundError::Overflow)?;
    Ok(NativeDurationParts { nanoseconds, fsp })
}

/// NumberToDuration keeps its integer bounds, UTC calendar fallback and event.
pub fn native_number_to_duration(
    mut number: i64,
    fsp: i64,
) -> Result<NativeDurationConverted<NativeDurationParts>, NativeTimeError> {
    const TIME_MAX_VALUE: i64 = 8_385_959;
    // Bound first: i64::MIN must never reach abs().
    if !(-TIME_MAX_VALUE..=TIME_MAX_VALUE).contains(&number) {
        if number >= 10_000_000_000 {
            if let Ok(parsed) = native_parse_time_from_num(
                number,
                TimeType::DateTime,
                fsp,
                false,
                false,
                true,
                &chrono_tz::UTC,
            )
            .into_result()
            {
                let (nanoseconds, fsp) =
                    native_duration_from_time(parsed.time.raw, i64::from(parsed.time.fsp))?;
                return Ok(exact(NativeDurationParts { nanoseconds, fsp }));
            }
        }
        let fsp = normalize_fsp(fsp).map_err(NativeTimeError::InvalidFsp)?;
        return Ok(NativeDurationConverted {
            value: NativeDurationParts {
                nanoseconds: if number < 0 {
                    -NATIVE_MAX_DURATION_NANOS
                } else {
                    NATIVE_MAX_DURATION_NANOS
                },
                fsp,
            },
            event: Some(NativeDurationConvertEvent::Overflow(number.to_string())),
        });
    }
    let negative = number < 0;
    number = number.abs();
    let hour = number / 10_000;
    let minute = (number / 100) % 100;
    let second = number % 100;
    if hour > 838 || minute >= 60 || second >= 60 {
        // The original zero-duration error-side value bypasses target FSP.
        return Ok(NativeDurationConverted {
            value: NativeDurationParts {
                nanoseconds: 0,
                fsp: 0,
            },
            event: Some(NativeDurationConvertEvent::Truncated),
        });
    }
    let sign = if negative { -1 } else { 1 };
    let nanoseconds = sign * (hour * 3_600 + minute * 60 + second) * 1_000_000_000;
    let fsp = normalize_fsp(fsp).map_err(NativeTimeError::InvalidFsp)?;
    Ok(exact(NativeDurationParts { nanoseconds, fsp }))
}

/// StrToDateTime exposes truncation only, retaining the original DST-bit
/// policy.
pub fn native_str_to_datetime<TZ: TimeZone>(
    input: &str,
    fsp: i64,
    timezone: &TZ,
) -> Result<NativeDurationConverted<NativeTemporalValue>, NativeTimeError> {
    native_parse_time(
        input,
        TimeType::DateTime,
        fsp,
        false,
        true,
        false,
        true,
        timezone,
    )
    .map(|parsed| NativeDurationConverted {
        value: parsed.time,
        event: parsed
            .truncated
            .then_some(NativeDurationConvertEvent::Truncated),
    })
}

/// StrToDuration tries sufficiently long datetime text before the duration
/// parser.
pub fn native_str_to_duration<TZ: TimeZone>(
    input: &str,
    fsp: i64,
    timezone: &TZ,
) -> Result<NativeDurationConverted<NativeDurationOrTime>, NativeDurationValueError> {
    let input = input.trim();
    let unsigned = input.strip_prefix('-').unwrap_or(input);
    let integer_length = unsigned.find('.').unwrap_or(unsigned.len());
    if integer_length >= 12 {
        if let Ok(parsed) = native_str_to_datetime(input, fsp, timezone) {
            return Ok(NativeDurationConverted {
                value: NativeDurationOrTime::Time(parsed.value),
                event: parsed.event,
            });
        }
    }
    let parsed = native_parse_mysql_duration(input, fsp, timezone, true, false)?;
    let normalized_fsp = normalize_fsp(parsed.fsp())
        .map_err(NativeDurationParseError::InvalidFsp)
        .map_err(NativeDurationValueError::Duration)?;
    Ok(NativeDurationConverted {
        value: NativeDurationOrTime::Duration(NativeDurationParts {
            nanoseconds: parsed.nanoseconds(),
            fsp: normalized_fsp,
        }),
        event: parsed.event().and_then(|event| match event {
            NativeDurationParseEvent::Truncated => Some(NativeDurationConvertEvent::Truncated),
            NativeDurationParseEvent::Overflow(_) => {
                Some(NativeDurationConvertEvent::Overflow(input.to_owned()))
            }
            NativeDurationParseEvent::DateTimeFallback(_) => None,
        }),
    })
}

/// Project StrToDuration's two successful alternatives into duration storage.
pub fn native_duration_from_text<TZ: TimeZone>(
    text: &str,
    fsp: i64,
    timezone: &TZ,
) -> Result<NativeDurationConverted<NativeDurationParts>, NativeDurationTargetError> {
    let converted =
        native_str_to_duration(text, fsp, timezone).map_err(NativeDurationTargetError::Duration)?;
    let value = match converted.value {
        NativeDurationOrTime::Duration(value) => value,
        NativeDurationOrTime::Time(value) => {
            let (nanoseconds, fsp) = native_duration_from_time(value.raw, i64::from(value.fsp))
                .map_err(NativeDurationTargetError::Time)?;
            NativeDurationParts { nanoseconds, fsp }
        }
    };
    Ok(NativeDurationConverted {
        value,
        event: converted.event,
    })
}

fn unquote_json(type_code: u8, value: &[u8]) -> Result<String, NativeDurationTargetError> {
    native_unquote_binary_json(type_code, value).map_err(|error| match error {
        NativeJsonError::InvalidBinary => NativeDurationTargetError::JsonInvalidBinary,
        NativeJsonError::EmptyText | NativeJsonError::InvalidText => {
            NativeDurationTargetError::JsonInvalidText
        }
    })
}

/// Native ConvertTo duration target. Its caller retains the SQL-NULL guard.
/// No source metadata, statement clock, expression warnings or wire clamps
/// enter.
pub fn native_convert_to_duration_target<TZ: TimeZone>(
    input: NativeSqlStringInput<'_>,
    target_decimal: i64,
    timezone: &TZ,
) -> Result<NativeDurationConverted<NativeDurationParts>, NativeDurationTargetError> {
    use NativeSqlStringInput as I;
    let fsp = if target_decimal == -1 {
        0
    } else {
        target_decimal
    };
    match input {
        I::Time(value) => {
            let (nanoseconds, current_fsp) =
                native_duration_from_time(value.raw, i64::from(value.fsp))
                    .map_err(NativeDurationTargetError::Time)?;
            native_round_duration_fsp(nanoseconds, current_fsp, fsp)
                .map(exact)
                .map_err(NativeDurationTargetError::Round)
        }
        I::Duration {
            nanoseconds,
            fsp: current_fsp,
        } => native_round_duration_fsp(nanoseconds, current_fsp, fsp)
            .map(exact)
            .map_err(NativeDurationTargetError::Round),
        I::String(bytes) | I::Bytes(bytes) => {
            let text =
                std::str::from_utf8(bytes).map_err(NativeDurationTargetError::InvalidUtf8)?;
            native_duration_from_text(text, fsp, timezone)
        }
        numeric @ (I::Int(_) | I::UInt(_) | I::Real(_) | I::Float32(_) | I::Decimal(_)) => {
            let text = native_sql_string(numeric).map_err(NativeDurationTargetError::SqlString)?;
            native_duration_from_text(&text, fsp, timezone)
        }
        I::Json { type_code, value } => {
            native_duration_from_text(&unquote_json(type_code, value)?, fsp, timezone)
        }
        _ => Err(NativeDurationTargetError::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_duration_parts_keep_unvalidated_metadata() {
        let parts = NativeDurationParts::from_raw_parts(-1, 7);
        assert_eq!(parts.nanoseconds(), -1);
        assert_eq!(parts.fsp(), 7);
    }

    #[test]
    fn numeric_and_round_duration_keep_fixed_values_event_subjects_and_raw_domain() {
        for (nanos, current, target, expected, fsp) in [
            (1_500_000_000, 9, 0, 2_000_000_000, 0),
            (-1_500_000_000, 9, 0, -1_000_000_000, 0),
            (-1_500_000_001, 9, 0, -2_000_000_000, 0),
            (1, 6, 6, 1, 6),
            (1_500, 9, 7, 2_000, 6),
            (i64::MAX, 0, -1, i64::MAX, 0),
        ] {
            assert_eq!(
                native_round_duration_fsp(nanos, current, target).unwrap(),
                NativeDurationParts {
                    nanoseconds: expected,
                    fsp
                }
            );
        }
        assert_eq!(
            native_round_duration_fsp(0, -2, -2),
            Err(NativeDurationRoundError::InvalidFsp(
                NativeFspError::InvalidFsp(-2)
            ))
        );
        for value in [i64::MIN, i64::MAX] {
            assert_eq!(
                native_round_duration_fsp(value, 9, 0),
                Err(NativeDurationRoundError::Overflow)
            );
        }
        assert_eq!(
            NativeDurationRoundError::Overflow.to_string(),
            "rounded duration is out of range"
        );
        assert_eq!(
            native_number_to_duration(126060, -2).unwrap(),
            NativeDurationConverted {
                value: NativeDurationParts {
                    nanoseconds: 0,
                    fsp: 0
                },
                event: Some(NativeDurationConvertEvent::Truncated),
            }
        );
        assert_eq!(
            native_number_to_duration(-123456, 2).unwrap(),
            exact(NativeDurationParts {
                nanoseconds: -45_296_000_000_000,
                fsp: 2
            })
        );
        assert_eq!(
            native_number_to_duration(20200102123456, 3).unwrap(),
            exact(NativeDurationParts {
                nanoseconds: 45_296_000_000_000,
                fsp: 3
            })
        );
        for (number, nanos, subject) in [
            (i64::MIN, -3_020_399_000_000_000, "-9223372036854775808"),
            (8_385_960, 3_020_399_000_000_000, "8385960"),
        ] {
            assert_eq!(
                native_number_to_duration(number, 7).unwrap(),
                NativeDurationConverted {
                    value: NativeDurationParts {
                        nanoseconds: nanos,
                        fsp: 6
                    },
                    event: Some(NativeDurationConvertEvent::Overflow(subject.to_owned())),
                }
            );
        }
        assert_eq!(
            native_number_to_duration(0, -2),
            Err(NativeTimeError::InvalidFsp(NativeFspError::InvalidFsp(-2)))
        );
    }

    #[test]
    fn string_and_target_duration_keep_alternatives_unquote_errors_and_no_round_event() {
        use NativeDurationTargetError as E;
        use NativeSqlStringInput as I;
        let zone = chrono_tz::UTC;
        let parsed = native_str_to_duration(" 20200102123456 ", 3, &zone).unwrap();
        assert_eq!(parsed.event, None);
        let NativeDurationOrTime::Time(value) = parsed.value else {
            panic!("datetime alternative lost")
        };
        assert_eq!(
            Time::native_core_fields(value.raw),
            [2020, 1, 2, 12, 34, 56, 0]
        );
        assert_eq!(value.kind, TimeType::DateTime);
        assert_eq!(value.fsp, 3);
        assert_eq!(
            native_duration_from_text(" 20200102123456 ", 3, &zone).unwrap(),
            exact(NativeDurationParts {
                nanoseconds: 45_296_000_000_000,
                fsp: 3
            })
        );
        assert_eq!(
            native_str_to_duration(" 900:00:00 ", 0, &zone).unwrap(),
            NativeDurationConverted {
                value: NativeDurationOrTime::Duration(NativeDurationParts {
                    nanoseconds: 3_020_399_000_000_000,
                    fsp: 0
                }),
                event: Some(NativeDurationConvertEvent::Overflow("900:00:00".to_owned())),
            }
        );
        let truncated = native_str_to_datetime("1701020304.111", 0, &zone).unwrap();
        assert_eq!(
            Time::native_core_fields(truncated.value.raw),
            [2017, 1, 2, 3, 4, 11, 0]
        );
        assert_eq!(truncated.event, Some(NativeDurationConvertEvent::Truncated));
        for input in [
            I::String(b"12:34:56"),
            I::Bytes(b"12:34:56"),
            I::Int(123456),
        ] {
            assert_eq!(
                native_convert_to_duration_target(input, -1, &zone).unwrap(),
                exact(NativeDurationParts {
                    nanoseconds: 45_296_000_000_000,
                    fsp: 0
                })
            );
        }
        assert_eq!(
            native_convert_to_duration_target(
                I::Duration {
                    nanoseconds: -1_500_000_000,
                    fsp: 9
                },
                0,
                &zone
            )
            .unwrap(),
            exact(NativeDurationParts {
                nanoseconds: -1_000_000_000,
                fsp: 0
            })
        );
        assert_eq!(
            native_convert_to_duration_target(
                I::Time(NativeTemporalValue {
                    raw: 0,
                    kind: TimeType::DateTime,
                    fsp: 255
                }),
                6,
                &zone
            )
            .unwrap(),
            exact(NativeDurationParts {
                nanoseconds: 0,
                fsp: 6
            })
        );
        let twice_quoted = [
            10, b'"', b'1', b'2', b':', b'3', b'4', b':', b'5', b'6', b'"',
        ];
        assert_eq!(
            native_convert_to_duration_target(
                I::Json {
                    type_code: 0x0c,
                    value: &twice_quoted
                },
                0,
                &zone
            )
            .unwrap(),
            exact(NativeDurationParts {
                nanoseconds: 45_296_000_000_000,
                fsp: 0
            })
        );
        assert!(matches!(
            native_convert_to_duration_target(
                I::Json {
                    type_code: 0x0c,
                    value: &[1, 0xff]
                },
                0,
                &zone
            ),
            Err(E::JsonInvalidBinary)
        ));
        let bad_escape = [5, b'"', b'\\', b'u', b'x', b'"'];
        assert!(matches!(
            native_convert_to_duration_target(
                I::Json {
                    type_code: 0x0c,
                    value: &bad_escape
                },
                0,
                &zone
            ),
            Err(E::JsonInvalidText)
        ));
        for input in [I::String(&[0xff]), I::Bytes(&[0xff])] {
            assert!(matches!(
                native_convert_to_duration_target(input, 0, &zone),
                Err(E::InvalidUtf8(_))
            ));
        }
        for input in [
            I::Null,
            I::Enum(b"12:34:56"),
            I::Set(b"12:34:56"),
            I::Bit(b"123456"),
            I::BinaryLiteral(b"123456"),
            I::Raw(b"123456"),
        ] {
            assert!(matches!(
                native_convert_to_duration_target(input, 0, &zone),
                Err(E::Unsupported)
            ));
        }
        assert!(matches!(
            native_convert_to_duration_target(
                I::Duration {
                    nanoseconds: 0,
                    fsp: 0
                },
                -2,
                &zone
            ),
            Err(E::Round(NativeDurationRoundError::InvalidFsp(_)))
        ));
        assert!(matches!(
            native_duration_from_text("not a time", 0, &zone),
            Err(E::Duration(_))
        ));
    }
}
