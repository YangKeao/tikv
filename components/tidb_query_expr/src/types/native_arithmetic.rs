// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use tidb_query_common::error::{EvaluateError, Result};
use tidb_query_datatype::{EvalType, FieldTypeAccessor, codec::data_type::ScalarValueRef};
use tipb::FieldType;

use super::{
    ExpressionDecimal, ExpressionScalar, ExpressionVector, InputColumns, RpnStackNode,
    native_decimal::DecimalCodecWarning,
};

/// Native arithmetic uses full expression values; PB signatures remain
/// unchanged.
#[derive(Debug, Clone, Copy)]
pub enum NativeArithmetic {
    Add,
    Subtract,
    Multiply,
    Divide,
}

pub(super) fn is_null(value: &RpnStackNode<'_>, row: usize) -> bool {
    match value {
        RpnStackNode::ExpressionScalar { value, .. } => match value.as_ref() {
            ExpressionScalar::Real(v) => v.is_none(),
            ExpressionScalar::Decimal(v) => v.is_none(),
        },
        RpnStackNode::ExpressionVector { value, .. } => match value {
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
    if v.is_some_and(|v| !v.valid()) {
        return Err(other_err!("invalid expression decimal"));
    }
    Ok(v)
}

pub(super) fn evaluate<'a>(
    op: NativeArithmetic,
    field_type: &'a FieldType,
    overflow: &str,
    args: &[RpnStackNode<'_>],
    rows: usize,
    input: &dyn InputColumns,
) -> Result<RpnStackNode<'a>> {
    let [left, right] = args else {
        return Err(other_err!("binary arithmetic requires two operands"));
    };
    let overflow = || EvaluateError::Custom {
        code: 1690,
        msg: overflow.to_owned(),
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
                        return Err(overflow().into());
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
                        return Err(overflow().into());
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

// Native numeric-prefix scanner from TiDB364aef2 convert.rs. This expression
// conversion reports one truncation event, matching ops::bytes_to_f64.
struct NumericPrefix {
    value: String,
    truncated: bool,
}
fn valid_float_prefix(input: &str, is_function_cast: bool) -> NumericPrefix {
    if is_function_cast && input.is_empty() {
        return NumericPrefix {
            value: "0".to_owned(),
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
                    return NumericPrefix {
                        value: input[..index].to_owned(),
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
    NumericPrefix {
        value: if valid_len == 0 {
            "0".to_owned()
        } else {
            input[..valid_len].to_owned()
        },
        truncated: valid_len == 0 || valid_len != effective_len,
    }
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
            let text = text.trim();
            let prefix = valid_float_prefix(text, true);
            let (value, truncated) = match prefix.value.parse::<f64>() {
                Ok(v) if v.is_infinite() => (
                    if v.is_sign_positive() {
                        f64::MAX
                    } else {
                        -f64::MAX
                    },
                    true,
                ),
                Ok(v) => (v, prefix.truncated),
                Err(_) => (0.0, true),
            };
            if truncated {
                let subject = text.split('\0').next().unwrap_or_default();
                let mut cut = subject.len().min(128);
                while !subject.is_char_boundary(cut) {
                    cut -= 1;
                }
                input.handle_truncate(&format!(
                    "Truncated incorrect DOUBLE value: '{}'",
                    &subject[..cut]
                ))?;
            }
            Ok(Some(value))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RpnStackNode::ExpressionVector {
        value: ExpressionVector::Real(values),
        field_type,
    })
}
