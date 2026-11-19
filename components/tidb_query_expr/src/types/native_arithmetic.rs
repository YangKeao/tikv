// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use tidb_datatype::{DecimalCodecWarning, float_warning_input, str_to_float};
use tidb_query_common::error::{EvaluateError, Result};
use tidb_query_datatype::{
    EvalType, FieldTypeAccessor, FieldTypeFlag, FieldTypeTp,
    codec::data_type::{ChunkedVecSized, Decimal, Real, ScalarValueRef, VectorValue},
};
use tipb::FieldType;

use super::{
    ExpressionDecimal, ExpressionScalar, ExpressionVector, InputColumns, RpnExpressionNode,
    RpnFnMeta, RpnStackNode, RpnStackNodeVectorValue,
};
use crate::impl_compare::{BasicComparer, CmpOp, compare_fn_meta};

/// Native arithmetic uses full expression values; PB signatures remain
/// unchanged.
#[derive(Debug, Clone, Copy)]
pub enum NativeArithmetic {
    Add,
    Subtract,
    Multiply,
    Divide,
    IntegerModulo,
    IntegerDivide,
}

/// Static source text and demanded raw parameter positions for overflow
/// diagnostics.
#[derive(Debug, Clone)]
pub enum NativeDiagnosticPart {
    Text(String),
    Parameter(usize),
}

pub(super) fn parameter_decimal<'a>(
    order: usize,
    field_type: &'a FieldType,
    input: &dyn InputColumns,
) -> Result<RpnStackNode<'a>> {
    let value = input.parameter(order)?;
    let field = tidb_datatype::infer_param_type_from_datum(&value);
    let value = if value.is_null() || field.code() == tidb_datatype::FieldTypeCode::Null {
        None
    } else {
        let context = tidb_datatype::ConversionContext::new(
            input.conversion_flags(),
            input.conversion_location(),
            input.conversion_warnings(),
        );
        let (value, error) = value
            .to_typed_decimal_with_context(&field, &context)
            .map_err(|_| EvaluateError::Custom {
                code: 1105,
                msg: "constant cannot be converted to decimal".into(),
            })?;
        if let Some(error) = error {
            let error = error.to_sql_error();
            return Err(EvaluateError::Custom {
                code: i32::from(error.code),
                msg: error.message,
            }
            .into());
        }
        Some(value.into())
    };
    Ok(RpnStackNode::ExpressionScalar {
        value: std::borrow::Cow::Owned(ExpressionScalar::Decimal(value)),
        field_type,
    })
}

pub(super) fn is_null(value: &RpnStackNode<'_>, row: usize) -> bool {
    match value {
        RpnStackNode::ExpressionScalar { value, .. } => match value.as_ref() {
            ExpressionScalar::Time(v) => v.is_none(),
            ExpressionScalar::Real(v) => v.is_none(),
            ExpressionScalar::Decimal(v) => v.is_none(),
        },
        RpnStackNode::ExpressionVector { value, .. } => match value {
            ExpressionVector::Time(v) => v[row].is_none(),
            ExpressionVector::Real(v) => v[row].is_none(),
            ExpressionVector::Decimal(v) => v[row].is_none(),
        },
        _ => matches!(
            value.get_logical_scalar_ref(row),
            ScalarValueRef::Int(None)
                | ScalarValueRef::Bytes(None)
                | ScalarValueRef::Real(None)
                | ScalarValueRef::Decimal(None)
        ),
    }
}
fn real(value: &RpnStackNode<'_>, row: usize) -> Result<Option<f64>> {
    match value {
        RpnStackNode::ExpressionScalar { value, .. } => match value.as_ref() {
            ExpressionScalar::Real(v) => Ok(*v),
            _ => Err(other_err!("native real operand required")),
        },
        RpnStackNode::ExpressionVector {
            value: ExpressionVector::Real(v),
            ..
        } => Ok(v[row]),
        _ => Err(other_err!("native real operand required")),
    }
}
pub(super) struct NativeComparisonArgs;

trait NativeComparisonValue: PartialOrd + Sized {
    fn from_stack(value: &RpnStackNode<'_>, row: usize) -> Result<Option<Self>>;
}

impl NativeComparisonValue for f64 {
    fn from_stack(value: &RpnStackNode<'_>, row: usize) -> Result<Option<Self>> {
        real(value, row)
    }
}

impl NativeComparisonValue for tidb_datatype::Decimal {
    fn from_stack(value: &RpnStackNode<'_>, row: usize) -> Result<Option<Self>> {
        Ok(decimal(value, row)?
            .cloned()
            .map(ExpressionDecimal::into_decimal))
    }
}

/// Compare full-domain native doubles without converting them to PB Real.
pub fn native_real_comparison_node<F: CmpOp>() -> RpnExpressionNode {
    native_comparison_node::<F, f64>(compare_fn_meta::<BasicComparer<Real, F>>())
}

/// Compare exact native decimals with the original datatype ordering.
pub fn native_decimal_comparison_node<F: CmpOp>() -> RpnExpressionNode {
    native_comparison_node::<F, tidb_datatype::Decimal>(
        compare_fn_meta::<BasicComparer<Decimal, F>>(),
    )
}

fn native_comparison_node<F: CmpOp, T: NativeComparisonValue>(
    meta: RpnFnMeta,
) -> RpnExpressionNode {
    let func_meta = RpnFnMeta {
        fn_ptr: |_, rows, args, _, _| {
            let [lhs, rhs] = args else {
                return Err(other_err!("native comparison requires two operands"));
            };
            let mut values = Vec::with_capacity(rows);
            for row in 0..rows {
                values.push(match (T::from_stack(lhs, row)?, T::from_stack(rhs, row)?) {
                    (None, None) => F::compare_null(),
                    (None, _) | (_, None) => F::compare_partial_null(),
                    (Some(lhs), Some(rhs)) => {
                        let matched = lhs
                            .partial_cmp(&rhs)
                            .map_or_else(F::compare_unordered, F::compare_order);
                        Some(i64::from(matched))
                    }
                });
            }
            Ok(VectorValue::Int(ChunkedVecSized::from(values)))
        },
        ..meta
    };
    RpnExpressionNode::FnCall {
        func_meta,
        args_len: 2,
        field_type: FieldTypeTp::LongLong.into(),
        metadata: Box::new(NativeComparisonArgs),
    }
}

pub(super) fn valid_decimal(value: &ExpressionDecimal) -> bool {
    value.is_valid()
}
fn decimal<'a>(value: &'a RpnStackNode<'_>, row: usize) -> Result<Option<&'a ExpressionDecimal>> {
    let v = match value {
        RpnStackNode::ExpressionScalar { value, .. } => match value.as_ref() {
            ExpressionScalar::Decimal(v) => v.as_ref(),
            _ => return Err(other_err!("native decimal operand required")),
        },
        RpnStackNode::ExpressionVector {
            value: ExpressionVector::Decimal(v),
            ..
        } => v[row].as_ref(),
        _ => return Err(other_err!("native decimal operand required")),
    };
    if v.is_some_and(|v| !valid_decimal(v)) {
        return Err(other_err!("invalid expression decimal"));
    }
    Ok(v)
}

fn integer_arithmetic<'a>(
    op: NativeArithmetic,
    left: &RpnStackNode<'_>,
    right: &RpnStackNode<'_>,
    field_type: &'a FieldType,
    rows: usize,
    input: &dyn InputColumns,
) -> Result<RpnStackNode<'a>> {
    use tidb_query_common::error::ErrorInner;
    use tidb_query_datatype::codec::error::ERR_DATA_OUT_OF_RANGE;

    use crate::impl_arithmetic::{
        ArithmeticOp, IntDivideInt, IntDivideUint, IntIntMod, IntUintMod, UintDivideInt,
        UintDivideUint, UintIntMod, UintUintMod,
    };

    let unsigned = |value: &RpnStackNode<'_>| {
        value
            .field_type()
            .as_accessor()
            .flag()
            .contains(FieldTypeFlag::UNSIGNED)
    };
    let (lhs_unsigned, rhs_unsigned) = (unsigned(left), unsigned(right));
    let dividing = matches!(op, NativeArithmetic::IntegerDivide);
    let kernel = match (dividing, lhs_unsigned, rhs_unsigned) {
        (false, false, false) => IntIntMod::calc,
        (false, false, true) => IntUintMod::calc,
        (false, true, false) => UintIntMod::calc,
        (false, true, true) => UintUintMod::calc,
        (true, false, false) => IntDivideInt::calc,
        (true, false, true) => IntDivideUint::calc,
        (true, true, false) => UintDivideInt::calc,
        (true, true, true) => UintDivideUint::calc,
    };
    let mut values = Vec::with_capacity(rows);
    for row in 0..rows {
        let (ScalarValueRef::Int(lhs), ScalarValueRef::Int(rhs)) = (
            left.get_logical_scalar_ref(row),
            right.get_logical_scalar_ref(row),
        ) else {
            return Err(other_err!("integer arithmetic operands required"));
        };
        let result = match (lhs, rhs) {
            (Some(_), Some(0)) => {
                input.handle_division_by_zero()?;
                None
            }
            (Some(lhs), Some(rhs)) => kernel(lhs, rhs).map_err(|error| {
                if !dividing
                    || !matches!(
                        &*error.0,
                        ErrorInner::Evaluate(EvaluateError::Custom {
                            code: ERR_DATA_OUT_OF_RANGE,
                            ..
                        })
                    )
                {
                    return error;
                }
                let class = if lhs_unsigned || rhs_unsigned {
                    "BIGINT UNSIGNED"
                } else {
                    "BIGINT"
                };
                let text = |n: i64, unsigned| {
                    if unsigned {
                        (n as u64).to_string()
                    } else {
                        n.to_string()
                    }
                };
                EvaluateError::Custom {
                    code: ERR_DATA_OUT_OF_RANGE,
                    msg: format!(
                        "{class} value is out of range in '({}, {})'",
                        text(*lhs, lhs_unsigned),
                        text(*rhs, rhs_unsigned)
                    ),
                }
                .into()
            })?,
            _ => None,
        };
        values.push(result);
    }
    Ok(RpnStackNode::Vector {
        value: RpnStackNodeVectorValue::Generated {
            physical_value: VectorValue::Int(ChunkedVecSized::from(values)),
        },
        field_type: field_type.into(),
    })
}

pub(super) fn evaluate<'a>(
    op: NativeArithmetic,
    field_type: &'a FieldType,
    overflow: &str,
    diagnostic: &[NativeDiagnosticPart],
    args: &[RpnStackNode<'_>],
    rows: usize,
    input: &dyn InputColumns,
) -> Result<RpnStackNode<'a>> {
    let [left, right] = args else {
        return Err(other_err!("binary arithmetic requires two operands"));
    };
    if matches!(
        op,
        NativeArithmetic::IntegerModulo | NativeArithmetic::IntegerDivide
    ) {
        return integer_arithmetic(op, left, right, field_type, rows, input);
    }
    let overflow = || -> Result<EvaluateError> {
        let mut message = String::new();
        for part in diagnostic {
            match part {
                NativeDiagnosticPart::Text(text) => message.push_str(text),
                NativeDiagnosticPart::Parameter(order) => {
                    // Diagnostic lookup failure must not replace the arithmetic error.
                    let Some(text) = input
                        .parameter(*order)
                        .ok()
                        .and_then(|value| value.numeric_diagnostic_text(false))
                    else {
                        return Ok(EvaluateError::Custom {
                            code: 1690,
                            msg: "DECIMAL value is out of range".into(),
                        });
                    };
                    message.push_str(&text);
                }
            }
        }
        Ok(EvaluateError::Custom {
            code: 1690,
            msg: if diagnostic.is_empty() {
                overflow.to_owned()
            } else {
                message
            },
        })
    };
    let value = match EvalType::try_from(field_type.as_accessor().tp())
        .map_err(|e| other_err!("{}", e))?
    {
        EvalType::Real => ExpressionVector::Real(
            (0..rows)
                .map(|i| {
                    let (Some(a), Some(b)) = (real(left, i)?, real(right, i)?) else {
                        return Ok(None);
                    };
                    let value = match op {
                        NativeArithmetic::IntegerModulo | NativeArithmetic::IntegerDivide => {
                            unreachable!("handled above")
                        }
                        NativeArithmetic::Add => a + b,
                        NativeArithmetic::Subtract => a - b,
                        NativeArithmetic::Multiply => a * b,
                        NativeArithmetic::Divide if b == 0.0 => {
                            input.handle_division_by_zero()?;
                            return Ok(None);
                        }
                        NativeArithmetic::Divide => a / b,
                    };
                    if !value.is_finite() {
                        return Err(overflow()?.into());
                    }
                    Ok(Some(value))
                })
                .collect::<Result<Vec<_>>>()?,
        ),
        EvalType::Decimal => ExpressionVector::Decimal(
            (0..rows)
                .map(|i| {
                    let (Some(a), Some(b)) = (decimal(left, i)?, decimal(right, i)?) else {
                        return Ok(None);
                    };
                    let (value, warning) = match op {
                        NativeArithmetic::IntegerModulo | NativeArithmetic::IntegerDivide => {
                            unreachable!("handled above")
                        }
                        NativeArithmetic::Add => a.add_mysql(b),
                        NativeArithmetic::Subtract => a.sub_mysql(b),
                        NativeArithmetic::Multiply => a.mul_mysql(b),
                        NativeArithmetic::Divide => {
                            if b.is_zero() {
                                input.handle_division_by_zero()?;
                                return Ok(None);
                            }
                            let increment = input.div_precision_increment();
                            a.div_mysql_with_warning(b, if increment == 0 { 4 } else { increment })
                                .unwrap()
                        }
                    };
                    if warning == Some(DecimalCodecWarning::Overflow) {
                        return Err(overflow()?.into());
                    }
                    if matches!(op, NativeArithmetic::Divide)
                        && warning == Some(DecimalCodecWarning::Truncated)
                    {
                        input.handle_truncate(&format!(
                            "Truncated incorrect DECIMAL value: '{value}'"
                        ))?;
                    }
                    Ok(Some(value))
                })
                .collect::<Result<Vec<_>>>()?,
        ),
        _ => return Err(other_err!("native arithmetic result type required")),
    };
    Ok(RpnStackNode::ExpressionVector { value, field_type })
}

pub(super) fn cast_real<'a>(
    source: &RpnStackNode<'_>,
    field_type: &'a FieldType,
    rows: usize,
    input: &dyn InputColumns,
) -> Result<RpnStackNode<'a>> {
    let values = (0..rows)
        .map(|row| {
            let ScalarValueRef::Bytes(value) = source.get_logical_scalar_ref(row) else {
                return Err(other_err!("string cast requires bytes"));
            };
            let Some(value) = value else {
                return Ok(None);
            };
            let text = String::from_utf8_lossy(value);
            let converted = str_to_float(&text, true);
            if converted.event.is_some() {
                input.handle_truncate(&format!(
                    "Truncated incorrect DOUBLE value: '{}'",
                    float_warning_input(&text)
                ))?;
            }
            Ok(Some(converted.value))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RpnStackNode::ExpressionVector {
        value: ExpressionVector::Real(values),
        field_type,
    })
}
