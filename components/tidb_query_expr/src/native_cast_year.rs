// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native YEAR CAST control. Both borrowed views describe the same actual
//! datum: names are for text coercion, while Enum/Set ordinals remain available
//! to signed fallback. Only original context data reads are supplied by
//! callers.
use chrono::{DateTime, TimeZone, Utc};
use tidb_query_datatype::codec::{
    mysql::Time, native_duration_convert::NativeDurationParts, native_numeric::NativeNumericInput,
    native_sql_string::NativeSqlStringInput,
    native_temporal_convert::native_duration_convert_to_year_with_event,
};

use crate::{
    native_cast_integer::native_cast_integer_signed_numeric,
    native_coerce_string::native_coerce_string,
};

/// Preserve the original clock -> timestamp validation -> zone -> concat-mode
/// demand order for Duration. Other sources consult neither clock nor session
/// zone, and use the original fixed-UTC signed fallback after text-date
/// parsing.
pub fn native_cast_year<TZ: TimeZone>(
    text: NativeSqlStringInput<'_>,
    number: NativeNumericInput<'_>,
    now: impl FnOnce() -> Option<(i64, u32, i32)>,
    zone: impl FnOnce() -> TZ,
    concat: impl FnOnce() -> bool,
) -> Result<i64, &'static str> {
    if let NativeSqlStringInput::Duration { nanoseconds, fsp } = text {
        let (utc_secs, nanos, _) = now().ok_or("no statement clock for a YEAR cast")?;
        let now = DateTime::<Utc>::from_timestamp(utc_secs, nanos)
            .ok_or("statement clock is out of range")?
            .with_timezone(&zone());
        return native_duration_convert_to_year_with_event(
            NativeDurationParts { nanoseconds, fsp },
            now,
            concat(),
        )
        .and_then(|value| value.into_result())
        .map_err(|_| "duration to YEAR conversion");
    }
    if let Some(text) = native_coerce_string(text)? {
        if let Some((year, ..)) = Time::parse_native_date_ymd(&text) {
            return Ok(year);
        }
    }
    Ok(native_cast_integer_signed_numeric(number, &Utc))
}

#[cfg(test)]
#[test]
fn year_cast_keeps_text_first_signed_fallback_and_duration_context_demand() {
    use std::cell::RefCell;

    use NativeNumericInput as N;
    use NativeSqlStringInput as I;
    fn plain(text: I<'_>, number: N<'_>) -> Result<i64, &'static str> {
        native_cast_year(
            text,
            number,
            || panic!("unexpected statement clock"),
            || -> Utc { panic!("unexpected session zone") },
            || panic!("unexpected concat flag"),
        )
    }
    assert_eq!(
        plain(I::String(b"2020-02-03"), N::String(b"2020-02-03")),
        Ok(2020)
    );
    assert_eq!(plain(I::UInt(u64::MAX), N::UInt(u64::MAX)), Ok(-1));
    assert_eq!(plain(I::Enum(b"not a date"), N::Enum(37)), Ok(37));
    assert_eq!(plain(I::Set(b"not a date"), N::Set(u64::MAX)), Ok(i64::MAX));
    assert_eq!(plain(I::Enum(b"2020-02-03"), N::Enum(37)), Ok(2020));
    assert_eq!(
        plain(I::Bytes(&[0xff]), N::Bytes(&[0xff])),
        Err("invalid UTF-8 byte datum")
    );
    assert_eq!(plain(I::Raw(b"not a date"), N::Raw(b"not a date")), Ok(0));
    // Keep the old signed value helper's unreachable guard rather than adding
    // a new YEAR NULL result outside the caller's original admission boundary.
    assert!(std::panic::catch_unwind(|| plain(I::Null, N::Null)).is_err());

    let calls = RefCell::new(Vec::new());
    let zero = NativeDurationParts {
        nanoseconds: 0,
        fsp: 0,
    };
    let text = I::Duration {
        nanoseconds: 0,
        fsp: 0,
    };
    let number = N::Duration(zero);
    let missing = native_cast_year(
        text,
        number,
        || {
            calls.borrow_mut().push("now");
            None
        },
        || {
            calls.borrow_mut().push("zone");
            Utc
        },
        || {
            calls.borrow_mut().push("concat");
            true
        },
    );
    assert_eq!(missing, Err("no statement clock for a YEAR cast"));
    assert_eq!(*calls.borrow(), vec!["now"]);
    calls.borrow_mut().clear();
    let invalid = native_cast_year(
        text,
        number,
        || {
            calls.borrow_mut().push("now");
            Some((i64::MAX, 0, 0))
        },
        || {
            calls.borrow_mut().push("zone");
            Utc
        },
        || {
            calls.borrow_mut().push("concat");
            false
        },
    );
    assert_eq!(invalid, Err("statement clock is out of range"));
    assert_eq!(*calls.borrow(), vec!["now"]);
    calls.borrow_mut().clear();
    let calendar = native_cast_year(
        text,
        number,
        || {
            calls.borrow_mut().push("now");
            Some((0, 0, 7200))
        },
        || {
            calls.borrow_mut().push("zone");
            chrono::FixedOffset::west_opt(3600).unwrap()
        },
        || {
            calls.borrow_mut().push("concat");
            false
        },
    );
    assert_eq!(calendar, Ok(1969)); // session zone wins, not the clock tuple's ignored offset
    assert_eq!(*calls.borrow(), vec!["now", "zone", "concat"]);
    calls.borrow_mut().clear();
    let duration = NativeDurationParts {
        nanoseconds: 1_212_000_000_000,
        fsp: 0,
    };
    let concatenated = native_cast_year(
        I::Duration {
            nanoseconds: duration.nanoseconds,
            fsp: 0,
        },
        N::Duration(duration),
        || {
            calls.borrow_mut().push("now");
            Some((0, 0, 0))
        },
        || {
            calls.borrow_mut().push("zone");
            Utc
        },
        || {
            calls.borrow_mut().push("concat");
            true
        },
    );
    assert_eq!(concatenated, Ok(2012));
    assert_eq!(*calls.borrow(), vec!["now", "zone", "concat"]); // still demanded even though the leaf concat branch ignores its clock
    let overflow = NativeDurationParts {
        nanoseconds: 3_600_000_000_000,
        fsp: 0,
    };
    assert_eq!(
        native_cast_year(
            I::Duration {
                nanoseconds: overflow.nanoseconds,
                fsp: 0
            },
            N::Duration(overflow),
            || Some((0, 0, 0)),
            || Utc,
            || true
        ),
        Err("duration to YEAR conversion")
    );
}
