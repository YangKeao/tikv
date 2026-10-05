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
    native_eval_type::{
        NativeEvalType, native_field_decimal_under_limit, native_field_flen_under_limit,
    },
    native_float_parse::{native_float_warning_input, native_str_to_float},
    native_numeric::NativeNumericInput,
    native_scalar_convert::native_json_to_float,
    native_type_name::NativeTypeNameCode,
};

/// Final target policy only. None leaves the native FieldType constructor's
/// defaults untouched; unspecified scale bypasses fitting only for DECIMAL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeNumericArgumentTarget {
    pub code: u8,
    pub decimal_shape: Option<(i64, i64)>,
    pub skip_fitting: bool,
}
/// Describe the selected target without materializing caller-owned FieldType
/// storage.
pub fn native_numeric_argument_target(
    source: NativeEvalType,
    source_code: NativeTypeNameCode,
    flen: i64,
    decimal: i64,
    target_code: u8,
) -> NativeNumericArgumentTarget {
    let decimal_shape = if target_code == 246 {
        Some(native_numeric_argument_decimal_shape(
            source,
            source_code,
            flen,
            decimal,
        ))
    } else {
        None
    };
    NativeNumericArgumentTarget {
        code: target_code,
        decimal_shape,
        skip_fitting: decimal_shape.is_some_and(|(_, scale)| scale < 0),
    }
}
/// The existing String branch selects Real explicitly; every other admitted
/// target, including Int, follows its original intermediate-Decimal path.
pub fn native_numeric_argument_string_is_real(target: NativeEvalType) -> bool {
    target == NativeEvalType::Real
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeNumericArgumentResultError<C> {
    Unsupported(&'static str),
    Conversion(C),
}
fn numeric_argument_result<T, C, E>(
    result: Result<(T, Option<C>), E>,
    unsupported: &'static str,
) -> Result<T, NativeNumericArgumentResultError<C>> {
    let (value, error) =
        result.map_err(|_| NativeNumericArgumentResultError::Unsupported(unsupported))?;
    match error {
        Some(error) => Err(NativeNumericArgumentResultError::Conversion(error)),
        None => Ok(value),
    }
}
/// Fold the real final conversion result without formatting/replacing its
/// typed conversion error or retaining an unsupported engine error.
pub fn native_numeric_argument_conversion_result<T, C, E>(
    result: Result<(T, Option<C>), E>,
) -> Result<T, NativeNumericArgumentResultError<C>> {
    numeric_argument_result(result, "numeric argument conversion failed")
}
/// The context-bearing decimal conversion has its own original failure subject
/// but the same value/optional-conversion-error disposition.
pub fn native_numeric_argument_context_decimal_result<T, C, E>(
    result: Result<(T, Option<C>), E>,
) -> Result<T, NativeNumericArgumentResultError<C>> {
    numeric_argument_result(result, "numeric decimal argument conversion failed")
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeNumericArgumentNormalization {
    Keep,
    UInt(u64),
    Real(f64),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeNumericArgumentRoute {
    String,
    JsonReal,
    JsonInt,
    ContextDecimal,
    RealDecimal,
    Fit,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeNumericArgumentHead {
    Preserve,
    Cast {
        normalization: NativeNumericArgumentNormalization,
        route: NativeNumericArgumentRoute,
        target_code: u8,
    },
}
/// Initial numeric-argument policy only. NULL/same-evaluation identity precedes
/// normalization and target validation. Routing inspects the actual normalized
/// value, and nonhybrid String admission precedes every JSON/decimal route.
pub fn native_numeric_argument_head(
    input: NativeNumericInput<'_>,
    source: NativeEvalType,
    target: NativeEvalType,
    unsigned: bool,
    hybrid: bool,
) -> Result<NativeNumericArgumentHead, &'static str> {
    use NativeNumericArgumentNormalization as N;
    use NativeNumericArgumentRoute as R;
    use NativeNumericInput as I;
    if matches!(input, I::Null) || source == target {
        return Ok(NativeNumericArgumentHead::Preserve);
    }
    let (normalization, input) = match input {
        I::Int(value) if unsigned => (N::UInt(value as u64), I::UInt(value as u64)),
        I::Float32(value) => {
            let value = f64::from(value as f32);
            (N::Real(value), I::Real(value))
        }
        _ => (N::Keep, input),
    };
    let target_code = match target {
        NativeEvalType::Int => 8,
        NativeEvalType::Real => 5,
        NativeEvalType::Decimal => 246,
        _ => return Err("numeric argument cast domain"),
    };
    let route = if source == NativeEvalType::String && !hybrid {
        match input {
            I::String(_) | I::Bytes(_) => R::String,
            _ => return Err("string arithmetic argument domain"),
        }
    } else {
        match (input, target) {
            (I::Json { .. }, NativeEvalType::Real) => R::JsonReal,
            (I::Json { .. }, NativeEvalType::Int) => R::JsonInt,
            (I::Time(_) | I::Duration(_) | I::Json { .. }, NativeEvalType::Decimal) => {
                R::ContextDecimal
            }
            (I::Real(_), NativeEvalType::Decimal) => R::RealDecimal,
            _ => R::Fit,
        }
    };
    Ok(NativeNumericArgumentHead::Cast {
        normalization,
        route,
        target_code,
    })
}

/// Numeric-argument DECIMAL target shape. Source evaluation type selects the
/// integer-width table; other sources use the target DECIMAL's shared caps.
pub fn native_numeric_argument_decimal_shape(
    source: NativeEvalType,
    code: NativeTypeNameCode,
    flen: i64,
    decimal: i64,
) -> (i64, i64) {
    if source == NativeEvalType::Int {
        let width = match code {
            NativeTypeNameCode::Known(1) => 3,
            NativeTypeNameCode::Known(2) => 5,
            NativeTypeNameCode::Known(9) => 8,
            NativeTypeNameCode::Known(3) => 10,
            NativeTypeNameCode::Known(8) => 20,
            NativeTypeNameCode::Known(13) => 4,
            _ => 20,
        };
        (width, 0)
    } else {
        let target = NativeTypeNameCode::Known(246);
        let width = if flen < 0 {
            tidb_query_datatype::MAX_DECIMAL_WIDTH as i64
        } else {
            flen
        };
        (
            native_field_flen_under_limit(target, width),
            native_field_decimal_under_limit(target, decimal),
        )
    }
}
/// JSON integer arguments are document Display re-read as an ordinary String,
/// not numeric JSON casts. Warn/veto precedes the value-only signed conversion;
/// its actual UTC zone matches the original no-session-zone helper.
pub fn native_numeric_argument_json_to_i64<E>(
    type_code: u8,
    value: &[u8],
    handle: impl FnMut(&str) -> Result<(), E>,
) -> Result<i64, E> {
    let text = JsonDisplay { type_code, value }.to_string();
    let input = NativeNumericInput::String(text.as_bytes());
    crate::native_cast_integer::native_cast_integer_numeric_input_warning(input, handle)?;
    Ok(crate::native_cast_integer::native_cast_integer_signed_numeric(input, &chrono::Utc))
}

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
#[derive(Debug)]
enum RealDecimalValue {
    Ready(NativeDecimalParseValue),
    Parsed {
        value: NativeMyDecimal,
        overflow: Option<f64>,
    },
}
/// Staged source conversion. The expression renderer remains outside this
/// boundary and is demanded only after overflow has passed the level guard.
#[derive(Debug)]
pub struct NativeNumericArgumentRealDecimal {
    value: RealDecimalValue,
}
impl NativeNumericArgumentRealDecimal {
    pub fn requires_expression_subject(&self) -> bool {
        matches!(
            &self.value,
            RealDecimalValue::Parsed {
                overflow: Some(_),
                ..
            }
        )
    }
    /// Accept the original diagnostic effect before projecting fixed-word
    /// storage. A supplied subject (including empty text) is used unchanged;
    /// only a missing subject demands the canonical shortest-float fallback.
    pub fn finish<E>(
        self,
        subject: Option<&str>,
        mut handle: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<NativeDecimalParseValue, E> {
        match self.value {
            RealDecimalValue::Ready(value) => Ok(value),
            RealDecimalValue::Parsed { value, overflow } => {
                if let Some(real) = overflow {
                    let fallback;
                    let subject = match subject {
                        Some(subject) => subject,
                        None => {
                            fallback=tidb_query_datatype::codec::mysql::Decimal::native_format_float_g_shortest(real);
                            &fallback
                        }
                    };
                    handle(&format!("Truncated incorrect DECIMAL value: '{subject}'"))?;
                }
                Ok(native_decimal_from_my_decimal(value))
            }
        }
    }
}
/// Exact integral doubles in the inclusive +/-2^53 interval use the original
/// integer shortcut, except negative zero. Other values keep raw MyDecimal
/// until finish, and only Overflow consults the supplied level effect.
pub fn native_numeric_argument_real_decimal_prepare(
    real: f64,
    mut level: impl FnMut() -> NativeNumericArgumentLevel,
) -> Result<NativeNumericArgumentRealDecimal, NativeNumericArgumentConversionError> {
    if real.abs() <= 9_007_199_254_740_992.0
        && real.fract() == 0.0
        && (real != 0.0 || !real.is_sign_negative())
    {
        return Ok(NativeNumericArgumentRealDecimal {
            value: RealDecimalValue::Ready(NativeDecimalParseValue::from_int(real as i64)),
        });
    }
    let (value, error) = NativeMyDecimal::from_float64(real);
    let overflow = match error {
        Some(NativeMyDecimalError::Overflow) => {
            if level() == NativeNumericArgumentLevel::Error {
                return Err(NativeNumericArgumentConversionError::Overflow);
            }
            Some(real)
        }
        Some(NativeMyDecimalError::Truncated) | None => None,
        Some(_) => return Err(NativeNumericArgumentConversionError::BadNumber),
    };
    Ok(NativeNumericArgumentRealDecimal {
        value: RealDecimalValue::Parsed { value, overflow },
    })
}
/// The unspecified-scale final arm constructs only actual signed/unsigned
/// integers; every other datum remains the caller's original value.
pub fn native_numeric_argument_unscaled_integer(
    input: NativeNumericInput<'_>,
) -> Option<NativeDecimalParseValue> {
    match input {
        NativeNumericInput::Int(value) => Some(NativeDecimalParseValue::from_int(value)),
        NativeNumericInput::UInt(value) => Some(NativeDecimalParseValue::from_uint(value)),
        _ => None,
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
mod completion_tests {
    use super::*;
    #[test]
    fn numeric_argument_target_keeps_decimal_only_fitting_and_string_int_intermediate_decimal() {
        use NativeEvalType as T;
        use NativeTypeNameCode::{Known, Unknown};
        for (source, code, flen, scale, shape, skip) in [
            (T::Int, Known(1), 99, -1, (3, 0), false),
            (T::Int, Known(13), -1, -2, (4, 0), false),
            (T::Int, Unknown(1), 0, i64::MIN, (20, 0), false),
            (T::Decimal, Known(246), -1, -1, (65, -1), true),
            (T::String, Known(253), 66, 31, (65, 30), false),
            (T::Real, Known(5), 17, -2, (17, -2), true),
            (
                T::Json,
                Known(245),
                i64::MIN,
                i64::MIN,
                (65, i64::MIN),
                true,
            ),
            (T::Datetime, Known(12), 0, 0, (0, 0), false),
        ] {
            assert_eq!(
                native_numeric_argument_target(source, code, flen, scale, 246),
                NativeNumericArgumentTarget {
                    code: 246,
                    decimal_shape: Some(shape),
                    skip_fitting: skip
                }
            );
        }
        for code in [0, 5, 8, 245, 255] {
            assert_eq!(
                native_numeric_argument_target(T::Decimal, Known(246), i64::MAX, i64::MIN, code),
                NativeNumericArgumentTarget {
                    code,
                    decimal_shape: None,
                    skip_fitting: false
                }
            );
        }
        for target in T::ALL {
            assert_eq!(
                native_numeric_argument_string_is_real(target),
                target == T::Real
            );
        }
        // String-as-Int still takes the source's intermediate Decimal target;
        // it must not adopt final BIGINT defaults just because Int was requested.
        assert!(!native_numeric_argument_string_is_real(T::Int));
        assert_eq!(
            native_numeric_argument_target(T::String, Known(253), 9, -1, 246),
            NativeNumericArgumentTarget {
                code: 246,
                decimal_shape: Some((9, -1)),
                skip_fitting: true
            }
        );
    }
    #[test]
    fn numeric_argument_result_policies_preserve_owned_value_error_and_distinct_failure_subjects() {
        use std::{cell::Cell, rc::Rc};

        use NativeNumericArgumentResultError as R;
        // Neither the input engine error nor the typed conversion payload needs
        // Clone/Display. Pointer identity pins transfer rather than rebuilding.
        struct EngineError;
        #[derive(Debug)]
        struct Payload(Box<u64>);
        #[derive(Debug)]
        struct Dropped(Rc<Cell<usize>>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        for context in [false, true] {
            let value = Box::new(37u64);
            let pointer = &*value as *const u64;
            let result: Result<_, EngineError> = Ok((value, None::<Payload>));
            let value = if context {
                native_numeric_argument_context_decimal_result(result)
            } else {
                native_numeric_argument_conversion_result(result)
            }
            .unwrap();
            assert_eq!(&*value as *const u64, pointer);
            assert_eq!(*value, 37);
            let dropped = Rc::new(Cell::new(0));
            let error = Payload(Box::new(99));
            let pointer = &*error.0 as *const u64;
            let result: Result<_, EngineError> = Ok((Dropped(dropped.clone()), Some(error)));
            let error = if context {
                native_numeric_argument_context_decimal_result(result)
            } else {
                native_numeric_argument_conversion_result(result)
            }
            .unwrap_err();
            let R::Conversion(error) = error else {
                panic!("typed conversion error must survive");
            };
            assert_eq!(&*error.0 as *const u64, pointer);
            assert_eq!(*error.0, 99);
            assert_eq!(dropped.get(), 1);
            let result: Result<((), Option<Payload>), _> = Err(EngineError);
            let error = if context {
                native_numeric_argument_context_decimal_result(result)
            } else {
                native_numeric_argument_conversion_result(result)
            }
            .unwrap_err();
            let R::Unsupported(message) = error else {
                panic!("engine error maps to original unsupported subject");
            };
            assert_eq!(
                message,
                if context {
                    "numeric decimal argument conversion failed"
                } else {
                    "numeric argument conversion failed"
                }
            );
        }
    }
}

#[cfg(test)]
mod head_tests {
    use NativeEvalType as T;
    use NativeNumericArgumentHead as H;
    use NativeNumericArgumentNormalization as N;
    use NativeNumericArgumentRoute as R;
    use NativeNumericInput as I;

    use super::*;
    #[test]
    fn numeric_argument_head_preserves_identity_before_normalization_and_validates_before_admission()
     {
        for target in T::ALL {
            assert_eq!(
                native_numeric_argument_head(I::Null, T::String, target, true, false),
                Ok(H::Preserve)
            );
        }
        for source in T::ALL {
            for input in [
                I::Int(-1),
                I::Float32(16_777_217.0),
                I::Float32(f64::NAN),
                I::Raw(b"bad"),
                I::MinNotNull,
                I::MaxValue,
            ] {
                assert_eq!(
                    native_numeric_argument_head(input, source, source, true, false),
                    Ok(H::Preserve)
                );
            }
        }
        let json = I::Json {
            type_code: 9,
            value: &[],
        };
        assert_eq!(
            native_numeric_argument_head(json, T::String, T::Datetime, false, false),
            Err("numeric argument cast domain")
        );
        for target in [T::Int, T::Real, T::Decimal] {
            for input in [
                json,
                I::Int(-1),
                I::Float32(1.25),
                I::Raw(b"1"),
                I::MinNotNull,
                I::MaxValue,
            ] {
                assert_eq!(
                    native_numeric_argument_head(input, T::String, target, true, false),
                    Err("string arithmetic argument domain")
                );
            }
        }
        for target in [
            T::String,
            T::Datetime,
            T::Timestamp,
            T::Duration,
            T::Json,
            T::VectorFloat32,
        ] {
            assert_eq!(
                native_numeric_argument_head(I::Int(-1), T::Int, target, true, false),
                Err("numeric argument cast domain")
            );
        }
        assert_eq!(
            native_numeric_argument_head(I::Int(-1), T::Int, T::Real, true, false),
            Ok(H::Cast {
                normalization: N::UInt(u64::MAX),
                route: R::Fit,
                target_code: 5
            })
        );
        assert_eq!(
            native_numeric_argument_head(I::Int(-1), T::Int, T::Real, false, false),
            Ok(H::Cast {
                normalization: N::Keep,
                route: R::Fit,
                target_code: 5
            })
        );
        assert_eq!(
            native_numeric_argument_head(I::UInt(u64::MAX), T::Int, T::Decimal, false, false),
            Ok(H::Cast {
                normalization: N::Keep,
                route: R::Fit,
                target_code: 246
            })
        );
        for (raw, expected) in [
            (16_777_217.0, 16_777_216.0),
            (-0.0, -0.0),
            (1e-50, 0.0),
            (f64::MAX, f64::INFINITY),
        ] {
            let head =
                native_numeric_argument_head(I::Float32(raw), T::Real, T::Decimal, false, false)
                    .unwrap();
            let H::Cast {
                normalization: N::Real(value),
                route: R::RealDecimal,
                target_code: 246,
            } = head
            else {
                panic!("actual normalized Real route");
            };
            assert_eq!(value.to_bits(), expected.to_bits());
        }
        let head =
            native_numeric_argument_head(I::Float32(f64::NAN), T::Real, T::Int, false, false)
                .unwrap();
        let H::Cast {
            normalization: N::Real(value),
            route: R::Fit,
            target_code: 8,
        } = head
        else {
            panic!("NaN remains an actual Real value");
        };
        assert!(value.is_nan());
    }
    #[test]
    fn numeric_argument_head_routes_actual_values_after_string_admission_and_hybrid_bypass() {
        use tidb_query_datatype::codec::{
            mysql::{
                NativeDecimalParseRef, NativeVectorFloat32,
                time::{NativeTemporalValue, TimeType},
            },
            native_duration_convert::NativeDurationParts,
        };
        let json = I::Json {
            type_code: 4,
            value: &[],
        };
        let time = I::Time(NativeTemporalValue {
            raw: 0,
            kind: TimeType::DateTime,
            fsp: 255,
        });
        let duration = I::Duration(NativeDurationParts {
            nanoseconds: 0,
            fsp: -2,
        });
        let decimal = I::Decimal(NativeDecimalParseRef {
            negative: true,
            digits: &[255],
            scale: 1,
            storage_scale: 0,
            declared_shape: None,
        });
        let vector = NativeVectorFloat32::default();
        for (input, source, target, hybrid, route, code) in [
            (I::String(b"1\xff"), T::String, T::Int, false, R::String, 8),
            (I::Bytes(b"1"), T::String, T::Real, false, R::String, 5),
            (
                I::String(b"1"),
                T::String,
                T::Decimal,
                false,
                R::String,
                246,
            ),
            (json, T::Json, T::Real, false, R::JsonReal, 5),
            (json, T::Json, T::Int, false, R::JsonInt, 8),
            (json, T::Json, T::Decimal, false, R::ContextDecimal, 246),
            (json, T::String, T::Real, true, R::JsonReal, 5),
            (time, T::Datetime, T::Decimal, false, R::ContextDecimal, 246),
            (
                duration,
                T::Duration,
                T::Decimal,
                false,
                R::ContextDecimal,
                246,
            ),
            (
                I::Real(1.25),
                T::Real,
                T::Decimal,
                false,
                R::RealDecimal,
                246,
            ),
            (
                I::Real(1.25),
                T::String,
                T::Decimal,
                true,
                R::RealDecimal,
                246,
            ),
            (I::Enum(7), T::String, T::Real, true, R::Fit, 5),
            (I::Set(3), T::String, T::Decimal, true, R::Fit, 246),
            (I::String(b"1"), T::String, T::Real, true, R::Fit, 5),
            (I::Bytes(b"1"), T::Json, T::Int, false, R::Fit, 8),
            (time, T::Datetime, T::Real, false, R::Fit, 5),
            (duration, T::Duration, T::Int, false, R::Fit, 8),
            (decimal, T::Decimal, T::Real, false, R::Fit, 5),
            (I::Bit(&[1; 9]), T::Int, T::Real, false, R::Fit, 5),
            (I::Raw(b"1"), T::Real, T::Int, false, R::Fit, 8),
            (
                I::VectorFloat32(&vector),
                T::VectorFloat32,
                T::Decimal,
                false,
                R::Fit,
                246,
            ),
        ] {
            assert_eq!(
                native_numeric_argument_head(input, source, target, false, hybrid),
                Ok(H::Cast {
                    normalization: N::Keep,
                    route,
                    target_code: code
                })
            );
        }
    }
}

#[cfg(test)]
mod real_decimal_tests {
    use NativeNumericArgumentConversionError as C;
    use NativeNumericArgumentLevel as L;

    use super::*;
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
    fn real_decimal_stage_keeps_integral_bounds_negative_zero_raw_shape_and_unscaled_domains() {
        for (real, expected) in [
            (0.0, "0"),
            (42.0, "42"),
            (-42.0, "-42"),
            (9_007_199_254_740_992.0, "9007199254740992"),
            (-9_007_199_254_740_992.0, "-9007199254740992"),
        ] {
            let state = native_numeric_argument_real_decimal_prepare(real, || {
                panic!("integral shortcut reads no level")
            })
            .unwrap();
            assert!(matches!(&state.value, RealDecimalValue::Ready(_)));
            assert!(!state.requires_expression_subject());
            let value = state
                .finish::<()>(Some("undemanded subject"), |_| panic!("undemanded handle"))
                .unwrap();
            assert_eq!(visible(&value), expected);
            let parts = value.as_ref();
            assert_eq!(
                (parts.scale, parts.storage_scale, parts.declared_shape),
                (0, 0, None)
            );
        }
        for (real, expected) in [
            (-0.0, "0"),
            (1.25, "1.25"),
            (9_007_199_254_740_994.0, "9007199254740994"),
            (-9_007_199_254_740_994.0, "-9007199254740994"),
            (1e-100, "0"),
        ] {
            let state = native_numeric_argument_real_decimal_prepare(real, || {
                panic!("nonoverflow parser reads no level")
            })
            .unwrap();
            assert!(matches!(
                &state.value,
                RealDecimalValue::Parsed { overflow: None, .. }
            ));
            assert!(!state.requires_expression_subject());
            let value = state
                .finish::<()>(Some("ignored even on parsed values"), |_| {
                    panic!("undemanded handle")
                })
                .unwrap();
            assert_eq!(visible(&value), expected);
            if real == 1.25 {
                let parts = value.as_ref();
                assert_eq!(
                    (
                        parts.negative,
                        parts.digits,
                        parts.scale,
                        parts.storage_scale,
                        parts.declared_shape
                    ),
                    (false, &b"125"[..], 2, 2, None)
                );
            }
            if real == 0.0 {
                assert!(!value.as_ref().negative);
            }
        }
        // Fixed-word shift discards fractions beyond its nine-word capacity
        // with Truncated (native_mydecimal::shift), not an overflow effect.
        assert_eq!(
            NativeMyDecimal::from_float64(1e-100).1,
            Some(NativeMyDecimalError::Truncated)
        );
        for real in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                native_numeric_argument_real_decimal_prepare(real, || panic!(
                    "bad number reads no level"
                ))
                .unwrap_err(),
                C::BadNumber
            );
        }
        use NativeNumericInput as N;
        for (input, expected) in [
            (N::Int(i64::MIN), "-9223372036854775808"),
            (N::UInt(u64::MAX), "18446744073709551615"),
        ] {
            let value = native_numeric_argument_unscaled_integer(input).unwrap();
            assert_eq!(visible(&value), expected);
            let parts = value.as_ref();
            assert_eq!(
                (parts.scale, parts.storage_scale, parts.declared_shape),
                (0, 0, None)
            );
        }
        let raw = tidb_query_datatype::codec::mysql::NativeDecimalParseRef {
            negative: true,
            digits: &[255],
            scale: 1,
            storage_scale: 0,
            declared_shape: None,
        };
        let vector = tidb_query_datatype::codec::mysql::NativeVectorFloat32::default();
        for input in [
            N::Real(1.0),
            N::Float32(1.0),
            N::Enum(1),
            N::Set(1),
            N::Bit(&[1]),
            N::BinaryLiteral(&[1]),
            N::Decimal(raw),
            N::String(b"1"),
            N::Bytes(b"1"),
            N::Json {
                type_code: 9,
                value: &[],
            },
            N::Raw(b"1"),
            N::VectorFloat32(&vector),
            N::Null,
            N::MinNotNull,
            N::MaxValue,
        ] {
            assert!(native_numeric_argument_unscaled_integer(input).is_none());
        }
    }
    #[test]
    fn real_decimal_overflow_stage_keeps_level_subject_handle_order_and_veto_before_projection() {
        let mut calls = Vec::new();
        let error = native_numeric_argument_real_decimal_prepare(1e100, || {
            calls.push("level");
            L::Error
        })
        .unwrap_err();
        assert_eq!(error, C::Overflow);
        assert_eq!(calls, ["level"]);
        for level in [L::Warn, L::Ignore] {
            let mut calls = Vec::new();
            let state = native_numeric_argument_real_decimal_prepare(1e100, || {
                calls.push("level".to_owned());
                level
            })
            .unwrap();
            assert!(state.requires_expression_subject());
            assert!(matches!(
                &state.value,
                RealDecimalValue::Parsed {
                    overflow: Some(_),
                    ..
                }
            ));
            assert_eq!(calls, ["level"]);
            // The host's original expression renderer runs only at this point.
            calls.push("subject".into());
            let value = state.finish(Some(" expression + 1 "), |message| {
                calls.push(format!("handle:{message}"));
                Err("veto")
            });
            assert_eq!(value.unwrap_err(), "veto");
            assert_eq!(
                calls,
                [
                    "level",
                    "subject",
                    "handle:Truncated incorrect DECIMAL value: ' expression + 1 '"
                ]
            );
        }
        for (subject, expected) in [
            (None, "1e+100"),
            (Some(""), ""),
            (Some("expression"), "expression"),
        ] {
            let state = native_numeric_argument_real_decimal_prepare(1e100, || L::Warn).unwrap();
            let mut calls = Vec::new();
            let value = state
                .finish::<()>(subject, |message| {
                    calls.push(message.to_owned());
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                calls,
                vec![format!("Truncated incorrect DECIMAL value: '{expected}'")]
            );
            // FromString handles shift Overflow by max_decimal(9*9, 0): the
            // accepted raw MyDecimal projects to this exact unshaped payload.
            assert_eq!(visible(&value), "9".repeat(81));
            let parts = value.as_ref();
            assert_eq!(
                (parts.scale, parts.storage_scale, parts.declared_shape),
                (0, 0, None)
            );
        }
    }
}

#[cfg(test)]
mod shape_integer_tests {
    use super::*;
    #[test]
    fn numeric_argument_decimal_shape_keeps_integer_widths_and_noninteger_metadata_caps() {
        use NativeTypeNameCode::{Known, Unknown};
        for (code, width) in [
            (Known(1), 3),
            (Known(2), 5),
            (Known(9), 8),
            (Known(3), 10),
            (Known(8), 20),
            (Known(13), 4),
            (Known(16), 20),
            (Known(247), 20),
            (Known(248), 20),
            (Unknown(1), 20),
        ] {
            assert_eq!(
                native_numeric_argument_decimal_shape(
                    NativeEvalType::Int,
                    code,
                    i64::MAX,
                    i64::MIN
                ),
                (width, 0)
            );
        }
        for source in NativeEvalType::ALL {
            if source == NativeEvalType::Int {
                continue;
            }
            for code in [Known(246), Known(245), Known(1), Unknown(246)] {
                for (flen, decimal, expected) in [
                    (-1, -1, (65, -1)),
                    (i64::MIN, i64::MIN, (65, i64::MIN)),
                    (0, 31, (0, 30)),
                    (66, 99, (65, 30)),
                    (12, 2, (12, 2)),
                    (15, -2, (15, -2)),
                ] {
                    assert_eq!(
                        native_numeric_argument_decimal_shape(source, code, flen, decimal),
                        expected
                    );
                }
            }
        }
    }
    #[test]
    fn json_integer_arguments_reparse_display_and_keep_warning_veto_and_panic_boundaries() {
        use tidb_query_datatype::codec::native_json_parse::native_json_parse;
        for (document, expected, subject) in [
            ("3", 3, None),
            ("18446744073709551615", -1, None),
            ("1.5", 1, Some("1.5")),
            ("1e20", 1, Some("1e20")),
            ("\"3\"", 0, Some("\"3\"")),
            ("false", 0, Some("false")),
            ("null", 0, Some("null")),
            ("[]", 0, Some("[]")),
            ("{}", 0, Some("{}")),
        ] {
            let (tag, bytes) = native_json_parse(document).unwrap();
            let mut calls = Vec::new();
            let value = native_numeric_argument_json_to_i64::<()>(tag, &bytes, |message| {
                calls.push(message.to_owned());
                Ok(())
            });
            assert_eq!(value, Ok(expected));
            assert_eq!(
                calls,
                subject
                    .map(|subject| format!("Truncated incorrect INTEGER value: '{subject}'"))
                    .into_iter()
                    .collect::<Vec<_>>()
            );
        }
        let document = format!("\"{}\"", "界".repeat(50));
        let (tag, bytes) = native_json_parse(&document).unwrap();
        let mut calls = Vec::new();
        let value = native_numeric_argument_json_to_i64(tag, &bytes, |message| {
            calls.push(message.to_owned());
            Err("veto")
        });
        assert_eq!(value, Err("veto"));
        // The existing integer warning cap admits the opening quote plus 42
        // complete three-byte characters (127 bytes), not a split character.
        assert_eq!(
            calls,
            vec![format!(
                "Truncated incorrect INTEGER value: '\"{}'",
                "界".repeat(42)
            )]
        );
        for tag in [4, 9, 10, 11] {
            let mut calls = Vec::new();
            let value = native_numeric_argument_json_to_i64::<()>(tag, &[], |message| {
                calls.push(message.to_owned());
                Ok(())
            });
            assert_eq!(value, Ok(0));
            assert_eq!(calls, ["Truncated incorrect INTEGER value: ''"]);
        }
        for value in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            let calls = std::cell::RefCell::new(Vec::new());
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                native_numeric_argument_json_to_i64::<()>(11, &value.to_le_bytes(), |message| {
                    calls.borrow_mut().push(message.to_owned());
                    Ok(())
                })
            }));
            assert!(panic.is_err());
            assert!(calls.borrow().is_empty());
        }
    }
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
