// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native TIME casts over actual datum storage. Datatype conversion is closed
//! in the SDK; only the original, lazily demanded session-zone read is supplied
//! by the caller. The caller applies a returned truncation through its generic
//! handle_truncate effect before wrapping the selected value in native storage.
use chrono::TimeZone;
use tidb_query_datatype::codec::{
    convert::native_warning_subject_byte_cap,
    native_duration_convert::{
        NativeDurationParts, NativeDurationTargetError, native_convert_to_duration_target,
        native_number_to_duration,
    },
    native_eval_type::{NativeEvalType, native_field_eval_type},
    native_sql_string::{NativeSqlStringInput, native_sql_string},
    native_type_name::NativeTypeNameCode,
};

#[derive(Clone, Copy, Debug)]
pub struct NativeDurationCastSource {
    pub code: NativeTypeNameCode,
    pub flags: u64,
    pub decimal: i64,
}
#[derive(Clone, Debug)]
pub struct NativeDurationCastOutcome {
    pub value: Option<NativeDurationParts>,
    pub truncation: Option<String>,
}
use NativeSqlStringInput as I;

fn exact(value: Option<NativeDurationParts>) -> NativeDurationCastOutcome {
    NativeDurationCastOutcome {
        value,
        truncation: None,
    }
}
fn truncated(value: Option<NativeDurationParts>, input: &str) -> NativeDurationCastOutcome {
    NativeDurationCastOutcome {
        value,
        truncation: Some(format!(
            "Truncated incorrect time value: '{}'",
            native_warning_subject_byte_cap(input)
        )),
    }
}
/// The explicit cast's private conversion controller. It intentionally lacks an
/// early NULL guard: generic conversion consumes the zone before preserving a
/// NULL, while an explicitly integer-typed mismatched NULL is unsupported.
pub fn native_cast_duration<TZ: TimeZone>(
    input: I<'_>,
    source: Option<NativeDurationCastSource>,
    fsp: i64,
    zone: impl FnOnce() -> TZ,
) -> Result<NativeDurationCastOutcome, &'static str> {
    // This precedes both JSON tag rejection and source selection. In particular,
    // nonfinite binary JSON still reaches its original Display panic here.
    let text = native_sql_string(input).unwrap_or_else(|_| "<binary>".to_owned());
    let source_eval = source.map(|source| native_field_eval_type(source.code, source.flags));
    if let I::Json { type_code, .. } = input {
        if !matches!(type_code, 0x0e | 0x0f | 0x10 | 0x11 | 0x0c) {
            return Ok(truncated(None, &text));
        }
    }
    let numeric = matches!(
        source_eval,
        Some(NativeEvalType::Int | NativeEvalType::Real | NativeEvalType::Decimal)
    ) || (source_eval.is_none()
        && matches!(
            input,
            I::Int(_) | I::UInt(_) | I::Real(_) | I::Float32(_) | I::Decimal(_)
        ));
    let converted = if source_eval == Some(NativeEvalType::Int)
        || (source_eval.is_none() && matches!(input, I::Int(_) | I::UInt(_)))
    {
        let number = match input {
            I::Int(value) => value,
            I::UInt(value) => value as i64,
            _ => return Err("CAST AS TIME integer datum"),
        };
        native_number_to_duration(number, fsp).ok()
    } else {
        let zone = zone();
        // The original convert_to_in receiver is NULL-guarded, but the zone
        // argument was evaluated before that guard. The private datatype
        // duration selector itself does NOT have this outer NULL behavior.
        if matches!(input, I::Null) {
            return Ok(exact(None));
        }
        match native_convert_to_duration_target(input, fsp, &zone) {
            Ok(converted) => Some(converted),
            Err(NativeDurationTargetError::Unsupported) => return Err("CAST AS TIME source datum"),
            Err(_) => None,
        }
    };
    match converted {
        Some(converted) if converted.event.is_none() => Ok(exact(Some(converted.value))),
        // These closed duration paths produce only Truncated/Overflow events,
        // not RoundedToScale. Every produced event requires truncation handling.
        Some(converted) => Ok(truncated(
            if numeric { None } else { Some(converted.value) },
            &text,
        )),
        None => Ok(truncated(None, &text)),
    }
}
/// Argument coercion preserves existing raw duration metadata and NULL without
/// display, normalization or zone demand. Only named calendar source types
/// retain their declared precision; numeric lookalike unknown codes do not.
pub fn native_cast_arg_as_duration<TZ: TimeZone>(
    input: I<'_>,
    source: Option<NativeDurationCastSource>,
    zone: impl FnOnce() -> TZ,
) -> Result<NativeDurationCastOutcome, &'static str> {
    match input {
        I::Null => return Ok(exact(None)),
        I::Duration { nanoseconds, fsp } => {
            return Ok(exact(Some(NativeDurationParts { nanoseconds, fsp })));
        }
        _ => {}
    }
    let fsp = source
        .filter(|source| matches!(source.code, NativeTypeNameCode::Known(10 | 12 | 7)))
        .map_or(6, |source| source.decimal);
    native_cast_duration(input, source, fsp, zone)
}
/// Computed duration precision counts BYTES after the last dot in SQL Display,
/// including non-digits or a JSON closing quote. The ordinary controller then
/// renders again, exactly as the original pair of native calls did.
pub fn native_parse_computed_duration<TZ: TimeZone>(
    input: I<'_>,
    zone: impl FnOnce() -> TZ,
) -> Result<NativeDurationCastOutcome, &'static str> {
    let fsp = native_sql_string(input).ok().map_or(0, |text| {
        text.rsplit_once('.')
            .map_or(0, |(_, fraction)| fraction.len().min(6) as i64)
    });
    native_cast_duration(input, None, fsp, zone)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    fn check(outcome: NativeDurationCastOutcome, value: Option<(i64, i64)>, message: Option<&str>) {
        assert_eq!(
            outcome.value.map(|value| (value.nanoseconds, value.fsp)),
            value
        );
        assert_eq!(outcome.truncation.as_deref(), message);
    }
    #[test]
    fn duration_control_keeps_lazy_zone_json_admission_and_numeric_event_policy() {
        let reads = Cell::new(0);
        let zone = || {
            reads.set(reads.get() + 1);
            chrono::Utc
        };
        check(
            native_cast_duration(I::Int(123456), None, 0, zone).unwrap(),
            Some((45_296_000_000_000, 0)),
            None,
        );
        check(
            native_cast_duration(I::Int(20200101123456), None, 0, zone).unwrap(),
            Some((45_296_000_000_000, 0)),
            None,
        ); // NumberToDuration's fixed-UTC datetime fallback still does not demand the session zone.
        check(
            native_cast_duration(I::UInt(u64::MAX), None, 3, zone).unwrap(),
            Some((-1_000_000_000, 3)),
            None,
        );
        check(
            native_cast_duration(I::Int(i64::MIN), None, 6, zone).unwrap(),
            None,
            Some("Truncated incorrect time value: '-9223372036854775808'"),
        );
        check(
            native_cast_duration(I::Int(126060), None, 0, zone).unwrap(),
            None,
            Some("Truncated incorrect time value: '126060'"),
        );
        check(
            native_cast_duration(
                I::Json {
                    type_code: 9,
                    value: &[123, 0, 0, 0, 0, 0, 0, 0],
                },
                None,
                0,
                zone,
            )
            .unwrap(),
            None,
            Some("Truncated incorrect time value: '123'"),
        );
        assert_eq!(reads.get(), 0);
        check(
            native_cast_duration(I::Null, None, 0, zone).unwrap(),
            None,
            None,
        );
        assert_eq!(reads.get(), 1);
        let integer = NativeDurationCastSource {
            code: NativeTypeNameCode::Known(3),
            flags: 0,
            decimal: 0,
        };
        assert_eq!(
            native_cast_duration(I::Null, Some(integer), 0, zone).unwrap_err(),
            "CAST AS TIME integer datum"
        );
        assert_eq!(reads.get(), 1);
        assert_eq!(
            native_cast_duration(I::Enum(b"1"), None, 0, zone).unwrap_err(),
            "CAST AS TIME source datum"
        );
        assert_eq!(reads.get(), 2); // Unsupported fallback still demanded the zone.
        check(
            native_cast_duration(I::Bytes(&[0xff]), None, 0, zone).unwrap(),
            None,
            Some("Truncated incorrect time value: '<binary>'"),
        );
        check(
            native_cast_duration(I::String(b"900:00:00"), None, 0, zone).unwrap(),
            Some((3_020_399_000_000_000, 0)),
            Some("Truncated incorrect time value: '900:00:00'"),
        );
        let real = NativeDurationCastSource {
            code: NativeTypeNameCode::Known(5),
            flags: 0,
            decimal: 0,
        };
        check(
            native_cast_duration(I::String(b"900:00:00"), Some(real), 0, zone).unwrap(),
            None,
            Some("Truncated incorrect time value: '900:00:00'"),
        );
        assert_eq!(reads.get(), 5);
        let long = "é".repeat(65);
        let expected = format!("Truncated incorrect time value: '{}'", "é".repeat(64));
        check(
            native_cast_duration(I::String(long.as_bytes()), None, 0, zone).unwrap(),
            None,
            Some(&expected),
        );
        let inf = [0, 0, 0, 0, 0, 0, 240, 127];
        assert!(
            std::panic::catch_unwind(|| native_cast_duration(
                I::Json {
                    type_code: 11,
                    value: &inf
                },
                None,
                0,
                || chrono::Utc
            ))
            .is_err()
        );
    }
    #[test]
    fn duration_argument_and_computed_precision_keep_raw_pass_and_display_byte_policy() {
        let reads = Cell::new(0);
        let zone = || {
            reads.set(reads.get() + 1);
            chrono::Utc
        };
        check(
            native_cast_arg_as_duration(
                I::Duration {
                    nanoseconds: i64::MAX,
                    fsp: -123,
                },
                None,
                zone,
            )
            .unwrap(),
            Some((i64::MAX, -123)),
            None,
        );
        check(
            native_cast_arg_as_duration(I::Null, None, zone).unwrap(),
            None,
            None,
        );
        assert_eq!(reads.get(), 0);
        let date = NativeDurationCastSource {
            code: NativeTypeNameCode::Known(10),
            flags: 0,
            decimal: 2,
        };
        let unknown_date = NativeDurationCastSource {
            code: NativeTypeNameCode::Unknown(10),
            flags: 0,
            decimal: 2,
        };
        check(
            native_cast_arg_as_duration(I::String(b"12:34:56.12"), Some(date), zone).unwrap(),
            Some((45_296_120_000_000, 2)),
            None,
        );
        check(
            native_cast_arg_as_duration(I::String(b"12:34:56.12"), Some(unknown_date), zone)
                .unwrap(),
            Some((45_296_120_000_000, 6)),
            None,
        );
        check(
            native_parse_computed_duration(I::String(b"12:34:56.12"), zone).unwrap(),
            Some((45_296_120_000_000, 2)),
            None,
        );
        // SQL JSON Display contains the quotes, so its suffix is `12"` (three
        // bytes), while duration conversion itself uses the unquoted text.
        check(
            native_parse_computed_duration(
                I::Json {
                    type_code: 12,
                    value: b"\x0b12:34:56.12",
                },
                zone,
            )
            .unwrap(),
            Some((45_296_120_000_000, 3)),
            None,
        );
        check(
            native_parse_computed_duration(I::Null, zone).unwrap(),
            None,
            None,
        );
        assert_eq!(reads.get(), 5);
    }
}
