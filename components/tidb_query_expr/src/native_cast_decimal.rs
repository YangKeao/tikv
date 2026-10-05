// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Ordinary CAST AS DECIMAL control, not the other CAST signatures or wire
//! policy. The original warning sink and default-context datum conversion are
//! callbacks; source selection, warning decisions and conversion error/event
//! folding belong here. No comparison/evaluation facade is added or bypassed.
use tidb_query_datatype::codec::{
    convert::native_warning_subject_byte_cap,
    mysql::{
        Decimal, NativeDecimalCmpParts, NativeDecimalParseError, NativeDecimalParseRef,
        NativeDecimalParseValue, native_decimal_cmp, native_decimal_from_literal,
        native_decimal_normalize, native_decimal_parse_mysql,
    },
};

/// Actual source values. `Other` retains the original datum in the conversion
/// callback; Float32 deliberately follows that same default-context service,
/// rather than the distinct ordinary Real/Rust-Display path.
/// The caller's existing NULL/range-sentinel guards precede this branch.
#[derive(Clone, Copy, Debug)]
pub enum NativeCastDecimalInput<'a> {
    Decimal(NativeDecimalParseRef<'a>),
    Int(i64),
    UInt(u64),
    Real(f64),
    String(&'a [u8]),
    Bytes(&'a [u8]),
    Float32(f64),
    Other,
}

/// The existing UNION warning-only helper and ordinary CAST share this policy.
/// Unicode trimming here is intentionally NOT the source conversion's parser
/// input. A warning is appended synchronously before any source conversion.
pub fn native_cast_decimal_input_warning(
    input: NativeCastDecimalInput<'_>,
    mut append_warning: impl FnMut(u16, &str),
) {
    let text = match input {
        NativeCastDecimalInput::String(bytes) | NativeCastDecimalInput::Bytes(bytes) => {
            std::str::from_utf8(bytes).ok()
        }
        _ => None,
    };
    let Some(text) = text else {
        return;
    };
    let trimmed = text.trim();
    let (_, disposition) = native_decimal_parse_mysql(trimmed, 9);
    match disposition {
        Some(NativeDecimalParseError::Overflow) => {
            append_warning(1690, "%s value is out of range in '%s'")
        }
        Some(
            NativeDecimalParseError::Truncated
            | NativeDecimalParseError::BadNumber
            | NativeDecimalParseError::TruncatedWrongValue,
        ) => {
            append_warning(
                1292,
                &format!(
                    "Truncated incorrect DECIMAL value: '{}'",
                    native_warning_subject_byte_cap(trimmed)
                ),
            );
        }
        None => {}
    }
}

/// Existing ordinary Decimal target behavior, including unspecified-scale raw
/// identity, flen-zero rounding, and overflow-before-truncation warning
/// priority. `W` is the real default conversion event, intentionally ignored by
/// this particular signature; neither the adapter nor a precomputed answer
/// folds it.
pub fn native_cast_decimal<E, W>(
    input: NativeCastDecimalInput<'_>,
    flen: u32,
    scale: u32,
    convert_other: impl FnOnce() -> Result<(NativeDecimalParseValue, W), E>,
    mut append_warning: impl FnMut(u16, &str),
) -> NativeDecimalParseValue {
    native_cast_decimal_input_warning(input, &mut append_warning);
    let source = match input {
        NativeCastDecimalInput::Decimal(value) => value.copy_raw(),
        NativeCastDecimalInput::Int(value) => NativeDecimalParseValue::from_int(value),
        NativeCastDecimalInput::UInt(value) => NativeDecimalParseValue::from_uint(value),
        NativeCastDecimalInput::Real(value) => decimal_prefix(&value.to_string()),
        NativeCastDecimalInput::String(bytes) | NativeCastDecimalInput::Bytes(bytes) => {
            std::str::from_utf8(bytes)
                .map(|text| native_decimal_parse_mysql(text, 9).0)
                .unwrap_or_else(|_| NativeDecimalParseValue::from_int(0))
        }
        NativeCastDecimalInput::Float32(_) | NativeCastDecimalInput::Other => match convert_other()
        {
            Ok((value, event)) => {
                drop(event);
                value
            }
            Err(_) => NativeDecimalParseValue::from_int(0),
        },
    };
    if scale == u32::MAX {
        return source;
    }
    let produced = source.cast_to_precision(flen, scale);
    if flen != 0 {
        let rounded = source.round_to_scale(scale as i32);
        let rounded = rounded.as_ref();
        let int_digits = digits(rounded).len() as u32 - rounded.storage_scale;
        if int_digits > flen.saturating_sub(scale) {
            append_warning(
                1690,
                &format!("DECIMAL value is out of range in '({flen}, {scale})'"),
            );
        } else {
            let source = source.as_ref();
            if source.storage_scale > scale
                && native_decimal_cmp(cmp_parts(produced.as_ref()), cmp_parts(source)).is_ne()
            {
                let text = Decimal::native_format_visible(
                    source.negative,
                    source.digits,
                    source.scale,
                    source.storage_scale,
                );
                append_warning(
                    1292,
                    &format!("Truncated incorrect DECIMAL value: '{text}'"),
                );
            }
        }
    }
    produced
}
fn digits(value: NativeDecimalParseRef<'_>) -> &str {
    std::str::from_utf8(value.digits).expect("decimal coefficients are ASCII digits")
}
fn cmp_parts(value: NativeDecimalParseRef<'_>) -> NativeDecimalCmpParts<'_> {
    NativeDecimalCmpParts {
        negative: value.negative,
        digits: digits(value),
        storage_scale: value.storage_scale,
    }
}

/// Source numeric-prefix algorithm shared by ordinary Real-to-Decimal and
/// existing string/bytes-to-f64 consumers. Do not substitute the MySQL text
/// parser, Go-shortest float formatter, or Float32 datum conversion.
pub fn decimal_prefix(s: &str) -> NativeDecimalParseValue {
    let s = s.trim_start();
    let (negative, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let int_digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let after_int = &rest[int_digits.len()..];
    let (frac_digits, after_frac) = match after_int.strip_prefix('.') {
        Some(r) => {
            let f: String = r.chars().take_while(char::is_ascii_digit).collect();
            let len = f.len();
            (f, &r[len..])
        }
        None => (String::new(), after_int),
    };
    if int_digits.is_empty() && frac_digits.is_empty() {
        return NativeDecimalParseValue::from_int(0);
    }
    let base = if int_digits.is_empty() {
        format!("0.{frac_digits}")
    } else if frac_digits.is_empty() {
        int_digits.clone()
    } else {
        format!("{int_digits}.{frac_digits}")
    };
    let exponent = exponent_prefix(after_frac);
    if exponent != 0 {
        let base_f: f64 = base.parse().unwrap_or(0.0);
        let sign = if negative { -1.0 } else { 1.0 };
        let scaled = sign * base_f * 10f64.powi(exponent);
        return decimal_prefix(&scaled.to_string());
    }
    let value = native_decimal_from_literal(&base);
    if negative {
        // The base is already canonical, positive and unshaped. Reuse the
        // native constructor's sign/zero normalization for precisely that domain.
        let (_, digits, scale, storage_scale, _) = value.into_raw_parts();
        native_decimal_normalize(true, digits, scale, storage_scale, false)
    } else {
        value
    }
}
fn exponent_prefix(s: &str) -> i32 {
    let Some(rest) = s.strip_prefix(['e', 'E']) else {
        return 0;
    };
    let (negative, rest) = match rest.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, rest.strip_prefix('+').unwrap_or(rest)),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return 0;
    }
    let mag: i32 = digits.parse().unwrap_or(0);
    if negative { -mag } else { mag }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    fn visible(value: &NativeDecimalParseValue) -> String {
        let value = value.as_ref();
        Decimal::native_format_visible(
            value.negative,
            value.digits,
            value.scale,
            value.storage_scale,
        )
    }
    fn cast(
        input: NativeCastDecimalInput<'_>,
        flen: u32,
        scale: u32,
    ) -> (NativeDecimalParseValue, Vec<(u16, String)>) {
        let mut warnings = Vec::new();
        let value = native_cast_decimal::<(), ()>(
            input,
            flen,
            scale,
            || panic!("undemanded default conversion"),
            |code, message| warnings.push((code, message.to_owned())),
        );
        (value, warnings)
    }
    #[test]
    fn ordinary_decimal_cast_preserves_source_warning_lifecycle_and_raw_identity() {
        use NativeCastDecimalInput as I;
        let (value, warnings) = cast(I::String(b" \t1.239x "), 10, 2);
        assert_eq!(visible(&value), "1.24");
        assert_eq!(
            warnings,
            [
                (
                    1292,
                    "Truncated incorrect DECIMAL value: '1.239x'".to_owned()
                ),
                (
                    1292,
                    "Truncated incorrect DECIMAL value: '1.239'".to_owned()
                )
            ]
        );
        // The first parser sees Unicode trim, the second the original bytes.
        let (value, warnings) = cast(I::Bytes("\u{2003}1.239\u{2003}".as_bytes()), 10, 2);
        assert_eq!(visible(&value), "0.00");
        assert!(warnings.is_empty());
        let (value, warnings) = cast(I::String(&[0xff, b'1']), 10, 2);
        assert_eq!(visible(&value), "0.00");
        assert!(warnings.is_empty());
        let (value, warnings) = cast(I::String(b"1234.56"), 4, 1);
        assert_eq!(visible(&value), "999.9");
        assert_eq!(
            warnings,
            [(1690, "DECIMAL value is out of range in '(4, 1)'".to_owned())]
        );
        let (value, warnings) = cast(I::Bytes(b"1e300"), 4, 1);
        assert_eq!(visible(&value), "999.9");
        assert_eq!(
            warnings,
            [
                (1690, "%s value is out of range in '%s'".to_owned()),
                (1690, "DECIMAL value is out of range in '(4, 1)'".to_owned())
            ]
        );
        let hidden = NativeDecimalParseRef {
            negative: false,
            digits: b"12345",
            scale: 2,
            storage_scale: 4,
            declared_shape: Some((10, 2)),
        };
        let (value, warnings) = cast(I::Decimal(hidden), 10, 2);
        assert_eq!(visible(&value), "1.23");
        assert_eq!(
            warnings,
            [(1292, "Truncated incorrect DECIMAL value: '1.23'".to_owned())]
        );
        assert_eq!(value.as_ref().declared_shape, None);
        let (value, warnings) = cast(I::Decimal(hidden), 0, 2);
        assert_eq!(visible(&value), "1.23");
        assert!(warnings.is_empty());
        assert_eq!(value.as_ref().storage_scale, 2);
        let raw = NativeDecimalParseRef {
            negative: true,
            digits: &[0xff, 0],
            scale: 7,
            storage_scale: 1,
            declared_shape: Some((-4, 99)),
        };
        let (value, warnings) = cast(I::Decimal(raw), 8, u32::MAX);
        let value = value.as_ref();
        assert_eq!(
            (
                value.negative,
                value.digits,
                value.scale,
                value.storage_scale,
                value.declared_shape
            ),
            (
                raw.negative,
                raw.digits,
                raw.scale,
                raw.storage_scale,
                raw.declared_shape
            )
        );
        assert!(warnings.is_empty());
        let (_, warnings) = cast(I::String(b"1x"), 10, u32::MAX);
        assert_eq!(warnings.len(), 1); // input warning still precedes the bypass
        let text = format!("1{}", "é".repeat(64));
        let mut warnings = Vec::new();
        native_cast_decimal_input_warning(I::String(text.as_bytes()), |code, message| {
            warnings.push((code, message.to_owned()))
        });
        assert_eq!(
            warnings,
            [(
                1292,
                format!("Truncated incorrect DECIMAL value: '1{}'", "é".repeat(63))
            )]
        );

        struct Event<'a>(&'a RefCell<Vec<&'static str>>);
        impl Drop for Event<'_> {
            fn drop(&mut self) {
                self.0.borrow_mut().push("discard event");
            }
        }
        let order = RefCell::new(Vec::new());
        let value = native_cast_decimal::<(), _>(
            I::Other,
            10,
            2,
            || {
                order.borrow_mut().push("convert");
                Ok((native_decimal_from_literal("1.234"), Event(&order)))
            },
            |code, _| {
                assert_eq!(code, 1292);
                order.borrow_mut().push("production warning");
            },
        );
        assert_eq!(visible(&value), "1.23");
        assert_eq!(
            *order.borrow(),
            ["convert", "discard event", "production warning"]
        );
        let value = native_cast_decimal::<&str, ()>(
            I::Float32(f64::NAN),
            10,
            2,
            || Err("actual default-context conversion error"),
            |_, _| panic!("quiet folded error"),
        );
        assert_eq!(visible(&value), "0.00");
        let value = native_cast_decimal::<(), ()>(
            I::Float32(0.1),
            0,
            u32::MAX,
            || Ok((native_decimal_from_literal("7.50"), ())),
            |_, _| unreachable!(),
        );
        assert_eq!(visible(&value), "7.50");
    }
    #[test]
    fn real_prefix_remains_rust_display_with_original_exponent_and_zero_policy() {
        use NativeCastDecimalInput as I;
        for (text, expected) in [
            ("  -000.50tail", "-0.50"),
            ("+.5", "0.5"),
            ("-0", "0"),
            ("3.5e1rest", "35"),
            ("1e-2", "0.01"),
            ("1e2147483648", "1"),
            ("1e-2147483648", "1"),
            ("NaN", "0"),
            ("-inf", "0"),
        ] {
            assert_eq!(visible(&decimal_prefix(text)), expected, "{text}");
        }
        assert_eq!(exponent_prefix("e+3x"), 3);
        assert_eq!(exponent_prefix("E-3x"), -3);
        assert_eq!(exponent_prefix("e-"), 0);
        for (value, expected) in [
            (0.1, "0.1"),
            (1.25, "1.25"),
            (-1.25, "-1.25"),
            (-0.0, "0"),
            (f64::NAN, "0"),
            (f64::INFINITY, "0"),
            (f64::NEG_INFINITY, "0"),
        ] {
            let (actual, warnings) = cast(I::Real(value), 0, u32::MAX);
            assert_eq!(visible(&actual), expected);
            assert!(warnings.is_empty());
        }
        let (negative_zero, _) = cast(I::Real(-0.0), 0, u32::MAX);
        assert!(!negative_zero.as_ref().negative);
        let (minimum, _) = cast(I::Int(i64::MIN), 0, u32::MAX);
        assert_eq!(visible(&minimum), i64::MIN.to_string());
        let (maximum, _) = cast(I::UInt(u64::MAX), 0, u32::MAX);
        assert_eq!(visible(&maximum), u64::MAX.to_string());
    }
}
