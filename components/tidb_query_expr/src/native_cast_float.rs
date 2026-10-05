// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Ordinary DOUBLE/FLOAT and the distinct value-only numeric helper. Source
//! selection, diagnostic demand and conversion error/event folding live here;
//! callbacks retain only original JSON Display, default-context datatype
//! conversion and statement truncation handling. No new facade is introduced.
use tidb_query_datatype::codec::{
    mysql::{Decimal, NativeDecimalParseRef},
    native_float_parse::{native_float_warning_input, native_str_to_float},
    native_numeric::NativeNumericInput,
    native_scalar_convert::native_datum_to_f64,
    native_sql_string::{NativeSqlStringInput, native_sql_string},
};

use crate::native_cast_decimal_prefix;

#[derive(Clone, Copy, Debug)]
pub enum NativeCastFloatInput<'a> {
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
    Json,
    Other,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCastFloatTarget {
    Double,
    Float,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeCastFloatError<E> {
    Child(E),
    ConstantFloatCastOverflow { value: String },
}

fn numeric_input(number: NativeNumericInput<'_>) -> NativeCastFloatInput<'_> {
    use NativeCastFloatInput as I;
    use NativeNumericInput as N;
    match number {
        N::Null => I::Null,
        N::MinNotNull => I::MinNotNull,
        N::MaxValue => I::MaxValue,
        N::Int(value) => I::Int(value),
        N::UInt(value) => I::UInt(value),
        N::Decimal(value) => I::Decimal(value),
        N::Real(value) => I::Real(value),
        N::Float32(value) => I::Float32(value),
        N::String(value) => I::String(value),
        N::Bytes(value) => I::Bytes(value),
        N::Json { .. } => I::Json,
        _ => I::Other,
    }
}

/// Closed ordinary FLOAT/DOUBLE composition. JSON Display and the datatype
/// fallback are SDK-owned; only the original truncation effect crosses out.
pub fn native_cast_float_numeric<E>(
    number: NativeNumericInput<'_>,
    target: NativeCastFloatTarget,
    handle_truncate: impl FnMut(&str) -> Result<(), E>,
) -> Result<f64, NativeCastFloatError<E>> {
    native_cast_float(
        numeric_input(number),
        target,
        || {
            let NativeNumericInput::Json { type_code, value } = number else {
                unreachable!("SDK JSON display request needs an actual JSON datum");
            };
            native_sql_string(NativeSqlStringInput::Json { type_code, value })
                .expect("JSON Display produces UTF-8")
        },
        || native_datum_to_f64(number).map(|converted| (converted.value, converted.truncated)),
        handle_truncate,
    )
}
/// Closed value-only numeric helper. It intentionally does not use ordinary
/// JSON Display/lossy UTF-8 parsing or publish datatype truncation events.
pub fn native_cast_float_numeric_value(number: NativeNumericInput<'_>) -> f64 {
    native_cast_float_value(numeric_input(number), || {
        native_datum_to_f64(number).map(|converted| (converted.value, converted.truncated))
    })
}

/// The original naked to_f64_for_cast policy. Strings use strict UTF-8 and the
/// numeric decimal-prefix helper, NOT the ordinary cast's lossy byte parser.
/// Float32 and JSON stay on the actual default-context datatype service.
pub fn native_cast_float_value<CE, W>(
    input: NativeCastFloatInput<'_>,
    to_f64: impl FnOnce() -> Result<(f64, W), CE>,
) -> f64 {
    use NativeCastFloatInput as I;
    match input {
        I::Int(value) => value as f64,
        I::UInt(value) => value as f64,
        I::Decimal(value) => value.to_f64(),
        I::Real(value) => value,
        I::String(bytes) | I::Bytes(bytes) => std::str::from_utf8(bytes)
            .map(native_cast_decimal_prefix)
            .map_or(0.0, |value| value.as_ref().to_f64()),
        I::Null | I::MinNotNull | I::MaxValue => unreachable!("guarded by caller"),
        I::Float32(_) | I::Json | I::Other => match to_f64() {
            Ok((value, event)) => {
                drop(event);
                value
            }
            Err(_) => 0.0,
        },
    }
}

/// Ordinary FLOAT narrows only actual Real/Float32 inputs. Other source kinds
/// retain their f64 conversion in range, or return the typed constant-overflow
/// error. Ordinary textual parsing publishes only its final truncation event,
/// not the datatype reported parser's potentially multiple internal events.
pub fn native_cast_float<E, CE, W>(
    input: NativeCastFloatInput<'_>,
    target: NativeCastFloatTarget,
    json_text: impl FnOnce() -> String,
    to_f64: impl FnOnce() -> Result<(f64, W), CE>,
    mut handle_truncate: impl FnMut(&str) -> Result<(), E>,
) -> Result<f64, NativeCastFloatError<E>> {
    use NativeCastFloatInput as I;
    if target == NativeCastFloatTarget::Float {
        if let I::Real(value) | I::Float32(value) = input {
            let narrowed = value as f32;
            return Ok(if narrowed.is_infinite() {
                0.0
            } else {
                f64::from(narrowed)
            });
        }
    }
    let textual = match input {
        I::String(bytes) | I::Bytes(bytes) => {
            Some((String::from_utf8_lossy(bytes).into_owned(), "DOUBLE"))
        }
        I::Json => Some((json_text(), "FLOAT")),
        _ => None,
    };
    let value = if let Some((text, type_word)) = textual {
        let converted = native_str_to_float(&text, true);
        if converted.truncated {
            handle_truncate(&format!(
                "Truncated incorrect {type_word} value: '{}'",
                native_float_warning_input(&text)
            ))
            .map_err(NativeCastFloatError::Child)?;
        }
        converted.value
    } else {
        native_cast_float_value(input, to_f64)
    };
    if target == NativeCastFloatTarget::Float && value.abs() > f64::from(f32::MAX) {
        return Err(NativeCastFloatError::ConstantFloatCastOverflow {
            value: Decimal::native_format_float_g_shortest(value),
        });
    }
    Ok(value)
}

#[cfg(test)]
mod numeric_tests {
    use NativeCastFloatError as E;
    use NativeCastFloatTarget as T;
    use NativeNumericInput as N;

    use super::*;
    fn cast(number: N<'_>, target: T, reject: bool) -> (Result<f64, E<&'static str>>, Vec<String>) {
        let mut warnings = Vec::new();
        let value = native_cast_float_numeric(number, target, |message| {
            warnings.push(message.to_owned());
            if reject {
                Err("truncate error")
            } else {
                Ok(())
            }
        });
        (value, warnings)
    }
    #[test]
    fn closed_float_composition_keeps_actual_sources_and_truncation_veto_order() {
        assert_eq!(
            cast(N::Bytes(b"1\xff"), T::Double, false),
            (
                Ok(1.0),
                vec!["Truncated incorrect DOUBLE value: '1�'".into()]
            )
        );
        assert_eq!(native_cast_float_numeric_value(N::Bytes(b"1\xff")), 0.0);
        assert_eq!(
            native_cast_float_numeric_value(N::String(b"3.5e1tail")),
            35.0
        );
        let json = N::Json {
            type_code: 12,
            value: b"\x032.5",
        };
        assert_eq!(
            cast(json, T::Double, false),
            (
                Ok(0.0),
                vec!["Truncated incorrect FLOAT value: '\"2.5\"'".into()]
            )
        );
        assert_eq!(native_cast_float_numeric_value(json), 2.5);
        assert_eq!(
            cast(N::String(b"1e400x"), T::Float, true),
            (
                Err(E::Child("truncate error")),
                vec!["Truncated incorrect DOUBLE value: '1e400x'".into()]
            )
        );
        assert_eq!(
            cast(N::String(b"1e400x"), T::Double, false),
            (
                Ok(f64::MAX),
                vec!["Truncated incorrect DOUBLE value: '1e400x'".into()]
            )
        );
        assert_eq!(
            cast(N::String(b"1e300"), T::Float, false),
            (
                Err(E::ConstantFloatCastOverflow {
                    value: "1e+300".into()
                }),
                vec![]
            )
        );
        assert_eq!(cast(N::Real(1e300), T::Float, true), (Ok(0.0), vec![]));
        assert_eq!(
            cast(N::Real(0.1), T::Float, false),
            (Ok(0.10000000149011612), vec![])
        );
        assert_eq!(cast(N::String(b"0.1"), T::Float, false), (Ok(0.1), vec![]));
        assert_eq!(
            cast(N::Int(123456789), T::Float, false),
            (Ok(123456789.0), vec![])
        );
        assert_eq!(
            cast(N::Real(123456789.0), T::Float, false),
            (Ok(123456792.0), vec![])
        );
        for target in [T::Float, T::Double] {
            assert_eq!(
                cast(N::Float32(16_777_217.0), target, false),
                (Ok(16_777_216.0), vec![])
            );
        }
        let hidden = NativeDecimalParseRef {
            negative: false,
            digits: b"12345",
            scale: 2,
            storage_scale: 4,
            declared_shape: Some((10, 2)),
        };
        assert_eq!(
            cast(N::Decimal(hidden), T::Float, false),
            (Ok(1.23), vec![])
        );
        assert_eq!(native_cast_float_numeric_value(N::Decimal(hidden)), 1.23);
        for input in [
            N::Enum(u64::MAX),
            N::Set(u64::MAX),
            N::BinaryLiteral(&[1; 9]),
            N::Bit(&[1; 9]),
        ] {
            assert_eq!(
                cast(input, T::Double, true),
                (Ok(18_446_744_073_709_551_616.0), vec![])
            );
            assert_eq!(
                native_cast_float_numeric_value(input),
                18_446_744_073_709_551_616.0
            );
        }
    }
    #[test]
    fn closed_float_domains_keep_signed_zero_nonfinite_json_and_original_guards() {
        for input in [N::Real(-0.0), N::Float32(-0.0)] {
            for target in [T::Float, T::Double] {
                let (value, warnings) = cast(input, target, false);
                assert_eq!(value.unwrap().to_bits(), (-0.0f64).to_bits());
                assert!(warnings.is_empty());
            }
            assert_eq!(
                native_cast_float_numeric_value(input).to_bits(),
                (-0.0f64).to_bits()
            );
        }
        assert_eq!(
            native_cast_float_numeric_value(N::String(b"-0")).to_bits(),
            0.0f64.to_bits()
        );
        assert_eq!(
            cast(N::String(b"-0"), T::Double, false)
                .0
                .unwrap()
                .to_bits(),
            (-0.0f64).to_bits()
        );
        for input in [N::Real(f64::NAN), N::Float32(f64::NAN)] {
            assert!(native_cast_float_numeric_value(input).is_nan());
            for target in [T::Float, T::Double] {
                let (value, warnings) = cast(input, target, true);
                assert!(value.unwrap().is_nan());
                assert!(warnings.is_empty());
            }
        }
        for value in [f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(cast(N::Real(value), T::Double, true), (Ok(value), vec![]));
            assert_eq!(
                cast(N::Real(value), T::Float, true).0.unwrap().to_bits(),
                0.0f64.to_bits()
            );
            let bytes = value.to_le_bytes();
            let json = N::Json {
                type_code: 11,
                value: &bytes,
            };
            assert_eq!(native_cast_float_numeric_value(json), value);
            assert!(std::panic::catch_unwind(|| cast(json, T::Double, false)).is_err());
        }
        // Display suppresses a bad-width numeric payload's decode error, while
        // the value-only numeric JSON accessor retains its expect panic.
        let malformed = N::Json {
            type_code: 11,
            value: &[],
        };
        assert_eq!(cast(malformed, T::Double, false), (Ok(0.0), vec![]));
        assert!(std::panic::catch_unwind(|| native_cast_float_numeric_value(malformed)).is_err());
        let vector = tidb_query_datatype::codec::mysql::NativeVectorFloat32::must_create(vec![1.0]);
        for input in [N::Raw(b"12"), N::VectorFloat32(&vector)] {
            assert_eq!(cast(input, T::Double, true), (Ok(0.0), vec![]));
            assert_eq!(native_cast_float_numeric_value(input), 0.0);
        }
        for input in [N::Null, N::MinNotNull, N::MaxValue] {
            assert!(std::panic::catch_unwind(|| native_cast_float_numeric_value(input)).is_err());
            assert!(std::panic::catch_unwind(|| cast(input, T::Double, true)).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use NativeCastFloatError as E;
    use NativeCastFloatInput as I;
    use NativeCastFloatTarget as T;

    use super::*;
    fn cast(input: I<'_>, target: T, reject: bool) -> (Result<f64, E<&'static str>>, Vec<String>) {
        let calls = RefCell::new(Vec::new());
        let result = native_cast_float::<&str, &str, ()>(
            input,
            target,
            || {
                calls.borrow_mut().push("json".into());
                "{}".into()
            },
            || {
                calls.borrow_mut().push("default conversion".into());
                Ok((1.25, ()))
            },
            |message| {
                calls.borrow_mut().push(message.to_owned());
                if reject {
                    Err("truncate error")
                } else {
                    Ok(())
                }
            },
        );
        (result, calls.into_inner())
    }
    #[test]
    fn ordinary_float_cast_keeps_parser_diagnostic_demand_and_target_asymmetry() {
        let (value, calls) = cast(I::Bytes(&[b'1', 0xff]), T::Double, false);
        assert_eq!(value, Ok(1.0));
        assert_eq!(calls, ["Truncated incorrect DOUBLE value: '1�'"]);
        let (value, calls) = cast(I::String(b"1e"), T::Double, false);
        assert_eq!(value, Ok(1.0));
        assert!(calls.is_empty());
        let (value, calls) = cast(I::String(b"1\0suffix"), T::Double, false);
        assert_eq!(value, Ok(1.0));
        assert!(calls.is_empty());
        let (value, calls) = cast(I::Bytes(b"\0suffix"), T::Double, false);
        assert_eq!(value, Ok(0.0));
        assert_eq!(calls, ["Truncated incorrect DOUBLE value: ''"]);
        let (value, calls) = cast(I::String("\u{2003} \t".as_bytes()), T::Double, false);
        assert_eq!(value, Ok(0.0));
        assert!(calls.is_empty());
        let (value, calls) = cast(I::Json, T::Double, true);
        assert_eq!(value, Err(E::Child("truncate error")));
        assert_eq!(calls, ["json", "Truncated incorrect FLOAT value: '{}'"]);
        let (value, calls) = cast(I::String(b"1e400x"), T::Double, false);
        assert_eq!(value, Ok(f64::MAX));
        assert_eq!(calls, ["Truncated incorrect DOUBLE value: '1e400x'"]); // one final event, not two parser diagnostics
        let (value, calls) = cast(I::String(b"1e400x"), T::Float, true);
        assert_eq!(value, Err(E::Child("truncate error")));
        assert_eq!(calls.len(), 1); // truncation error wins over the later overflow decision
        let (value, calls) = cast(I::String(b"1e300"), T::Float, false);
        assert_eq!(
            value,
            Err(E::ConstantFloatCastOverflow {
                value: "1e+300".into()
            })
        );
        assert!(calls.is_empty());
        let (value, calls) = cast(I::Real(1e300), T::Float, true);
        assert_eq!(value, Ok(0.0));
        assert!(calls.is_empty());
        let (value, calls) = cast(I::Float32(f64::INFINITY), T::Float, true);
        assert_eq!(value, Ok(0.0));
        assert!(calls.is_empty());
        let (value, calls) = cast(I::Real(f64::NAN), T::Float, true);
        assert!(value.unwrap().is_nan());
        assert!(calls.is_empty());
        let (value, calls) = cast(I::Real(-0.0), T::Float, true);
        assert_eq!(value.unwrap().to_bits(), (-0.0_f64).to_bits());
        assert!(calls.is_empty());
        let (value, calls) = cast(I::Real(0.1), T::Float, false);
        assert_eq!(value, Ok(f64::from(0.1_f32)));
        assert!(calls.is_empty());
        let (value, calls) = cast(I::String(b"0.1"), T::Float, false);
        assert_eq!(value, Ok(0.1_f64));
        assert!(calls.is_empty()); // no narrowing for text
        let (value, calls) = cast(I::Int(123456789), T::Float, false);
        assert_eq!(value, Ok(123456789.0));
        assert!(calls.is_empty());
        let (value, calls) = cast(I::Real(123456789.0), T::Float, false);
        assert_eq!(value, Ok(123456792.0));
        assert!(calls.is_empty());
        let (value, calls) = cast(I::Float32(1.25), T::Double, false);
        assert_eq!(value, Ok(1.25));
        assert_eq!(calls, ["default conversion"]);
        let (value, calls) = cast(I::Float32(1.25), T::Float, false);
        assert_eq!(value, Ok(1.25));
        assert!(calls.is_empty());
    }
    #[test]
    fn naked_float_value_retains_strict_prefix_default_event_folding_and_guards() {
        assert_eq!(
            native_cast_float_value::<(), ()>(I::Bytes(&[b'1', 0xff]), || unreachable!()),
            0.0
        );
        assert_eq!(
            native_cast_float_value::<(), ()>(I::String(b"3.5e1tail"), || unreachable!()),
            35.0
        );
        assert_eq!(
            native_cast_float_value::<(), ()>(I::String(b"-0"), || unreachable!()).to_bits(),
            0.0_f64.to_bits()
        );
        assert_eq!(
            native_cast_float_value::<(), ()>(I::Real(-0.0), || unreachable!()).to_bits(),
            (-0.0_f64).to_bits()
        );
        assert_eq!(
            native_cast_float_value::<(), ()>(I::UInt(u64::MAX), || unreachable!()),
            u64::MAX as f64
        );
        let hidden = NativeDecimalParseRef {
            negative: false,
            digits: b"12345",
            scale: 2,
            storage_scale: 4,
            declared_shape: Some((10, 2)),
        };
        assert_eq!(
            native_cast_float_value::<(), ()>(I::Decimal(hidden), || unreachable!()),
            1.23
        );
        let (value, calls) = cast(I::Decimal(hidden), T::Double, false);
        assert_eq!(value, Ok(1.23));
        assert!(calls.is_empty());
        assert_eq!(
            native_cast_float_value::<&str, ()>(I::Other, || Err("datatype error")),
            0.0
        );
        let value = native_cast_float::<(), &str, ()>(
            I::Other,
            T::Float,
            || unreachable!(),
            || Err("datatype error"),
            |_| unreachable!(),
        );
        assert_eq!(value, Ok(0.0));
        let value = native_cast_float::<(), (), ()>(
            I::Other,
            T::Float,
            || unreachable!(),
            || Ok((0.1, ())),
            |_| unreachable!(),
        );
        assert_eq!(value, Ok(0.1));
        let value = native_cast_float::<(), (), ()>(
            I::Other,
            T::Float,
            || unreachable!(),
            || Ok((f64::NAN, ())),
            |_| unreachable!(),
        );
        assert!(value.unwrap().is_nan());
        struct Event<'a>(&'a RefCell<Vec<&'static str>>);
        impl Drop for Event<'_> {
            fn drop(&mut self) {
                self.0.borrow_mut().push("discard actual event");
            }
        }
        let calls = RefCell::new(Vec::new());
        let value = native_cast_float_value::<(), _>(I::Json, || {
            calls.borrow_mut().push("default conversion");
            Ok((1.25, Event(&calls)))
        });
        assert_eq!(value, 1.25);
        assert_eq!(
            *calls.borrow(),
            ["default conversion", "discard actual event"]
        );
        for sentinel in [I::Null, I::MinNotNull, I::MaxValue] {
            assert!(
                std::panic::catch_unwind(|| native_cast_float_value::<(), ()>(
                    sentinel,
                    || panic!("default conversion")
                ))
                .is_err()
            );
        }
    }
}
