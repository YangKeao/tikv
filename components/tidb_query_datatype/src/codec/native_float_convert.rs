// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native floating-point helpers and ProduceFloatWithSpecifiedTp. The wire
//! truncate_f64 uses a different rounding rule/domain and is not substituted.
use std::fmt;

use super::{
    mysql::Decimal,
    native_type_name::{NativeTypeNameCode, native_type_str},
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct NativeFloatOverflow;
impl fmt::Debug for NativeFloatOverflow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("FloatOverflow")
    }
}
impl fmt::Display for NativeFloatOverflow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DOUBLE value is out of range")
    }
}
impl std::error::Error for NativeFloatOverflow {}

pub fn native_round_float(value: f64) -> f64 {
    value.round_ties_even()
}
pub fn native_round(value: f64, decimal: i32) -> f64 {
    let shift = decimal_shift(decimal);
    let shifted = value * shift;
    if shifted.is_infinite() {
        return value;
    }
    let result = native_round_float(shifted) / shift;
    if result.is_nan() { 0.0 } else { result }
}
pub fn native_truncate(value: f64, decimal: i32) -> f64 {
    let shift = decimal_shift(decimal);
    let shifted = value * shift;
    if shifted.is_infinite() || shifted.is_nan() {
        return value;
    }
    if shift == 0.0 {
        return if value.is_nan() { value } else { 0.0 };
    }
    shifted.trunc() / shift
}
pub fn native_get_max_float(flen: i32, decimal: i32) -> f64 {
    decimal_shift(flen - decimal) - decimal_shift(-decimal)
}
pub fn native_truncate_float(
    mut value: f64,
    flen: i32,
    decimal: i32,
) -> Result<f64, (f64, NativeFloatOverflow)> {
    if value.is_nan() {
        return Err((0.0, NativeFloatOverflow));
    }
    let maximum = native_get_max_float(flen, decimal);
    if !value.is_infinite() {
        value = native_round(value, decimal);
    }
    if value > maximum {
        Err((maximum, NativeFloatOverflow))
    } else if value < -maximum {
        Err((-maximum, NativeFloatOverflow))
    } else {
        Ok(value)
    }
}
fn decimal_shift(decimal: i32) -> f64 {
    if decimal > 308 {
        f64::INFINITY
    } else if decimal < -323 {
        0.0
    } else {
        10_f64.powi(decimal)
    }
}

#[derive(Clone, Copy, Debug)]
enum FloatTargetDiagnosticKind {
    Constant {
        value: f64,
        code: NativeTypeNameCode,
    },
    Truncated,
}
/// An original diagnostic site, without eager formatting. Native adapters call
/// message only inside their existing Diagnostics.error construction closure.
#[derive(Clone, Copy, Debug)]
pub struct NativeFloatTargetDiagnostic {
    kind: FloatTargetDiagnosticKind,
}
impl NativeFloatTargetDiagnostic {
    pub fn message(&self) -> String {
        match self.kind {
            FloatTargetDiagnosticKind::Constant { value, code } => format!(
                "constant {} overflows {}",
                Decimal::native_format_float_g_shortest(value),
                native_type_str(code)
            ),
            FloatTargetDiagnosticKind::Truncated => "DOUBLE value is out of range in ''".into(),
        }
    }
    fn constant(value: f64, code: NativeTypeNameCode) -> Self {
        Self {
            kind: FloatTargetDiagnosticKind::Constant { value, code },
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct NativeFloatTargetValue {
    pub value: f64,
    /// Original event subject, not the diagnostic message. The caller retains
    /// only projection into its native event factory with the actual target.
    pub overflow: Option<String>,
}
pub fn native_produce_float(
    value: f64,
    code: NativeTypeNameCode,
    flen: i64,
    decimal: i64,
    unsigned: bool,
    mut report: impl FnMut(NativeFloatTargetDiagnostic),
) -> NativeFloatTargetValue {
    if value.is_nan() {
        report(NativeFloatTargetDiagnostic::constant(value, code));
        return NativeFloatTargetValue {
            value: 0.0,
            overflow: Some(value.to_string()),
        };
    }
    if value.is_infinite() {
        report(NativeFloatTargetDiagnostic::constant(value, code));
        return NativeFloatTargetValue {
            value,
            overflow: Some(value.to_string()),
        };
    }
    let mut value = value;
    let mut overflow = None;
    if flen != -1 && decimal != -1 {
        match native_truncate_float(value, flen as i32, decimal as i32) {
            Ok(produced) => value = produced,
            Err((produced, error)) => {
                value = produced;
                // This fixed error event is constructed at the original site,
                // before unsigned clipping or the truncation diagnostic.
                overflow = Some(error.to_string());
            }
        }
    }
    if unsigned && value < 0.0 {
        report(NativeFloatTargetDiagnostic::constant(value, code));
        return NativeFloatTargetValue {
            value: 0.0,
            overflow: Some(value.to_string()),
        };
    }
    if overflow.is_some() {
        report(NativeFloatTargetDiagnostic {
            kind: FloatTargetDiagnosticKind::Truncated,
        });
        return NativeFloatTargetValue { value, overflow };
    }
    if code == NativeTypeNameCode::Known(4)
        && !(-f64::from(f32::MAX)..=f64::from(f32::MAX)).contains(&value)
    {
        let source = value;
        report(NativeFloatTargetDiagnostic::constant(source, code));
        value = if value.is_sign_positive() {
            f64::from(f32::MAX)
        } else {
            -f64::from(f32::MAX)
        };
        overflow = Some(source.to_string());
    }
    NativeFloatTargetValue { value, overflow }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_float_foundation_keeps_even_rounding_bounds_nonfinite_and_signed_zero() {
        assert_eq!(format!("{NativeFloatOverflow:?}"), "FloatOverflow");
        assert_eq!(
            NativeFloatOverflow.to_string(),
            "DOUBLE value is out of range"
        );
        for (value, expected) in [(2.5, 2.0), (3.5, 4.0), (-2.5, -2.0), (-3.5, -4.0)] {
            assert_eq!(native_round_float(value), expected);
        }
        assert_eq!(native_round_float(-0.5).to_bits(), (-0.0f64).to_bits());
        assert_eq!(native_round(1.25, 1), 1.2);
        assert_eq!(native_truncate(-1.29, 1), -1.2);
        assert_eq!(native_get_max_float(3, 1), 99.9);
        assert_eq!(
            native_truncate_float(100.0, 3, 1),
            Err((99.9, NativeFloatOverflow))
        );
        assert_eq!(
            native_truncate_float(-100.0, 3, 1),
            Err((-99.9, NativeFloatOverflow))
        );
        assert_eq!(native_truncate_float(2.5, 2, 0), Ok(2.0));
        assert_eq!(
            native_truncate_float(f64::NAN, 3, 1),
            Err((0.0, NativeFloatOverflow))
        );
        assert_eq!(
            native_truncate_float(f64::INFINITY, 3, 1),
            Err((99.9, NativeFloatOverflow))
        );
        assert_eq!(
            native_truncate_float(f64::NEG_INFINITY, 3, 1),
            Err((-99.9, NativeFloatOverflow))
        );
        assert_eq!(native_round(f64::NAN, 0), 0.0);
        assert!(native_truncate(f64::NAN, 0).is_nan());
        assert_eq!(native_round(f64::INFINITY, 0), f64::INFINITY);
        assert_eq!(native_round(f64::INFINITY, -324), 0.0);
        assert_eq!(native_truncate(f64::INFINITY, -324), f64::INFINITY);
        assert_eq!(native_round(1.25, 309), 1.25);
        assert_eq!(native_truncate(1.25, 309), 1.25);
        assert_eq!(native_round(1.25, -324), 0.0);
        assert_eq!(native_truncate(-1.25, -324).to_bits(), 0.0f64.to_bits());
        assert_eq!(native_round(f64::MAX, 1), f64::MAX);
        assert_eq!(native_truncate(f64::MAX, 1), f64::MAX);
        assert_eq!(native_round(-0.0, 0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(native_truncate(-0.0, 0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(
            native_truncate_float(-0.0, 3, 1).unwrap().to_bits(),
            (-0.0f64).to_bits()
        );
    }
    #[test]
    fn native_float_target_preserves_precedence_lazy_diagnostics_names_and_event_subjects() {
        use NativeTypeNameCode::{Known, Unknown};
        let mut diagnostics = Vec::new();
        let value = native_produce_float(1e100, Known(4), -1, -1, false, |diagnostic| {
            diagnostics.push(diagnostic)
        });
        assert_eq!(value.value, f64::from(f32::MAX));
        assert_eq!(value.overflow, Some(format!("1{}", "0".repeat(100))));
        // The report carried raw data and did not demand the Go-formatted
        // message; formatting here is explicitly later than event production.
        let FloatTargetDiagnosticKind::Constant {
            value: source,
            code,
        } = diagnostics[0].kind
        else {
            panic!("constant diagnostic carries the source");
        };
        assert_eq!(source, 1e100);
        assert_eq!(code, Known(4));
        assert_eq!(diagnostics[0].message(), "constant 1e+100 overflows float");
        for code in [Known(5), Unknown(4)] {
            let value =
                native_produce_float(1e100, code, -1, -1, false, |_| panic!("not a FLOAT range"));
            assert_eq!(
                value,
                NativeFloatTargetValue {
                    value: 1e100,
                    overflow: None
                }
            );
        }
        for (source, expected, subject, message) in [
            (f64::NAN, 0.0, "NaN", "constant NaN overflows "),
            (
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
                "-inf",
                "constant -Inf overflows ",
            ),
        ] {
            let mut messages = Vec::new();
            let value =
                native_produce_float(source, Unknown(4), i32::MIN as i64, 1, true, |diagnostic| {
                    messages.push(diagnostic.message())
                });
            assert_eq!(
                value,
                NativeFloatTargetValue {
                    value: expected,
                    overflow: Some(subject.into())
                }
            );
            assert_eq!(messages, [message]);
        }
        let mut messages = Vec::new();
        let value = native_produce_float(-100.0, Known(5), 3, 1, true, |diagnostic| {
            messages.push(diagnostic.message())
        });
        assert_eq!(
            value,
            NativeFloatTargetValue {
                value: 0.0,
                overflow: Some("-99.9".into())
            }
        );
        assert_eq!(messages, ["constant -99.9 overflows double"]);
        let mut messages = Vec::new();
        let value = native_produce_float(1e300, Known(4), 100, 0, false, |diagnostic| {
            messages.push(diagnostic.message())
        });
        // Retain the native powi-derived bound exactly, without assuming powi
        // rounds identically to a decimal literal at this large exponent.
        assert_eq!(value.value, native_get_max_float(100, 0));
        assert!(value.value > f64::from(f32::MAX));
        assert_eq!(value.overflow, Some("DOUBLE value is out of range".into()));
        assert_eq!(messages, ["DOUBLE value is out of range in ''"]);
        // Metadata is tested for exactly -1 before the original i64->i32 casts.
        let value = native_produce_float(
            1.25,
            Known(5),
            (1i64 << 32) + 3,
            (1i64 << 32) + 1,
            false,
            |_| panic!("exact rounded result"),
        );
        assert_eq!(
            value,
            NativeFloatTargetValue {
                value: 1.2,
                overflow: None
            }
        );
        let value = native_produce_float(1.25, Known(5), -1, 1, false, |_| {
            panic!("unspecified width")
        });
        assert_eq!(
            value,
            NativeFloatTargetValue {
                value: 1.25,
                overflow: None
            }
        );
        let value = native_produce_float(1.25, Known(5), 3, -1, false, |_| {
            panic!("unspecified scale")
        });
        assert_eq!(
            value,
            NativeFloatTargetValue {
                value: 1.25,
                overflow: None
            }
        );
        let value = native_produce_float(-0.0, Known(4), -1, -1, true, |_| {
            panic!("negative zero is not negative")
        });
        assert_eq!(value.value.to_bits(), (-0.0f64).to_bits());
        assert_eq!(value.overflow, None);
        let called = std::cell::Cell::new(false);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            native_produce_float(1e100, Known(4), -1, -1, false, |_| {
                called.set(true);
                panic!("original diagnostic effect");
            })
        }));
        assert!(panic.is_err());
        assert!(called.get());
    }
}
