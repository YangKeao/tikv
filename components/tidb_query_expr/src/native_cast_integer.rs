// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Ordinary integer CAST control. Datatype callbacks carry real conversion
//! results/events, while the existing RealUnsigned facade retains its original
//! execution and infrastructure errors. This module adds no facade entries.
use tidb_query_datatype::codec::{
    convert::native_warning_subject_byte_cap,
    mysql::{Decimal, JsonType, NativeDecimalParseRef, NativeDecimalParseValue},
};

pub use crate::NativeIntervalEvalType as NativeCastIntegerEvalType;

#[derive(Clone, Copy, Debug)]
pub enum NativeCastIntegerInput<'a> {
    Null,
    MinNotNull,
    MaxValue,
    Int(i64),
    UInt(u64),
    Decimal(NativeDecimalParseRef<'a>),
    Real(f64),
    Float32(f64),
    String(&'a [u8]),
    Bytes(&'a [u8]),
    Time,
    Duration,
    Json { type_code: u8 },
    Other,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCastIntegerTarget {
    Signed,
    Unsigned,
    UnsignedInUnion,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCastIntegerResult {
    Signed(i64),
    Unsigned(u64),
}

fn text(input: NativeCastIntegerInput<'_>) -> Option<&str> {
    match input {
        NativeCastIntegerInput::String(bytes) | NativeCastIntegerInput::Bytes(bytes) => {
            std::str::from_utf8(bytes).ok()
        }
        _ => None,
    }
}
fn decimal_text(value: NativeDecimalParseRef<'_>) -> String {
    Decimal::native_format_visible(
        value.negative,
        value.digits,
        value.scale,
        value.storage_scale,
    )
}
fn int_prefix_consumed_all(s: &str) -> bool {
    let trimmed = s.trim();
    let mut valid_len = 0;
    for (i, byte) in trimmed.bytes().enumerate() {
        if (byte == b'+' || byte == b'-') && i == 0 {
            continue;
        }
        if byte.is_ascii_digit() {
            valid_len = i + 1;
            continue;
        }
        break;
    }
    valid_len != 0 && valid_len == trimmed.len()
}
fn signed_string_integer_parse_overflows(text: &str) -> bool {
    if !int_prefix_consumed_all(text) {
        return false;
    }
    let trimmed = text.trim();
    if trimmed.starts_with('-') {
        trimmed.parse::<i64>().is_err()
    } else {
        trimmed
            .strip_prefix('+')
            .unwrap_or(trimmed)
            .parse::<u64>()
            .is_err()
    }
}
fn str_int_prefix(s: &str) -> i64 {
    let s = s.trim_start();
    let (negative, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return 0;
    }
    if negative {
        format!("-{digits}").parse::<i64>().unwrap_or(i64::MIN)
    } else {
        digits.parse::<u64>().map_or(-1, |value| value as i64)
    }
}

/// Warning-only public helper. Only actual JSON object/array tags demand the
/// original Display service. NULL and range sentinels are ignored here, unlike
/// the value helpers whose original guards remain mandatory.
pub fn native_cast_integer_input_warning<E>(
    input: NativeCastIntegerInput<'_>,
    json_text: impl FnOnce() -> String,
    mut handle_truncate: impl FnMut(&str) -> Result<(), E>,
) -> Result<(), E> {
    let json;
    let text = match input {
        NativeCastIntegerInput::Json { type_code }
            if type_code == JsonType::Object as u8 || type_code == JsonType::Array as u8 =>
        {
            json = json_text();
            Some(json.as_str())
        }
        _ => text(input),
    };
    match text {
        Some(text)
            if !int_prefix_consumed_all(text) || signed_string_integer_parse_overflows(text) =>
        {
            handle_truncate(&format!(
                "Truncated incorrect INTEGER value: '{}'",
                native_warning_subject_byte_cap(text.trim())
            ))
        }
        _ => Ok(()),
    }
}
fn signed_warning(input: NativeCastIntegerInput<'_>, mut append_warning: impl FnMut(u16, &str)) {
    match input {
        NativeCastIntegerInput::Real(value) | NativeCastIntegerInput::Float32(value) => {
            let rounded = value.round_ties_even();
            if !(i64::MIN as f64..=i64::MAX as f64).contains(&rounded) {
                append_warning(
                    1690,
                    &format!(
                        "constant {} overflows bigint",
                        Decimal::native_format_float_g_shortest(rounded)
                    ),
                );
            }
        }
        NativeCastIntegerInput::Decimal(value) if value.round_to_i64().is_none() => append_warning(
            1292,
            &format!(
                "Truncated incorrect DECIMAL value: '{}'",
                decimal_text(value)
            ),
        ),
        _ => {
            let Some(text) = text(input) else {
                return;
            };
            if !int_prefix_consumed_all(text) {
                return;
            }
            let trimmed = text.trim();
            if trimmed.starts_with('-') {
                return;
            }
            if trimmed
                .strip_prefix('+')
                .unwrap_or(trimmed)
                .parse::<u64>()
                .is_ok_and(|value| value > i64::MAX as u64)
            {
                append_warning(
                    8030,
                    "Cast to signed converted positive out-of-range integer to its negative complement",
                );
            }
        }
    }
}
fn negative_string_warning(
    input: NativeCastIntegerInput<'_>,
    mut append_warning: impl FnMut(u16, &str),
) {
    let Some(text) = text(input) else {
        return;
    };
    let trimmed = text.trim();
    if trimmed.len() <= 1
        || !trimmed.starts_with('-')
        || !int_prefix_consumed_all(trimmed)
        || trimmed.parse::<i64>().is_err()
    {
        return;
    }
    append_warning(
        8031,
        "Cast to unsigned converted negative integer to it's positive complement",
    );
}
fn union_negative(
    input: NativeCastIntegerInput<'_>,
    source: Option<NativeCastIntegerEvalType>,
) -> bool {
    use NativeCastIntegerEvalType as T;
    use NativeCastIntegerInput as I;
    match source {
        Some(T::Int) => matches!(input,I::Int(value) if value<0),
        Some(T::Real) => matches!(input,I::Real(value)|I::Float32(value) if value<0.0),
        Some(T::Decimal) => matches!(input,I::Decimal(value) if value.round_to_i64_saturating()<0),
        Some(T::String) | None => match input {
            I::String(bytes) | I::Bytes(bytes) => std::str::from_utf8(bytes)
                .is_ok_and(|text| text.trim().len() > 1 && text.trim().starts_with('-')),
            I::Int(value) => value < 0,
            I::Real(value) | I::Float32(value) => value < 0.0,
            I::Decimal(value) => value.round_to_i64_saturating() < 0,
            _ => false,
        },
        Some(T::Datetime | T::Timestamp | T::Duration | T::Json | T::VectorFloat32) => false,
    }
}

/// Original value-only signed helper, with an already-obtained actual zone.
/// Float32 stays on the default datatype path; it is not the Real branch.
pub fn native_cast_integer_signed_value<Z, E, W>(
    input: NativeCastIntegerInput<'_>,
    zone: &Z,
    to_signed: impl FnOnce(&Z) -> Result<(i64, W), E>,
) -> i64 {
    use NativeCastIntegerInput as I;
    match input {
        I::Int(value) => value,
        I::UInt(value) => value as i64,
        I::Decimal(value) => value.round_to_i64_saturating(),
        I::Real(value) => value.round_ties_even() as i64,
        I::String(bytes) | I::Bytes(bytes) => {
            std::str::from_utf8(bytes).map(str_int_prefix).unwrap_or(0)
        }
        I::Null | I::MinNotNull | I::MaxValue => unreachable!("guarded by caller"),
        _ => match to_signed(zone) {
            Ok((value, event)) => {
                drop(event);
                value
            }
            Err(_) => 0,
        },
    }
}
/// Original unsigned value-only helper: no input-truncation or 8031 advisory,
/// but decimal negative warnings and the original RealUnsigned worker remain.
/// Datatype errors fold to zero; infrastructure errors from that worker do not.
pub fn native_cast_integer_unsigned_value<E, EI, ED, WI, WD, Z>(
    input: NativeCastIntegerInput<'_>,
    time_zone: impl FnOnce() -> Z,
    to_signed: impl FnOnce(&Z) -> Result<(i64, WI), EI>,
    to_decimal: impl FnOnce() -> Result<(NativeDecimalParseValue, WD), ED>,
    real_unsigned: impl FnOnce() -> Result<u64, E>,
    mut append_warning: impl FnMut(u16, &str),
) -> Result<u64, E> {
    use NativeCastIntegerInput as I;
    Ok(match input {
        I::Int(_) | I::String(_) | I::Bytes(_) | I::Time | I::Duration => {
            native_cast_integer_signed_value(input, &time_zone(), to_signed) as u64
        }
        I::UInt(value) => value,
        I::Decimal(value) => {
            if value.round_to_i64_saturating() < 0 {
                append_warning(
                    1292,
                    &format!(
                        "Truncated incorrect DECIMAL value: '{}'",
                        decimal_text(value)
                    ),
                );
            }
            value.round_to_u64_saturating()
        }
        I::Real(_) | I::Float32(_) => real_unsigned()?,
        I::Null | I::MinNotNull | I::MaxValue => unreachable!("guarded by caller"),
        _ => match to_decimal() {
            Ok((value, event)) => {
                let result = value.as_ref().round_to_u64_saturating();
                drop(value);
                drop(event);
                result
            }
            Err(_) => 0,
        },
    })
}

/// Complete ordinary Signed/Unsigned/UnsignedInUnion control. Static eval type
/// is consulted only for UNION. Signed reads the actual timezone
/// unconditionally after successful warnings; unsigned only reads it on its
/// original signed conversion branches. Each callback is a demanded original
/// operation.
#[allow(clippy::too_many_arguments)]
pub fn native_cast_integer<E, EI, ED, WI, WD, Z>(
    input: NativeCastIntegerInput<'_>,
    target: NativeCastIntegerTarget,
    source_eval_type: Option<NativeCastIntegerEvalType>,
    json_text: impl FnOnce() -> String,
    time_zone: impl FnOnce() -> Z,
    to_signed: impl FnOnce(&Z) -> Result<(i64, WI), EI>,
    to_decimal: impl FnOnce() -> Result<(NativeDecimalParseValue, WD), ED>,
    real_unsigned: impl FnOnce() -> Result<u64, E>,
    mut handle_truncate: impl FnMut(&str) -> Result<(), E>,
    mut append_warning: impl FnMut(u16, &str),
) -> Result<NativeCastIntegerResult, E> {
    if target == NativeCastIntegerTarget::UnsignedInUnion && union_negative(input, source_eval_type)
    {
        return Ok(NativeCastIntegerResult::Unsigned(0));
    }
    native_cast_integer_input_warning(input, json_text, &mut handle_truncate)?;
    if target == NativeCastIntegerTarget::Signed {
        signed_warning(input, &mut append_warning);
        let zone = time_zone();
        return Ok(NativeCastIntegerResult::Signed(
            native_cast_integer_signed_value(input, &zone, to_signed),
        ));
    }
    negative_string_warning(input, &mut append_warning);
    native_cast_integer_unsigned_value(
        input,
        time_zone,
        to_signed,
        to_decimal,
        real_unsigned,
        append_warning,
    )
    .map(NativeCastIntegerResult::Unsigned)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use NativeCastIntegerInput as I;
    use NativeCastIntegerResult as R;
    use NativeCastIntegerTarget as T;

    use super::*;
    fn run(
        input: I<'_>,
        target: T,
        source: Option<NativeCastIntegerEvalType>,
        reject: bool,
    ) -> (Result<R, &'static str>, Vec<String>) {
        let calls = RefCell::new(Vec::new());
        let value = native_cast_integer::<&str, &str, &str, (), (), u8>(
            input,
            target,
            source,
            || {
                calls.borrow_mut().push("json".into());
                "{}".into()
            },
            || {
                calls.borrow_mut().push("zone".into());
                17
            },
            |zone| {
                assert_eq!(*zone, 17);
                calls.borrow_mut().push("signed".into());
                Ok((2, ()))
            },
            || {
                calls.borrow_mut().push("decimal".into());
                Ok((NativeDecimalParseValue::from_int(2), ()))
            },
            || {
                calls.borrow_mut().push("real worker".into());
                Ok(2)
            },
            |message| {
                calls.borrow_mut().push(format!("truncate {message}"));
                if reject {
                    Err("truncate error")
                } else {
                    Ok(())
                }
            },
            |code, message| calls.borrow_mut().push(format!("append {code} {message}")),
        );
        (value, calls.into_inner())
    }
    #[test]
    fn integer_controllers_preserve_effect_order_static_union_and_error_domains() {
        let (value, calls) = run(I::String(b"18446744073709551615"), T::Signed, None, false);
        assert_eq!(value, Ok(R::Signed(-1)));
        assert_eq!(
            calls,
            [
                "append 8030 Cast to signed converted positive out-of-range integer to its negative complement",
                "zone"
            ]
        );
        let (value, calls) = run(I::String(b"-5"), T::Unsigned, None, false);
        assert_eq!(value, Ok(R::Unsigned(u64::MAX - 4)));
        assert_eq!(
            calls,
            [
                "append 8031 Cast to unsigned converted negative integer to it's positive complement",
                "zone"
            ]
        );
        let (value, calls) = run(I::Bytes(b"1x"), T::Signed, None, true);
        assert_eq!(value, Err("truncate error"));
        assert_eq!(calls, ["truncate Truncated incorrect INTEGER value: '1x'"]);
        let (value, calls) = run(
            I::Json {
                type_code: JsonType::Object as u8,
            },
            T::Signed,
            None,
            true,
        );
        assert_eq!(value, Err("truncate error"));
        assert_eq!(
            calls,
            ["json", "truncate Truncated incorrect INTEGER value: '{}'"]
        );
        let (value, calls) = run(
            I::Json {
                type_code: JsonType::I64 as u8,
            },
            T::Signed,
            None,
            false,
        );
        assert_eq!(value, Ok(R::Signed(2)));
        assert_eq!(calls, ["zone", "signed"]);
        let (value, calls) = run(I::Real(2.5), T::Signed, None, false);
        assert_eq!(value, Ok(R::Signed(2)));
        assert_eq!(calls, ["zone"]);
        // Both actual source conversions answer 2; only the demanded service
        // distinguishes Float32 here, not a fabricated tie-rounding difference.
        let (value, calls) = run(I::Float32(2.5), T::Signed, None, false);
        assert_eq!(value, Ok(R::Signed(2)));
        assert_eq!(calls, ["zone", "signed"]);
        let (value, calls) = run(I::Real(f64::NAN), T::Signed, None, false);
        assert_eq!(value, Ok(R::Signed(0)));
        assert_eq!(calls, ["append 1690 constant NaN overflows bigint", "zone"]);
        let (_, calls) = run(I::Real(2.5), T::Unsigned, None, false);
        assert_eq!(calls, ["real worker"]);
        let (_, calls) = run(I::UInt(2), T::Unsigned, None, false);
        assert!(calls.is_empty());
        let (_, calls) = run(I::Int(2), T::Unsigned, None, false);
        assert_eq!(calls, ["zone"]);
        let (_, calls) = run(I::Time, T::Unsigned, None, false);
        assert_eq!(calls, ["zone", "signed"]);
        let (_, calls) = run(I::Duration, T::Unsigned, None, false);
        assert_eq!(calls, ["zone", "signed"]);
        let (_, calls) = run(I::Other, T::Unsigned, None, false);
        assert_eq!(calls, ["decimal"]);
        let (value, calls) = run(
            I::Bytes(b"-x"),
            T::UnsignedInUnion,
            Some(NativeCastIntegerEvalType::String),
            true,
        );
        assert_eq!(value, Ok(R::Unsigned(0)));
        assert!(calls.is_empty());
        let (value, calls) = run(
            I::Int(-2),
            T::UnsignedInUnion,
            Some(NativeCastIntegerEvalType::Datetime),
            false,
        );
        assert_eq!(value, Ok(R::Unsigned(u64::MAX - 1)));
        assert_eq!(calls, ["zone"]);
        let (value, calls) = run(
            I::Real(-2.5),
            T::UnsignedInUnion,
            Some(NativeCastIntegerEvalType::Real),
            true,
        );
        assert_eq!(value, Ok(R::Unsigned(0)));
        assert!(calls.is_empty());
        let value = native_cast_integer_unsigned_value::<&str, (), (), (), (), ()>(
            I::Real(1.0),
            || panic!("zone"),
            |_| unreachable!(),
            || unreachable!(),
            || Err("infrastructure"),
            |_, _| unreachable!(),
        );
        assert_eq!(value, Err("infrastructure"));
        assert_eq!(
            native_cast_integer_signed_value::<(), &str, ()>(I::Other, &(), |_| Err("datatype")),
            0
        );
        assert_eq!(
            native_cast_integer_unsigned_value::<(), (), &str, (), (), ()>(
                I::Other,
                || unreachable!(),
                |_| unreachable!(),
                || Err("datatype"),
                || unreachable!(),
                |_, _| unreachable!()
            ),
            Ok(0)
        );
        struct Event<'a>(&'a RefCell<Vec<&'static str>>);
        impl Drop for Event<'_> {
            fn drop(&mut self) {
                self.0.borrow_mut().push("discard actual event");
            }
        }
        let calls = RefCell::new(Vec::new());
        assert_eq!(
            native_cast_integer_signed_value::<(), (), _>(I::Other, &(), |_| {
                calls.borrow_mut().push("convert");
                Ok((2, Event(&calls)))
            }),
            2
        );
        assert_eq!(*calls.borrow(), ["convert", "discard actual event"]);
    }
    #[test]
    fn integer_prefix_value_helpers_and_decimal_rounds_keep_original_domains() {
        for (text, expected) in [
            (" +12.3e4 ", 12),
            ("18446744073709551615", -1),
            ("18446744073709551616", -1),
            ("-9223372036854775809", i64::MIN),
            ("-", 0),
            ("\u{2003}-5tail", -5),
        ] {
            assert_eq!(
                native_cast_integer_signed_value::<(), (), ()>(
                    I::String(text.as_bytes()),
                    &(),
                    |_| unreachable!()
                ),
                expected
            );
        }
        assert_eq!(
            native_cast_integer_signed_value::<(), (), ()>(
                I::Bytes(&[0xff]),
                &(),
                |_| unreachable!()
            ),
            0
        );
        assert_eq!(
            native_cast_integer_signed_value::<(), (), ()>(
                I::UInt(u64::MAX),
                &(),
                |_| unreachable!()
            ),
            -1
        );
        assert!(int_prefix_consumed_all(" \u{2003}+12\u{2003} "));
        assert!(!int_prefix_consumed_all("+"));
        assert!(!int_prefix_consumed_all("12e0"));
        assert!(!signed_string_integer_parse_overflows(
            "18446744073709551615"
        ));
        assert!(signed_string_integer_parse_overflows(
            "18446744073709551616"
        ));
        assert!(signed_string_integer_parse_overflows(
            "-9223372036854775809"
        ));
        fn decimal(negative: bool, digits: &[u8], scale: u32) -> NativeDecimalParseRef<'_> {
            NativeDecimalParseRef {
                negative,
                digits,
                scale,
                storage_scale: scale,
                declared_shape: None,
            }
        }
        let negative = decimal(true, b"20".as_slice(), 1);
        let (value, calls) = run(I::Decimal(negative), T::Unsigned, None, false);
        assert_eq!(value, Ok(R::Unsigned(0)));
        assert_eq!(
            calls,
            ["append 1292 Truncated incorrect DECIMAL value: '-2.0'"]
        );
        let tiny = decimal(true, b"4".as_slice(), 1);
        let (value, calls) = run(
            I::Decimal(tiny),
            T::UnsignedInUnion,
            Some(NativeCastIntegerEvalType::Decimal),
            false,
        );
        assert_eq!(value, Ok(R::Unsigned(0)));
        assert!(calls.is_empty());
        let upper = decimal(false, b"18446744073709551615".as_slice(), 0);
        let (value, calls) = run(I::Decimal(upper), T::Unsigned, None, false);
        assert_eq!(value, Ok(R::Unsigned(u64::MAX)));
        assert!(calls.is_empty());
        assert_eq!(
            native_cast_integer_signed_value::<(), (), ()>(
                I::Decimal(decimal(false, b"25".as_slice(), 1)),
                &(),
                |_| unreachable!()
            ),
            3
        );
        let warnings = RefCell::new(Vec::new());
        let value = native_cast_integer_unsigned_value::<(), (), (), (), (), ()>(
            I::String(b"-5x"),
            || (),
            |_| unreachable!(),
            || unreachable!(),
            || unreachable!(),
            |code, _| warnings.borrow_mut().push(code),
        );
        assert_eq!(value, Ok(u64::MAX - 4));
        assert!(warnings.borrow().is_empty()); // value-only does not acquire full CAST warnings
        for sentinel in [I::Null, I::MinNotNull, I::MaxValue] {
            assert_eq!(
                native_cast_integer_input_warning::<()>(
                    sentinel,
                    || panic!("JSON"),
                    |_| panic!("truncation")
                ),
                Ok(())
            );
            assert!(
                std::panic::catch_unwind(|| native_cast_integer_signed_value::<(), (), ()>(
                    sentinel,
                    &(),
                    |_| panic!("default conversion")
                ))
                .is_err()
            );
            assert!(
                std::panic::catch_unwind(|| native_cast_integer_unsigned_value::<
                    (),
                    (),
                    (),
                    (),
                    (),
                    (),
                >(
                    sentinel,
                    || panic!("zone"),
                    |_| panic!("signed"),
                    || panic!("decimal"),
                    || panic!("worker"),
                    |_, _| panic!("warning")
                ))
                .is_err()
            );
        }
    }
}
