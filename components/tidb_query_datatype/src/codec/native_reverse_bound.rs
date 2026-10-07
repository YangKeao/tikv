// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native reverse-conversion bounds and control decisions. Evaluation, source
//! comparison and decimal arithmetic remain with their existing owners.

use super::{
    native_decimal_convert::native_bound_decimal_text,
    native_integer_convert::{
        native_integer_signed_upper_bound, native_integer_unsigned_upper_bound,
    },
    native_type_name::NativeTypeNameCode,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeReverseSourceKind {
    Int,
    UInt,
    Float32,
    Real,
    Decimal { digits: usize, scale: usize },
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NativeReverseSourceBound {
    Int(i64),
    UInt(u64),
    Real { value: f64, float32: bool },
    DecimalText(String),
    Maximum,
    MinimumNotNull,
}

pub fn native_reverse_source_bound(
    kind: NativeReverseSourceKind,
    maximum: bool,
) -> NativeReverseSourceBound {
    use NativeReverseSourceBound as Bound;
    match kind {
        NativeReverseSourceKind::Int => Bound::Int(if maximum { i64::MAX } else { i64::MIN }),
        NativeReverseSourceKind::UInt => Bound::UInt(if maximum { u64::MAX } else { 0 }),
        NativeReverseSourceKind::Float32 => Bound::Real {
            value: if maximum {
                f64::from(f32::MAX)
            } else {
                -f64::from(f32::MAX)
            },
            float32: true,
        },
        NativeReverseSourceKind::Real => Bound::Real {
            value: if maximum { f64::MAX } else { -f64::MAX },
            float32: false,
        },
        NativeReverseSourceKind::Decimal { digits, scale } => Bound::DecimalText(
            native_bound_decimal_text(digits.max(1) as i64, scale as i64, maximum),
        ),
        NativeReverseSourceKind::Other => {
            if maximum {
                Bound::Maximum
            } else {
                Bound::MinimumNotNull
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeReversePrepare {
    ReturnConverted,
    CompareSourceBound,
}

pub const fn native_reverse_prepare(converted_overflow: bool) -> NativeReversePrepare {
    if converted_overflow {
        NativeReversePrepare::ReturnConverted
    } else {
        NativeReversePrepare::CompareSourceBound
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeReverseFinish {
    ReplaceTargetBound,
    Increment,
    Keep,
}

pub const fn native_reverse_finish(equal: bool, ceiling: bool) -> NativeReverseFinish {
    if equal {
        NativeReverseFinish::ReplaceTargetBound
    } else if ceiling {
        NativeReverseFinish::Increment
    } else {
        NativeReverseFinish::Keep
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeReverseIncrementInput {
    Int(i64),
    UInt(u64),
    Float32(f64),
    Real(f64),
    Decimal { at_target_max: bool },
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NativeReverseIncrement {
    Int(i64),
    UInt(u64),
    Float32(f64),
    Real(f64),
    IncrementDecimal,
    Keep,
}

pub fn native_reverse_increment(
    input: NativeReverseIncrementInput,
    target: NativeTypeNameCode,
) -> NativeReverseIncrement {
    use NativeReverseIncrement as Output;
    match input {
        NativeReverseIncrementInput::Int(value) => Output::Int(
            value
                .checked_add(1)
                .filter(|next| *next <= native_integer_signed_upper_bound(target))
                .unwrap_or(value),
        ),
        NativeReverseIncrementInput::UInt(value) => Output::UInt(
            value
                .checked_add(1)
                .filter(|next| *next <= native_integer_unsigned_upper_bound(target))
                .unwrap_or(value),
        ),
        NativeReverseIncrementInput::Float32(value) => {
            Output::Float32(if value < f64::from(f32::MAX) {
                value + 1.0
            } else {
                value
            })
        }
        NativeReverseIncrementInput::Real(value) => {
            Output::Real(if value < f64::MAX { value + 1.0 } else { value })
        }
        NativeReverseIncrementInput::Decimal { at_target_max } => {
            if at_target_max {
                Output::Keep
            } else {
                Output::IncrementDecimal
            }
        }
        NativeReverseIncrementInput::Other => Output::Keep,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_states_and_source_bounds_preserve_priority_and_raw_decimal_shape() {
        const PREPARE: NativeReversePrepare = native_reverse_prepare(true);
        const FINISH: NativeReverseFinish = native_reverse_finish(true, false);
        assert_eq!(PREPARE, NativeReversePrepare::ReturnConverted);
        assert_eq!(
            native_reverse_prepare(false),
            NativeReversePrepare::CompareSourceBound
        );
        assert_eq!(FINISH, NativeReverseFinish::ReplaceTargetBound);
        assert_eq!(
            native_reverse_finish(true, true),
            NativeReverseFinish::ReplaceTargetBound
        );
        assert_eq!(
            native_reverse_finish(false, true),
            NativeReverseFinish::Increment
        );
        assert_eq!(
            native_reverse_finish(false, false),
            NativeReverseFinish::Keep
        );

        use NativeReverseSourceBound as B;
        use NativeReverseSourceKind as K;
        for (kind, maximum, expected) in [
            (K::Int, true, B::Int(i64::MAX)),
            (K::Int, false, B::Int(i64::MIN)),
            (K::UInt, true, B::UInt(u64::MAX)),
            (K::UInt, false, B::UInt(0)),
            (
                K::Float32,
                true,
                B::Real {
                    value: f64::from(f32::MAX),
                    float32: true,
                },
            ),
            (
                K::Float32,
                false,
                B::Real {
                    value: -f64::from(f32::MAX),
                    float32: true,
                },
            ),
            (
                K::Real,
                true,
                B::Real {
                    value: f64::MAX,
                    float32: false,
                },
            ),
            (
                K::Real,
                false,
                B::Real {
                    value: -f64::MAX,
                    float32: false,
                },
            ),
            (K::Other, true, B::Maximum),
            (K::Other, false, B::MinimumNotNull),
        ] {
            assert_eq!(native_reverse_source_bound(kind, maximum), expected);
        }
        for (digits, scale, maximum, expected) in [
            (0, 0, true, "9"),
            (0, 0, false, "-9"),
            (0, 2, true, "9.99"),
            (0, 2, false, "-9.99"),
            (3, 0, true, "999"),
            (3, 2, false, "-9.99"),
            (2, 4, true, "9.9999"),
        ] {
            assert_eq!(
                native_reverse_source_bound(K::Decimal { digits, scale }, maximum),
                B::DecimalText(expected.into())
            );
        }
    }

    #[test]
    fn reverse_increment_preserves_short_circuit_bounds_float_bits_and_decimal_actions() {
        use NativeReverseIncrement as O;
        use NativeReverseIncrementInput as I;
        use NativeTypeNameCode::{Known, Unknown};
        for (input, target, expected) in [
            (I::Int(126), Known(1), O::Int(127)),
            (I::Int(127), Known(1), O::Int(127)),
            (I::Int(200), Known(1), O::Int(200)),
            (I::Int(i64::MIN), Known(8), O::Int(i64::MIN + 1)),
            (I::UInt(254), Known(1), O::UInt(255)),
            (I::UInt(255), Known(1), O::UInt(255)),
            (I::UInt(300), Known(1), O::UInt(300)),
            (I::Int(i64::MAX), Unknown(8), O::Int(i64::MAX)),
            (I::UInt(u64::MAX), Unknown(8), O::UInt(u64::MAX)),
            (
                I::Decimal {
                    at_target_max: true,
                },
                Unknown(8),
                O::Keep,
            ),
            (
                I::Decimal {
                    at_target_max: false,
                },
                Unknown(8),
                O::IncrementDecimal,
            ),
            (I::Other, Unknown(8), O::Keep),
        ] {
            assert_eq!(native_reverse_increment(input, target), expected);
        }
        // Only a successful checked add demands the target's integer bound.
        for target in [Unknown(8), Known(245)] {
            assert!(
                std::panic::catch_unwind(|| native_reverse_increment(I::Int(0), target)).is_err()
            );
            assert!(
                std::panic::catch_unwind(|| native_reverse_increment(I::UInt(0), target)).is_err()
            );
            assert_eq!(
                native_reverse_increment(I::Int(i64::MAX), target),
                O::Int(i64::MAX)
            );
            assert_eq!(
                native_reverse_increment(I::UInt(u64::MAX), target),
                O::UInt(u64::MAX)
            );
        }
        assert_eq!(
            native_reverse_increment(I::Float32(-0.0), Unknown(8)),
            O::Float32(1.0)
        );
        assert_eq!(
            native_reverse_increment(I::Real(2.5), Unknown(8)),
            O::Real(3.5)
        );
        for value in [
            f64::from(f32::MAX),
            f64::MAX,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::from_bits(0xfff8_0000_0000_1234),
        ] {
            let O::Float32(actual) = native_reverse_increment(I::Float32(value), Unknown(8)) else {
                panic!("wrong float32 carrier")
            };
            assert_eq!(actual.to_bits(), value.to_bits());
        }
        for value in [
            f64::MAX,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::from_bits(0x7ff8_0000_0000_1234),
        ] {
            let O::Real(actual) = native_reverse_increment(I::Real(value), Unknown(8)) else {
                panic!("wrong real carrier")
            };
            assert_eq!(actual.to_bits(), value.to_bits());
        }
    }
}
