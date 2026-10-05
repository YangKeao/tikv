// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Context-bearing native Datum.ToDecimal. Fixed-word MyDecimal parsing, JSON
//! getter-chain policy and diagnostic subjects remain distinct from plain
//! decimal conversion. Callbacks perform only the original context/error
//! effects.
use std::fmt;

use super::{
    mysql::{
        Decimal, NativeDecimalParseValue, NativeMyDecimal, NativeMyDecimalError,
        binary_literal::{native_binary_literal_to_int, native_format_binary_literal},
        json::{native_binary_json_string_bytes, write_native_binary_json_text},
        native_decimal_from_my_decimal,
    },
    native_decimal_convert::native_datum_to_decimal,
    native_numeric::{NativeNumericError, NativeNumericInput},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeDecimalContextError {
    Truncated,
    Overflow,
    BadNumber,
    TruncatedWrongValue { message: String },
    BinaryTruncatedWrongValue { literal: String },
}
struct LiteralDisplay<'a>(&'a [u8]);
impl fmt::Display for LiteralDisplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        native_format_binary_literal(self.0, formatter)
    }
}
struct JsonDisplay<'a> {
    type_code: u8,
    value: &'a [u8],
}
impl fmt::Display for JsonDisplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_native_binary_json_text(formatter, self.type_code, self.value)
    }
}
fn decimal_conversion_error(
    error: NativeMyDecimalError,
    input: &[u8],
) -> NativeDecimalContextError {
    match error {
        NativeMyDecimalError::Truncated => NativeDecimalContextError::Truncated,
        NativeMyDecimalError::Overflow => NativeDecimalContextError::Overflow,
        NativeMyDecimalError::BadNumber => NativeDecimalContextError::BadNumber,
        NativeMyDecimalError::TruncatedWrongValue => {
            // All-space/tab input deliberately uses offset zero, not its end.
            let input = &input[input
                .iter()
                .position(|byte| !matches!(byte, b' ' | b'\t'))
                .unwrap_or(0)..];
            let input = if matches!(input.first(), Some(b'+' | b'-')) {
                &input[1..]
            } else {
                input
            };
            NativeDecimalContextError::TruncatedWrongValue {
                message: format!(
                    "Truncated incorrect DECIMAL value: '{}'",
                    String::from_utf8_lossy(input)
                ),
            }
        }
    }
}

/// String/BIT/JSON call the handler even for None. Real/Float32 bypass it and
/// only construct a direct error when parsing produced one. Other admitted
/// storage uses plain conversion's value without its event or context access.
pub fn native_datum_to_decimal_with_context<E>(
    input: NativeNumericInput<'_>,
    mut handle_truncate: impl FnMut(Option<NativeDecimalContextError>) -> Option<E>,
    mut direct_error: impl FnMut(NativeDecimalContextError) -> E,
) -> Result<(NativeDecimalParseValue, Option<E>), NativeNumericError> {
    use NativeNumericInput as I;
    let (parsed, float) = match input {
        I::String(bytes) => {
            let (decimal, error) = NativeMyDecimal::from_string(bytes);
            let value = native_decimal_from_my_decimal(decimal);
            let error = handle_truncate(error.map(|error| decimal_conversion_error(error, bytes)));
            return Ok((value, error));
        }
        I::Real(value) => (NativeMyDecimal::from_float64(value), value),
        I::Float32(value) => {
            let value = f64::from(value as f32);
            (NativeMyDecimal::from_float64(value), value)
        }
        I::BinaryLiteral(bytes) | I::Bit(bytes) => {
            let outcome = native_binary_literal_to_int(bytes);
            let error = outcome.is_truncated().then(|| {
                NativeDecimalContextError::BinaryTruncatedWrongValue {
                    literal: LiteralDisplay(bytes).to_string(),
                }
            });
            // The native ToInt(context) effect precedes decimal construction.
            let error = handle_truncate(error);
            return Ok((NativeDecimalParseValue::from_uint(outcome.value()), error));
        }
        I::Json { type_code, value } => {
            // Match the original optional getter chain. Bad-width numeric
            // payloads fall through to diagnostic Display, not accessor expect.
            let signed = if type_code == 9 {
                value.try_into().ok().map(i64::from_le_bytes)
            } else {
                None
            };
            let unsigned = if type_code == 10 {
                value.try_into().ok().map(u64::from_le_bytes)
            } else {
                None
            };
            let real = if type_code == 11 {
                value
                    .try_into()
                    .ok()
                    .map(|bytes| f64::from_bits(u64::from_le_bytes(bytes)))
            } else {
                None
            };
            let (decimal, error) = if let Some(value) = signed {
                (NativeDecimalParseValue::from_int(value), None)
            } else if let Some(value) = unsigned {
                (NativeDecimalParseValue::from_uint(value), None)
            } else if let Some(value) = real {
                let (decimal, error) = NativeMyDecimal::from_float64(value);
                (
                    native_decimal_from_my_decimal(decimal),
                    error.map(|error| {
                        decimal_conversion_error(
                            error,
                            Decimal::native_format_float_g_shortest(value).as_bytes(),
                        )
                    }),
                )
            } else if let Some(bytes) = native_binary_json_string_bytes(type_code, value) {
                let (decimal, error) = NativeMyDecimal::from_string(bytes);
                (
                    native_decimal_from_my_decimal(decimal),
                    error.map(|error| decimal_conversion_error(error, bytes)),
                )
            } else if type_code == 4 && value[0] != 0 {
                (
                    NativeDecimalParseValue::from_int(i64::from(value[0] != 2)),
                    None,
                )
            } else {
                (
                    NativeDecimalParseValue::from_int(0),
                    Some(NativeDecimalContextError::TruncatedWrongValue {
                        message: format!(
                            "Truncated incorrect DECIMAL value: '{}'",
                            JsonDisplay { type_code, value }
                        ),
                    }),
                )
            };
            return Ok((decimal, handle_truncate(error)));
        }
        I::Int(_)
        | I::UInt(_)
        | I::Decimal(_)
        | I::Time(_)
        | I::Duration(_)
        | I::Enum(_)
        | I::Set(_) => {
            return Ok((native_datum_to_decimal(input)?.value, None));
        }
        I::Bytes(_) | I::Raw(_) | I::VectorFloat32(_) | I::Null | I::MinNotNull | I::MaxValue => {
            return Err(NativeNumericError::Unsupported);
        }
    };
    let value = native_decimal_from_my_decimal(parsed.0);
    let error = parsed.1.map(|error| {
        direct_error(decimal_conversion_error(
            error,
            Decimal::native_format_float_g_shortest(float).as_bytes(),
        ))
    });
    Ok((value, error))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use NativeDecimalContextError as E;
    use NativeNumericInput as I;

    use super::*;
    fn text(value: &NativeDecimalParseValue) -> String {
        let value = value.as_ref();
        Decimal::native_format_visible(
            value.negative,
            value.digits,
            value.scale,
            value.storage_scale,
        )
    }
    fn run(
        input: I<'_>,
        suppress: bool,
    ) -> (
        Result<(NativeDecimalParseValue, Option<E>), NativeNumericError>,
        Vec<(&'static str, Option<E>)>,
    ) {
        let calls = RefCell::new(Vec::new());
        let value = native_datum_to_decimal_with_context(
            input,
            |error| {
                calls.borrow_mut().push(("handle", error.clone()));
                if suppress { None } else { error }
            },
            |error| {
                calls.borrow_mut().push(("direct", Some(error.clone())));
                error
            },
        );
        (value, calls.into_inner())
    }
    #[test]
    fn decimal_context_keeps_handler_demand_float_bypass_and_exact_storage_projection() {
        for input in [I::String(b"1.25"), I::Bit(&[1]), I::BinaryLiteral(&[1])] {
            let (result, calls) = run(input, false);
            let (_, error) = result.unwrap();
            assert_eq!(error, None);
            assert_eq!(calls, vec![("handle", None)]);
        }
        for input in [
            I::Real(1.25),
            I::Float32(1.25),
            I::Int(1),
            I::UInt(1),
            I::Enum(1),
            I::Set(1),
        ] {
            let (result, calls) = run(input, false);
            assert_eq!(result.unwrap().1, None);
            assert!(calls.is_empty());
        }
        let (result, calls) = run(I::Float32(16_777_217.0), false);
        assert_eq!(text(&result.unwrap().0), "16777216");
        assert!(calls.is_empty());
        let (result, calls) = run(I::String(b"123x"), true);
        let (value, error) = result.unwrap();
        assert_eq!(text(&value), "123");
        assert_eq!(error, None);
        assert_eq!(calls, vec![("handle", Some(E::Truncated))]);
        let error = E::TruncatedWrongValue {
            message: "Truncated incorrect DECIMAL value: 'Inf'".into(),
        };
        let (result, calls) = run(I::Real(f64::INFINITY), true);
        assert_eq!(text(&result.as_ref().unwrap().0), "0");
        assert_eq!(result.unwrap().1, Some(error.clone()));
        assert_eq!(calls, vec![("direct", Some(error))]);
        let (result, calls) = run(I::Real(1e100), true);
        assert_eq!(result.unwrap().1, Some(E::Overflow));
        assert_eq!(calls, vec![("direct", Some(E::Overflow))]);
        let bytes = [0, 1, 1, 1, 1, 1, 1, 1, 1, 1];
        let error = E::BinaryTruncatedWrongValue {
            literal: "0x00010101010101010101".into(),
        };
        let (result, calls) = run(I::Bit(&bytes), false);
        let (value, actual) = result.unwrap();
        assert_eq!(text(&value), "18446744073709551615");
        assert_eq!(actual, Some(error.clone()));
        assert_eq!(calls, vec![("handle", Some(error))]);
        assert_eq!(LiteralDisplay(&[]).to_string(), "");
        assert_eq!(LiteralDisplay(&[0, 10, 255]).to_string(), "0x000aff");
        for input in [
            I::Bytes(b"1"),
            I::Raw(b"1"),
            I::Null,
            I::MinNotNull,
            I::MaxValue,
        ] {
            let (result, calls) = run(input, false);
            assert!(matches!(result, Err(NativeNumericError::Unsupported)));
            assert!(calls.is_empty());
        }
        let (decimal, error) = NativeMyDecimal::from_string(b"12.34");
        assert_eq!(error, None);
        // The setter admits only result_frac <= digits_frac. Exercise the
        // projection's wider-result case through the exact raw transport.
        let mut parts = decimal.raw_parts();
        parts.2 = 4;
        let value = native_decimal_from_my_decimal(NativeMyDecimal::from_raw_parts(parts));
        let parts = value.as_ref();
        assert_eq!(
            (
                parts.digits,
                parts.scale,
                parts.storage_scale,
                parts.declared_shape
            ),
            (&b"123400"[..], 4, 4, None)
        );
        let (mut decimal, _) = NativeMyDecimal::from_string(b"12.3456");
        decimal.set_result_frac(2);
        let value = native_decimal_from_my_decimal(decimal);
        let parts = value.as_ref();
        assert_eq!(
            (parts.digits, parts.scale, parts.storage_scale),
            (&b"123456"[..], 2, 4)
        );
    }
    #[test]
    fn decimal_context_json_getter_chain_and_diagnostic_subjects_preserve_original_boundaries() {
        for (input, message) in [
            (&b" \t-xyz"[..], "Truncated incorrect DECIMAL value: 'xyz'"),
            (&b" \t"[..], "Truncated incorrect DECIMAL value: ' \t'"),
            (&b"\t+\xff"[..], "Truncated incorrect DECIMAL value: '�'"),
            (
                &b"\n+xyz"[..],
                "Truncated incorrect DECIMAL value: '\n+xyz'",
            ),
        ] {
            let error = E::TruncatedWrongValue {
                message: message.into(),
            };
            let (result, calls) = run(I::String(input), false);
            assert_eq!(result.unwrap().1, Some(error.clone()));
            assert_eq!(calls, vec![("handle", Some(error))]);
        }
        for (tag, value, expected) in [
            (9, &1i64.to_le_bytes()[..], "1"),
            (10, &[255; 8][..], "18446744073709551615"),
            (4, &[2][..], "0"),
            (4, &[255][..], "1"),
        ] {
            let (result, calls) = run(
                I::Json {
                    type_code: tag,
                    value,
                },
                false,
            );
            let (value, error) = result.unwrap();
            assert_eq!(text(&value), expected);
            assert_eq!(error, None);
            assert_eq!(calls, vec![("handle", None)]);
        }
        let bytes = 1e100f64.to_le_bytes();
        let (result, calls) = run(
            I::Json {
                type_code: 11,
                value: &bytes,
            },
            true,
        );
        assert_eq!(result.unwrap().1, None);
        assert_eq!(calls, vec![("handle", Some(E::Overflow))]);
        let inf = f64::INFINITY.to_le_bytes();
        let (result, calls) = run(
            I::Json {
                type_code: 11,
                value: &inf,
            },
            false,
        );
        let error = E::TruncatedWrongValue {
            message: "Truncated incorrect DECIMAL value: 'Inf'".into(),
        };
        assert_eq!(result.unwrap().1, Some(error.clone()));
        assert_eq!(calls, vec![("handle", Some(error))]);
        let (result, calls) = run(
            I::Json {
                type_code: 12,
                value: b"\x0312\xff",
            },
            false,
        );
        let (value, error) = result.unwrap();
        assert_eq!(text(&value), "12");
        assert_eq!(error, Some(E::Truncated));
        assert_eq!(calls, vec![("handle", Some(E::Truncated))]);
        let (result, calls) = run(
            I::Json {
                type_code: 4,
                value: &[0],
            },
            false,
        );
        let error = E::TruncatedWrongValue {
            message: "Truncated incorrect DECIMAL value: 'null'".into(),
        };
        assert_eq!(result.unwrap().1, Some(error.clone()));
        assert_eq!(calls, vec![("handle", Some(error))]);
        // Invalid-width numeric getters return None; their fallback Display
        // suppresses decoding errors and emits empty text, not fmt::Error.
        for tag in [9, 10, 11] {
            let (result, calls) = run(
                I::Json {
                    type_code: tag,
                    value: &[],
                },
                false,
            );
            let (value, actual) = result.unwrap();
            let error = E::TruncatedWrongValue {
                message: "Truncated incorrect DECIMAL value: ''".into(),
            };
            assert_eq!(text(&value), "0");
            assert_eq!(actual, Some(error.clone()));
            assert_eq!(calls, vec![("handle", Some(error))]);
        }
        // Only the literal branch indexes an absent first payload byte.
        {
            let tag = 4;
            let calls = RefCell::new(Vec::new());
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                native_datum_to_decimal_with_context(
                    I::Json {
                        type_code: tag,
                        value: &[],
                    },
                    |error| {
                        calls.borrow_mut().push("handle");
                        error
                    },
                    |error| {
                        calls.borrow_mut().push("direct");
                        error
                    },
                )
            }));
            assert!(panic.is_err());
            assert!(calls.borrow().is_empty());
        }
    }
}
