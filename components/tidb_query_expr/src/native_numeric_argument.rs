// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Numeric-argument source parsing and original conversion-stage effects.
//! Target metadata/fitting remains separate. In particular, vectorized decimal
//! truncation and scalar named-value truncation demand different context calls.
use std::fmt;

use tidb_query_datatype::codec::{
    mysql::{
        NativeDecimalParseValue, NativeMyDecimal, NativeMyDecimalError,
        json::{native_binary_json_string_bytes, write_native_binary_json_text},
        native_decimal_from_my_decimal,
    },
    native_float_parse::{native_float_warning_input, native_str_to_float},
    native_scalar_convert::native_json_to_float,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeNumericArgumentLevel {
    Error,
    Warn,
    Ignore,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeNumericArgumentConversionError {
    Truncated,
    Overflow,
    BadNumber,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeNumericArgumentError<E> {
    Effect(E),
    Conversion(NativeNumericArgumentConversionError),
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
/// Lossy bytes use the function-cast float parser (including its silent empty
/// input). Only the final event requests the original DOUBLE diagnostic effect.
pub fn native_numeric_argument_bytes_to_f64<E>(
    bytes: &[u8],
    mut handle: impl FnMut(&str) -> Result<(), E>,
) -> Result<f64, E> {
    let text = String::from_utf8_lossy(bytes);
    let converted = native_str_to_float(&text, true);
    if converted.truncated {
        handle(&format!(
            "Truncated incorrect DOUBLE value: '{}'",
            native_float_warning_input(&text)
        ))?;
    }
    Ok(converted.value)
}
/// JSON conversion uses its numeric accessor, not Display. Display is demanded
/// only for a non-string truncation diagnostic; string diagnostics remain
/// DOUBLE and use the original raw bytes lossily, even when conversion
/// discarded them.
pub fn native_numeric_argument_json_to_f64<E>(
    type_code: u8,
    value: &[u8],
    mut handle: impl FnMut(&str) -> Result<(), E>,
) -> Result<f64, E> {
    let converted = native_json_to_float(type_code, value);
    if converted.truncated {
        if let Some(bytes) = native_binary_json_string_bytes(type_code, value) {
            let text = String::from_utf8_lossy(bytes);
            handle(&format!(
                "Truncated incorrect DOUBLE value: '{}'",
                native_float_warning_input(&text)
            ))?;
        } else {
            handle(&format!(
                "Truncated incorrect FLOAT value: '{}'",
                JsonDisplay { type_code, value }
            ))?;
        }
    }
    Ok(converted.value)
}
/// Parse after lossy decoding and Unicode trim. Effects precede the native
/// value projection, as in the source; Error/veto never constructs that value.
/// The level callback is demanded only by the raw-error branch, and append only
/// at Warn. Scalar Truncated and every TruncatedWrongValue use handle instead.
pub fn native_numeric_argument_string_to_decimal<E>(
    bytes: &[u8],
    vectorized: bool,
    mut level: impl FnMut() -> NativeNumericArgumentLevel,
    mut handle: impl FnMut(&str) -> Result<(), E>,
    mut append: impl FnMut(NativeNumericArgumentConversionError),
) -> Result<NativeDecimalParseValue, NativeNumericArgumentError<E>> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    let (decimal, error) = NativeMyDecimal::from_string(text.as_bytes());
    if let Some(error) = error {
        if error == NativeMyDecimalError::TruncatedWrongValue
            || (error == NativeMyDecimalError::Truncated && !vectorized)
        {
            handle(&format!("Truncated incorrect DECIMAL value: '{text}'"))
                .map_err(NativeNumericArgumentError::Effect)?;
        } else {
            let error = match error {
                NativeMyDecimalError::Truncated => NativeNumericArgumentConversionError::Truncated,
                NativeMyDecimalError::Overflow => NativeNumericArgumentConversionError::Overflow,
                _ => NativeNumericArgumentConversionError::BadNumber,
            };
            match level() {
                NativeNumericArgumentLevel::Error => {
                    return Err(NativeNumericArgumentError::Conversion(error));
                }
                NativeNumericArgumentLevel::Warn => append(error),
                NativeNumericArgumentLevel::Ignore => {}
            }
        }
    }
    Ok(native_decimal_from_my_decimal(decimal))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use NativeNumericArgumentConversionError as C;
    use NativeNumericArgumentError as E;
    use NativeNumericArgumentLevel as L;

    use super::*;
    fn decimal(
        bytes: &[u8],
        vectorized: bool,
        level: L,
        reject: bool,
    ) -> (
        Result<NativeDecimalParseValue, E<&'static str>>,
        Vec<String>,
    ) {
        let calls = RefCell::new(Vec::new());
        let value = native_numeric_argument_string_to_decimal(
            bytes,
            vectorized,
            || {
                calls.borrow_mut().push("level".into());
                level
            },
            |message| {
                calls.borrow_mut().push(format!("handle:{message}"));
                if reject { Err("veto") } else { Ok(()) }
            },
            |error| calls.borrow_mut().push(format!("append:{error:?}")),
        );
        (value, calls.into_inner())
    }
    fn visible(value: &NativeDecimalParseValue) -> String {
        let value = value.as_ref();
        tidb_query_datatype::codec::mysql::Decimal::native_format_visible(
            value.negative,
            value.digits,
            value.scale,
            value.storage_scale,
        )
    }
    #[test]
    fn decimal_argument_effects_preserve_scalar_vector_level_order_and_veto() {
        let (value, calls) = decimal(b"12.50", false, L::Error, true);
        assert_eq!(visible(&value.unwrap()), "12.50");
        assert!(calls.is_empty());
        let (value, calls) = decimal(b"1x", false, L::Error, false);
        assert_eq!(visible(&value.unwrap()), "1");
        assert_eq!(calls, ["handle:Truncated incorrect DECIMAL value: '1x'"]);
        let (value, calls) = decimal(b"1x", false, L::Ignore, true);
        assert_eq!(value.unwrap_err(), E::Effect("veto"));
        assert_eq!(calls, ["handle:Truncated incorrect DECIMAL value: '1x'"]);
        for level in [L::Error, L::Warn, L::Ignore] {
            let (value, calls) = decimal(b"1x", true, level, true);
            match level {
                L::Error => {
                    assert_eq!(value.unwrap_err(), E::Conversion(C::Truncated));
                    assert_eq!(calls, ["level"]);
                }
                L::Warn => {
                    assert_eq!(visible(&value.unwrap()), "1");
                    assert_eq!(calls, ["level", "append:Truncated"]);
                }
                L::Ignore => {
                    assert_eq!(visible(&value.unwrap()), "1");
                    assert_eq!(calls, ["level"]);
                }
            }
        }
        for vectorized in [false, true] {
            for (bytes, subject, expected) in [
                (&b""[..], "", "0"),
                (&b" \t-xyz "[..], "-xyz", "0"),
                ("\u{2003}+12x\u{2003}".as_bytes(), "+12x", "12"),
                (&b"12\xff"[..], "12�", "12"),
            ] {
                // The latter two are raw Truncated, so scalar only names them;
                // vector handling is covered by the explicit level cases above.
                if vectorized && expected == "12" {
                    continue;
                }
                let (value, calls) = decimal(bytes, vectorized, L::Error, false);
                assert_eq!(visible(&value.unwrap()), expected);
                assert_eq!(
                    calls,
                    vec![format!(
                        "handle:Truncated incorrect DECIMAL value: '{subject}'"
                    )]
                );
            }
            let (value, calls) = decimal(b"1e100", vectorized, L::Error, true);
            assert_eq!(value.unwrap_err(), E::Conversion(C::Overflow));
            assert_eq!(calls, ["level"]);
            let (value, calls) = decimal(b"1e100", vectorized, L::Warn, true);
            assert!(value.is_ok());
            assert_eq!(calls, ["level", "append:Overflow"]);
        }
        // Decimal diagnostic subjects are neither NUL-cut nor 128-byte capped.
        let subject = format!("{}\0tail", "x".repeat(140));
        let (value, calls) = decimal(subject.as_bytes(), false, L::Error, false);
        assert_eq!(visible(&value.unwrap()), "0");
        assert_eq!(
            calls,
            vec![format!(
                "handle:Truncated incorrect DECIMAL value: '{subject}'"
            )]
        );
    }
    #[test]
    fn byte_real_arguments_keep_lossy_prefix_empty_nul_cap_and_final_event_only() {
        // At NUL the scanner sets effective_len = valid_len. A nonempty
        // finite prefix therefore has no final truncation event (lines 66–83
        // of native_float_parse), so diagnostic NUL-cutting is not demanded.
        for (bytes, expected) in [
            (&b""[..], 0.0),
            (&b" \t"[..], 0.0),
            (&b"12"[..], 12.0),
            (&b" 12\0tail "[..], 12.0),
        ] {
            assert_eq!(
                native_numeric_argument_bytes_to_f64::<()>(bytes, |_| panic!("undemanded warning")),
                Ok(expected)
            );
        }
        for (bytes, expected, subject) in [
            (&b"12\xff"[..], 12.0, "12�"),
            // A bad byte before NUL terminates scanning first and truncates;
            // only then does the warning formatter cut the diagnostic at NUL.
            (&b" 12x\0tail "[..], 12.0, "12x"),
            (&b"abc"[..], 0.0, "abc"),
            (&b"1e400x"[..], f64::MAX, "1e400x"),
        ] {
            let mut calls = Vec::new();
            let value = native_numeric_argument_bytes_to_f64::<()>(bytes, |message| {
                calls.push(message.to_owned());
                Ok(())
            });
            assert_eq!(value, Ok(expected));
            assert_eq!(
                calls,
                vec![format!("Truncated incorrect DOUBLE value: '{subject}'")]
            );
        }
        let text = format!(" \t{}\0tail ", "界".repeat(50));
        let mut calls = Vec::new();
        let value = native_numeric_argument_bytes_to_f64(text.as_bytes(), |message| {
            calls.push(message.to_owned());
            Err("veto")
        });
        assert_eq!(value, Err("veto"));
        assert_eq!(
            calls,
            vec![format!(
                "Truncated incorrect DOUBLE value: '{}'",
                "界".repeat(42)
            )]
        );
    }
    #[test]
    fn json_real_arguments_keep_numeric_value_string_diagnostic_and_display_demand_separate() {
        for (tag, value, expected, warning) in [
            (
                12,
                &b"\x0312\xff"[..],
                0.0,
                Some("Truncated incorrect DOUBLE value: '12�'"),
            ),
            (
                12,
                &b"\x00"[..],
                0.0,
                Some("Truncated incorrect DOUBLE value: ''"),
            ),
            (12, &b"\x032.5"[..], 2.5, None),
            (4, &[2][..], 0.0, None),
            (4, &[255][..], 1.0, None),
            (
                4,
                &[0][..],
                0.0,
                Some("Truncated incorrect FLOAT value: 'null'"),
            ),
            (4, &[][..], 0.0, Some("Truncated incorrect FLOAT value: ''")),
            (3, &[][..], 0.0, Some("Truncated incorrect FLOAT value: ''")),
        ] {
            let mut calls = Vec::new();
            let actual = native_numeric_argument_json_to_f64::<()>(tag, value, |message| {
                calls.push(message.to_owned());
                Ok(())
            });
            assert_eq!(actual, Ok(expected));
            assert_eq!(
                calls,
                warning.map(str::to_owned).into_iter().collect::<Vec<_>>()
            );
        }
        for value in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            let actual =
                native_numeric_argument_json_to_f64::<()>(11, &value.to_le_bytes(), |_| {
                    panic!("nonfinite numeric JSON has no truncation event")
                })
                .unwrap();
            assert_eq!(actual.to_bits(), value.to_bits());
        }
        let mut calls = Vec::new();
        let value = native_numeric_argument_json_to_f64(4, &[0], |message| {
            calls.push(message.to_owned());
            Err("veto")
        });
        assert_eq!(value, Err("veto"));
        assert_eq!(calls, ["Truncated incorrect FLOAT value: 'null'"]);
        let calls = RefCell::new(Vec::new());
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            native_numeric_argument_json_to_f64::<()>(9, &[], |message| {
                calls.borrow_mut().push(message.to_owned());
                Ok(())
            })
        }));
        assert!(panic.is_err());
        assert!(calls.borrow().is_empty());
    }
}
