// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native byte-prefix float conversion. The wire character scanner has a
//! different bare-exponent/NUL policy and is deliberately not used here.

/// The actual accepted prefix and truncation disposition, borrowing the input
/// (or static "0"). Scanning does not trim; conversion owns that separate step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFloatPrefix<'a> {
    pub value: &'a str,
    pub truncated: bool,
}

/// The best-effort float and final conversion disposition. Ordered diagnostic
/// calls are separate: prefix and range errors can both occur for this value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NativeFloatConversion {
    pub value: f64,
    pub truncated: bool,
}

/// Requests to the caller's existing diagnostic policy. Numeric input is the
/// original trimmed FULL subject, not the separate NUL-cut/capped warning text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeFloatDiagnostic<'a> {
    TruncatedNumericInput(&'a str),
    UnhandledTruncated,
}

/// Native getValidFloatPrefix byte scanner, without context or trimming.
pub fn native_valid_float_prefix(input: &str, is_function_cast: bool) -> NativeFloatPrefix<'_> {
    if is_function_cast && input.is_empty() {
        return NativeFloatPrefix {
            value: "0",
            truncated: false,
        };
    }
    let bytes = input.as_bytes();
    let mut saw_dot = false;
    let mut saw_digit = false;
    let mut valid_len = 0;
    let mut exponent_index: Option<usize> = None;
    let mut effective_len = bytes.len();
    for (index, byte) in bytes.iter().copied().enumerate() {
        match byte {
            b'+' | b'-'
                if index == 0 || exponent_index.is_some_and(|exponent| index == exponent + 1) => {}
            b'+' | b'-' => break,
            b'.' if saw_dot || exponent_index.is_some_and(|exponent| exponent > 0) => break,
            b'.' => {
                saw_dot = true;
                if saw_digit {
                    valid_len = index + 1;
                }
            }
            b'e' | b'E' if !saw_digit || exponent_index.is_some() => break,
            b'e' | b'E' => {
                exponent_index = Some(index);
                if index + 1 == bytes.len() {
                    return NativeFloatPrefix {
                        value: &input[..index],
                        truncated: false,
                    };
                }
            }
            0 => {
                effective_len = valid_len;
                break;
            }
            b'0'..=b'9' => {
                saw_digit = true;
                valid_len = index + 1;
            }
            _ => break,
        }
    }
    NativeFloatPrefix {
        value: if valid_len == 0 {
            "0"
        } else {
            &input[..valid_len]
        },
        truncated: valid_len == 0 || valid_len != effective_len,
    }
}

/// Native StrToFloat with synchronous, ordered diagnostic requests. No fake
/// context or SQL mode is created; the sink retains its own error precedence,
/// warning retention and ignore policy. A prefix diagnostic occurs before the
/// Rust float parse, and a range diagnostic remains independent of it.
pub fn native_str_to_float_reported<'a>(
    input: &'a str,
    is_function_cast: bool,
    mut report: impl FnMut(NativeFloatDiagnostic<'a>),
) -> NativeFloatConversion {
    let input = input.trim();
    let prefix = native_valid_float_prefix(input, is_function_cast);
    if prefix.truncated {
        report(NativeFloatDiagnostic::TruncatedNumericInput(input));
    }
    match prefix.value.parse::<f64>() {
        Ok(value) if value.is_infinite() => {
            report(NativeFloatDiagnostic::TruncatedNumericInput(input));
            NativeFloatConversion {
                value: if value.is_sign_positive() {
                    f64::MAX
                } else {
                    -f64::MAX
                },
                truncated: true,
            }
        }
        Ok(value) => NativeFloatConversion {
            value,
            truncated: prefix.truncated,
        },
        Err(_) => {
            report(NativeFloatDiagnostic::UnhandledTruncated);
            NativeFloatConversion {
                value: 0.0,
                truncated: true,
            }
        }
    }
}

/// Value and final event only. This is not the reported diagnostic channel:
/// expression callers may deliberately map one final event to one SQL warning.
pub fn native_str_to_float(input: &str, is_function_cast: bool) -> NativeFloatConversion {
    native_str_to_float_reported(input, is_function_cast, |_| {})
}

/// The native ordinary warning subject: Unicode trim, first NUL, then the
/// existing UTF-8-boundary-safe 128-byte cap. Reported datatype diagnostics use
/// the uncapped trimmed input instead and must not silently select this helper.
pub fn native_float_warning_input(input: &str) -> &str {
    let nul_cut = input.trim().split('\0').next().unwrap_or_default();
    super::convert::native_warning_subject_byte_cap(nul_cut)
}

#[cfg(test)]
#[test]
fn native_float_parse_keeps_byte_prefix_and_reported_diagnostics_separate() {
    for (input, cast, expected, truncated) in [
        ("5e", false, "5", false),
        ("5E", false, "5", false),
        ("5e+", false, "5", true),
        ("5e\0tail", false, "5", false),
        ("5e+\0tail", false, "5", false),
        ("1e5e", false, "1e5", true),
        ("\0tail", false, "0", true),
        (" 12", false, "0", true),
        ("12é", false, "12", true),
        ("", false, "0", true),
        ("", true, "0", false),
    ] {
        assert_eq!(
            native_valid_float_prefix(input, cast),
            NativeFloatPrefix {
                value: expected,
                truncated
            },
            "{input:?}"
        );
    }
    for (input, cast, expected, truncated, count) in [
        (" \t5e\u{2003}", true, 5.0, false, 0),
        ("12\0tail", false, 12.0, false, 0),
        ("-1e999", false, -f64::MAX, true, 1),
        ("1e999x", false, f64::MAX, true, 2),
        ("-1e-9999", false, -0.0, false, 0),
        ("", true, 0.0, false, 0),
        ("", false, 0.0, true, 1),
    ] {
        let mut diagnostics = Vec::new();
        let actual = native_str_to_float_reported(input, cast, |event| diagnostics.push(event));
        assert_eq!(actual.value.to_bits(), expected.to_bits(), "{input:?}");
        assert_eq!(actual.truncated, truncated, "{input:?}");
        assert_eq!(
            diagnostics,
            vec![NativeFloatDiagnostic::TruncatedNumericInput(input.trim()); count],
            "{input:?}"
        );
        let plain = native_str_to_float(input, cast);
        assert_eq!(plain.value.to_bits(), expected.to_bits());
        assert_eq!(plain.truncated, truncated);
    }
    let full_subject = format!("1e999{}\0tail", "x".repeat(130));
    let input = format!(" \t{full_subject}\u{2003}");
    let mut diagnostics = Vec::new();
    let output = native_str_to_float_reported(&input, true, |event| diagnostics.push(event));
    assert_eq!(output.value, f64::MAX);
    assert!(output.truncated);
    assert_eq!(
        diagnostics,
        vec![NativeFloatDiagnostic::TruncatedNumericInput(&full_subject); 2]
    );
    assert_eq!(native_float_warning_input(&input), &full_subject[..128]);
    assert_eq!(native_float_warning_input(" 12\0tail "), "12");
    let boundary = format!("{}é", "a".repeat(127));
    assert_eq!(native_float_warning_input(&boundary), &boundary[..127]);
}
