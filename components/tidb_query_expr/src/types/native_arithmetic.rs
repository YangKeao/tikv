// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use tidb_datatype::{DecimalCodecWarning, float_warning_input, str_to_float};
use tidb_query_common::error::{EvaluateError, Result};
use tidb_query_datatype::{EvalType, FieldTypeAccessor, codec::data_type::ScalarValueRef};
use tipb::FieldType;

use super::{ExpressionDecimal, ExpressionScalar, ExpressionVector, InputColumns, RpnStackNode};

/// Native arithmetic uses full expression values; PB signatures remain
/// unchanged.
#[derive(Debug, Clone, Copy)]
pub enum NativeArithmetic {
    Add,
    Subtract,
    Multiply,
    Divide,
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
            ScalarValueRef::Int(None) | ScalarValueRef::Bytes(None)
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
