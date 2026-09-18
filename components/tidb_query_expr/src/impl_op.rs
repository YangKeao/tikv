// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::any::Any;

use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::{
    codec::{Error, data_type::*},
    expr::EvalContext,
};

use crate::{LazyChildren, RpnFnCallExtra};

#[rpn_fn(nullable)]
#[inline]
pub fn logical_and(lhs: Option<&i64>, rhs: Option<&i64>) -> Result<Option<i64>> {
    Ok(match (lhs, rhs) {
        (Some(0), _) | (_, Some(0)) => Some(0),
        (None, _) | (_, None) => None,
        _ => Some(1),
    })
}

#[rpn_fn(nullable)]
#[inline]
pub fn logical_or(arg0: Option<&i64>, arg1: Option<&i64>) -> Result<Option<i64>> {
    // This is a standard Kleene OR used in SQL where
    // `null OR false == null` and `null OR true == true`
    Ok(match (arg0, arg1) {
        (Some(0), Some(0)) => Some(0),
        (None, None) | (None, Some(0)) | (Some(0), None) => None,
        _ => Some(1),
    })
}

#[rpn_fn(nullable)]
#[inline]
pub fn logical_xor(arg0: Option<&i64>, arg1: Option<&i64>) -> Result<Option<i64>> {
    // evaluates to 1 if an odd number of operands is nonzero, otherwise 0 is
    // returned.
    Ok(match (arg0, arg1) {
        (Some(arg0), Some(arg1)) => Some(((*arg0 == 0) ^ (*arg1 == 0)) as i64),
        _ => None,
    })
}

/// Reads one operand out of a dense child vector; `None` is SQL NULL.
#[inline]
fn read_int(value: &VectorValue, row: usize) -> Option<i64> {
    <Int as Evaluable>::borrow_scalar_value_ref(value.get_scalar_ref(row)).copied()
}

/// Builds the same `VectorValue::Int` the eager kernels return.
#[inline]
fn int_vector(values: Vec<Option<i64>>) -> VectorValue {
    <Int as EvaluableRet>::cast_chunk_into_vector_value(
        <<Int as EvaluableRet>::ChunkedType as ChunkedVec<Int>>::from_vec(values),
    )
}

/// Lazy three-valued `AND`.
///
/// `lhs` is evaluated for every row. Go's `builtinLogicAndSig.evalInt`
/// (`pkg/expression/builtin_op.go`) returns without touching the rhs only when
/// `lhs` is a *non-NULL zero*; a NULL lhs still evaluates the rhs because
/// `AND(NULL, FALSE)` is FALSE. Rows that skipped the rhs are merged with an
/// explicit truth table.
pub fn lazy_logical_and(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    let all_rows: Vec<usize> = (0..output_rows).collect();
    let lhs = children.eval(ctx, 0, &all_rows)?;

    let mut output = vec![None; output_rows];
    let mut need: Vec<usize> = Vec::new();
    for row in 0..output_rows {
        match read_int(&lhs, row) {
            Some(0) => output[row] = Some(0),
            _ => need.push(row),
        }
    }

    if !need.is_empty() {
        let rhs = children.eval(ctx, 1, &need)?;
        for (index, &row) in need.iter().enumerate() {
            output[row] = match (read_int(&lhs, row), read_int(&rhs, index)) {
                (_, Some(0)) => Some(0),
                (Some(_), Some(_)) => Some(1),
                _ => None,
            };
        }
    }

    Ok(int_vector(output))
}

/// Lazy three-valued `OR`.
///
/// Go's `builtinLogicOrSig.evalInt` returns without touching the rhs only when
/// `lhs` is a *non-NULL nonzero*; a NULL lhs still evaluates the rhs because
/// `OR(NULL, TRUE)` is TRUE.
pub fn lazy_logical_or(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    let all_rows: Vec<usize> = (0..output_rows).collect();
    let lhs = children.eval(ctx, 0, &all_rows)?;

    let mut output = vec![None; output_rows];
    let mut need: Vec<usize> = Vec::new();
    for row in 0..output_rows {
        match read_int(&lhs, row) {
            Some(value) if value != 0 => output[row] = Some(1),
            _ => need.push(row),
        }
    }

    if !need.is_empty() {
        let rhs = children.eval(ctx, 1, &need)?;
        for (index, &row) in need.iter().enumerate() {
            output[row] = match (read_int(&lhs, row), read_int(&rhs, index)) {
                (_, Some(value)) if value != 0 => Some(1),
                (Some(_), Some(_)) => Some(0),
                _ => None,
            };
        }
    }

    Ok(int_vector(output))
}

/// Lazy two-valued `XOR` (NULL if either operand is NULL).
///
/// Go's `builtinLogicXorSig.evalInt` returns on a NULL lhs without evaluating
/// the rhs, so this is the only logical operand that is skipped for NULL.
pub fn lazy_logical_xor(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    let all_rows: Vec<usize> = (0..output_rows).collect();
    let lhs = children.eval(ctx, 0, &all_rows)?;

    let mut output = vec![None; output_rows];
    let mut need: Vec<usize> = Vec::new();
    for row in 0..output_rows {
        if read_int(&lhs, row).is_some() {
            need.push(row);
        }
    }

    if !need.is_empty() {
        let rhs = children.eval(ctx, 1, &need)?;
        for (index, &row) in need.iter().enumerate() {
            output[row] = match (read_int(&lhs, row), read_int(&rhs, index)) {
                (Some(lhs), Some(rhs)) => Some(((lhs == 0) ^ (rhs == 0)) as i64),
                _ => None,
            };
        }
    }

    Ok(int_vector(output))
}

#[rpn_fn(nullable)]
#[inline]
pub fn unary_not_int(arg: Option<&Int>) -> Result<Option<i64>> {
    Ok(arg.map(|v| (*v == 0) as i64))
}

#[rpn_fn(nullable)]
#[inline]
pub fn unary_not_real(arg: Option<&Real>) -> Result<Option<i64>> {
    Ok(arg.map(|v| (v.into_inner() == 0f64) as i64))
}

#[rpn_fn(nullable)]
#[inline]
pub fn unary_not_decimal(arg: Option<&Decimal>) -> Result<Option<i64>> {
    Ok(arg.as_ref().map(|v| v.is_zero() as i64))
}

#[rpn_fn(nullable)]
#[inline]
pub fn unary_not_json(arg: Option<JsonRef>) -> Result<Option<i64>> {
    let json_zero = Json::from_i64(0).unwrap();
    Ok(arg.as_ref().map(|v| {
        if v == &json_zero.as_ref() {
            return 1;
        }
        0
    }))
}

#[rpn_fn(nullable)]
#[inline]
pub fn unary_minus_uint(arg: Option<&Int>) -> Result<Option<Int>> {
    use std::cmp::Ordering::*;

    match arg {
        Some(val) => {
            let uval = *val as u64;
            match uval.cmp(&(i64::MAX as u64 + 1)) {
                Greater => Err(Error::overflow("BIGINT", format!("-{}", uval)).into()),
                Equal => Ok(Some(i64::MIN)),
                Less => Ok(Some(-*val)),
            }
        }
        None => Ok(None),
    }
}

#[rpn_fn(nullable)]
#[inline]
pub fn unary_minus_int(arg: Option<&Int>) -> Result<Option<Int>> {
    match arg {
        Some(val) => {
            if *val == i64::MIN {
                Err(Error::overflow("BIGINT", format!("-{}", *val)).into())
            } else {
                Ok(Some(-*val))
            }
        }
        None => Ok(None),
    }
}

#[rpn_fn(nullable)]
#[inline]
pub fn unary_minus_real(arg: Option<&Real>) -> Result<Option<Real>> {
    Ok(arg.map(|val| -*val))
}

#[rpn_fn(nullable)]
#[inline]
pub fn unary_minus_decimal(arg: Option<&Decimal>) -> Result<Option<Decimal>> {
    Ok(arg.map(|val| -*val))
}

#[inline]
pub fn is_null_ref<'a, T: EvaluableRef<'a>>(arg: Option<T>) -> Result<Option<i64>> {
    Ok(Some(arg.is_none() as i64))
}

#[rpn_fn(nullable)]
#[inline]
pub fn is_null<T: Evaluable + EvaluableRet>(arg: Option<&T>) -> Result<Option<i64>> {
    is_null_ref(arg)
}

#[rpn_fn(nullable)]
#[inline]
pub fn is_null_bytes(arg: Option<BytesRef>) -> Result<Option<i64>> {
    is_null_ref(arg)
}

#[rpn_fn(nullable)]
#[inline]
pub fn is_null_json(arg: Option<JsonRef>) -> Result<Option<i64>> {
    is_null_ref(arg)
}

#[rpn_fn(nullable)]
#[inline]
pub fn is_null_vector_float32(arg: Option<VectorFloat32Ref>) -> Result<Option<i64>> {
    is_null_ref(arg)
}

#[rpn_fn(nullable)]
#[inline]
pub fn bit_and(lhs: Option<&Int>, rhs: Option<&Int>) -> Result<Option<Int>> {
    Ok(match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => Some(lhs & rhs),
        _ => None,
    })
}

#[rpn_fn(nullable)]
#[inline]
pub fn bit_or(lhs: Option<&Int>, rhs: Option<&Int>) -> Result<Option<Int>> {
    Ok(match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => Some(lhs | rhs),
        _ => None,
    })
}

#[rpn_fn(nullable)]
#[inline]
pub fn bit_xor(lhs: Option<&Int>, rhs: Option<&Int>) -> Result<Option<Int>> {
    Ok(match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => Some(lhs ^ rhs),
        _ => None,
    })
}

#[rpn_fn(nullable)]
#[inline]
pub fn bit_neg(arg: Option<&Int>) -> Result<Option<Int>> {
    Ok(arg.map(|arg| !arg))
}

pub trait KeepNull {
    const VALUE: bool;
}

pub struct KeepNullOn;
impl KeepNull for KeepNullOn {
    const VALUE: bool = true;
}

pub struct KeepNullOff;
impl KeepNull for KeepNullOff {
    const VALUE: bool = false;
}

#[rpn_fn(nullable)]
#[inline]
pub fn int_is_true<K: KeepNull>(arg: Option<&Int>) -> Result<Option<i64>> {
    Ok(if K::VALUE {
        arg.map(|v| (*v != 0) as i64)
    } else {
        Some(arg.map_or(0, |v| (*v != 0) as i64))
    })
}

#[rpn_fn(nullable)]
#[inline]
pub fn real_is_true<K: KeepNull>(arg: Option<&Real>) -> Result<Option<i64>> {
    Ok(if K::VALUE {
        arg.map(|v| (v.into_inner() != 0f64) as i64)
    } else {
        Some(arg.map_or(0, |v| (v.into_inner() != 0f64) as i64))
    })
}

#[rpn_fn(nullable)]
#[inline]
pub fn decimal_is_true<K: KeepNull>(arg: Option<&Decimal>) -> Result<Option<i64>> {
    Ok(if K::VALUE {
        arg.map(|v| !v.is_zero() as i64)
    } else {
        Some(arg.map_or(0, |v| !v.is_zero() as i64))
    })
}

#[rpn_fn(nullable)]
#[inline]
pub fn int_is_false<K: KeepNull>(arg: Option<&Int>) -> Result<Option<i64>> {
    Ok(if K::VALUE {
        arg.map(|v| (*v == 0) as i64)
    } else {
        Some(arg.map_or(0, |v| (*v == 0) as i64))
    })
}

#[rpn_fn(nullable)]
#[inline]
pub fn real_is_false<K: KeepNull>(arg: Option<&Real>) -> Result<Option<i64>> {
    Ok(if K::VALUE {
        arg.map(|v| (v.into_inner() == 0f64) as i64)
    } else {
        Some(arg.map_or(0, |v| (v.into_inner() == 0f64) as i64))
    })
}

#[rpn_fn(nullable)]
#[inline]
fn decimal_is_false<K: KeepNull>(arg: Option<&Decimal>) -> Result<Option<i64>> {
    Ok(if K::VALUE {
        arg.map(|v| v.is_zero() as i64)
    } else {
        Some(arg.map_or(0, |v| v.is_zero() as i64))
    })
}

#[rpn_fn(nullable)]
#[inline]
fn left_shift(lhs: Option<&Int>, rhs: Option<&Int>) -> Result<Option<Int>> {
    Ok(match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => {
            if *rhs as u64 >= 64 {
                Some(0)
            } else {
                Some((*lhs as u64).wrapping_shl(*rhs as u32) as i64)
            }
        }
        _ => None,
    })
}

#[rpn_fn(nullable)]
#[inline]
fn right_shift(lhs: Option<&Int>, rhs: Option<&Int>) -> Result<Option<Int>> {
    Ok(match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => {
            if *rhs as u64 >= 64 {
                Some(0)
            } else {
                Some((*lhs as u64).wrapping_shr(*rhs as u32) as i64)
            }
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use tidb_query_datatype::{
        EvalType, FieldTypeFlag, FieldTypeTp,
        builder::FieldTypeBuilder,
        codec::{
            batch::{LazyBatchColumn, LazyBatchColumnVec},
            mysql::TimeType,
        },
        expr::EvalContext,
    };
    use tipb::{FieldType, ScalarFuncSig};
    use tipb_helper::ExprDefBuilder;

    use super::*;
    use crate::{RpnExpression, RpnExpressionBuilder, test_util::RpnFnScalarEvaluator};

    fn decoded_int_column(values: impl IntoIterator<Item = Option<i64>>) -> LazyBatchColumn {
        let values: Vec<Option<i64>> = values.into_iter().collect();
        let mut column = LazyBatchColumn::decoded_with_capacity_and_tp(values.len(), EvalType::Int);
        for value in values {
            column.mut_decoded().push_int(value);
        }
        column
    }

    fn build_binary(sig: ScalarFuncSig, lhs: ExprDefBuilder, rhs: ExprDefBuilder) -> RpnExpression {
        RpnExpressionBuilder::build_from_expr_tree(
            ExprDefBuilder::scalar_func(sig, FieldTypeTp::LongLong)
                .push_child(lhs)
                .push_child(rhs)
                .build(),
            &mut EvalContext::default(),
            2,
        )
        .unwrap()
    }

    fn unary_minus(child: ExprDefBuilder) -> ExprDefBuilder {
        ExprDefBuilder::scalar_func(ScalarFuncSig::UnaryMinusInt, FieldTypeTp::LongLong)
            .push_child(child)
    }

    fn eval_int(
        expr: &RpnExpression,
        schema: &[FieldType],
        columns: &mut LazyBatchColumnVec,
        rows: &[usize],
    ) -> Vec<Option<i64>> {
        let mut ctx = EvalContext::default();
        expr.eval(&mut ctx, schema, columns, rows, rows.len())
            .unwrap()
            .vector_value()
            .unwrap()
            .as_ref()
            .to_int_vec()
    }

    #[test]
    fn test_logical_and() {
        let test_cases = vec![
            (Some(1), Some(1), Some(1)),
            (Some(1), Some(0), Some(0)),
            (Some(0), Some(0), Some(0)),
            (Some(2), Some(-1), Some(1)),
            (Some(0), None, Some(0)),
            (None, Some(1), None),
        ];
        for (arg0, arg1, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .push_param(arg1)
                .evaluate(ScalarFuncSig::LogicalAnd)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_logical_or() {
        let test_cases = vec![
            (Some(1), Some(1), Some(1)),
            (Some(1), Some(0), Some(1)),
            (Some(0), Some(0), Some(0)),
            (Some(2), Some(-1), Some(1)),
            (Some(1), None, Some(1)),
            (None, Some(0), None),
        ];
        for (arg0, arg1, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .push_param(arg1)
                .evaluate(ScalarFuncSig::LogicalOr)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_logical_xor() {
        let test_cases = vec![
            (Some(1), Some(1), Some(0)),
            (Some(1), Some(0), Some(1)),
            (Some(0), Some(0), Some(0)),
            (Some(2), Some(-1), Some(0)),
            (Some(-1), Some(0), Some(1)),
            (Some(0), None, None),
            (None, Some(1), None),
        ];
        for (arg0, arg1, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .push_param(arg1)
                .evaluate(ScalarFuncSig::LogicalXor)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_unary_not_int() {
        let test_cases = vec![
            (None, None),
            (0.into(), Some(1)),
            (1.into(), Some(0)),
            (2.into(), Some(0)),
            ((-1).into(), Some(0)),
        ];
        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::UnaryNotInt)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg);
        }
    }

    #[test]
    fn test_unary_not_real() {
        let test_cases = vec![
            (None, None),
            (0.0.into(), Some(1)),
            (1.0.into(), Some(0)),
            (0.3.into(), Some(0)),
        ];
        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::UnaryNotReal)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg);
        }
    }

    #[test]
    fn test_unary_not_decimal() {
        let test_cases = vec![
            (None, None),
            (Decimal::zero().into(), Some(1)),
            (Decimal::from(1).into(), Some(0)),
        ];
        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::UnaryNotDecimal)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg);
        }
    }

    #[test]
    fn test_unary_not_json() {
        let test_cases = vec![
            (None, None),
            (Some(Json::from_i64(0).unwrap()), Some(1)),
            (Some(Json::from_i64(1).unwrap()), Some(0)),
            (
                Some(Json::from_array(vec![Json::from_i64(0).unwrap()]).unwrap()),
                Some(0),
            ),
        ];
        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.clone())
                .evaluate(ScalarFuncSig::UnaryNotJson)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg.as_ref());
        }
    }

    #[test]
    fn test_unary_minus_int() {
        let unsigned_test_cases = vec![
            (None, None),
            (Some((i64::MAX as u64 + 1) as i64), Some(i64::MIN)),
            (Some(12345), Some(-12345)),
            (Some(0), Some(0)),
        ];
        for (arg, expect_output) in unsigned_test_cases {
            let field_type = FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .flag(FieldTypeFlag::UNSIGNED)
                .build();
            let output = RpnFnScalarEvaluator::new()
                .push_param_with_field_type(arg, field_type)
                .evaluate::<Int>(ScalarFuncSig::UnaryMinusInt)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg);
        }
        RpnFnScalarEvaluator::new()
            .push_param_with_field_type(
                Some((i64::MAX as u64 + 2) as i64),
                FieldTypeBuilder::new()
                    .tp(FieldTypeTp::LongLong)
                    .flag(FieldTypeFlag::UNSIGNED)
                    .build(),
            )
            .evaluate::<Int>(ScalarFuncSig::UnaryMinusInt)
            .unwrap_err();

        let signed_test_cases = vec![
            (None, None),
            (Some(i64::MAX), Some(-i64::MAX)),
            (Some(-i64::MAX), Some(i64::MAX)),
            (Some(i64::MIN + 1), Some(i64::MAX)),
            (Some(0), Some(0)),
        ];
        for (arg, expect_output) in signed_test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Int>(ScalarFuncSig::UnaryMinusInt)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg);
        }
        RpnFnScalarEvaluator::new()
            .push_param(i64::MIN)
            .evaluate::<Int>(ScalarFuncSig::UnaryMinusInt)
            .unwrap_err();
    }

    #[test]
    fn test_unary_minus_real() {
        let test_cases = vec![
            (None, None),
            (
                Some(Real::new(0.123_f64).unwrap()),
                Some(Real::new(-0.123_f64).unwrap()),
            ),
            (
                Some(Real::new(-0.123_f64).unwrap()),
                Some(Real::new(0.123_f64).unwrap()),
            ),
            (
                Some(Real::new(0.0_f64).unwrap()),
                Some(Real::new(0.0_f64).unwrap()),
            ),
            (
                Some(Real::new(f64::INFINITY).unwrap()),
                Some(Real::new(f64::NEG_INFINITY).unwrap()),
            ),
        ];
        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Real>(ScalarFuncSig::UnaryMinusReal)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg);
        }
    }

    #[test]
    fn test_unary_minus_decimal() {
        let test_cases = vec![
            (None, None),
            (Some(Decimal::zero()), Some(Decimal::zero())),
            (
                "0.123".parse::<Decimal>().ok(),
                "-0.123".parse::<Decimal>().ok(),
            ),
            (
                "-0.123".parse::<Decimal>().ok(),
                "0.123".parse::<Decimal>().ok(),
            ),
        ];
        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Decimal>(ScalarFuncSig::UnaryMinusDecimal)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg);
        }
    }

    #[test]
    fn test_is_null() {
        let test_cases = vec![
            (ScalarValue::Int(None), ScalarFuncSig::IntIsNull, Some(1)),
            (0.into(), ScalarFuncSig::IntIsNull, Some(0)),
            (ScalarValue::Real(None), ScalarFuncSig::RealIsNull, Some(1)),
            (0.0.into(), ScalarFuncSig::RealIsNull, Some(0)),
            (
                ScalarValue::Decimal(None),
                ScalarFuncSig::DecimalIsNull,
                Some(1),
            ),
            (
                Decimal::from(1).into(),
                ScalarFuncSig::DecimalIsNull,
                Some(0),
            ),
            (
                ScalarValue::Bytes(None),
                ScalarFuncSig::StringIsNull,
                Some(1),
            ),
            (vec![0u8].into(), ScalarFuncSig::StringIsNull, Some(0)),
            (
                ScalarValue::DateTime(None),
                ScalarFuncSig::TimeIsNull,
                Some(1),
            ),
            (
                DateTime::zero(&mut EvalContext::default(), 0, TimeType::DateTime)
                    .unwrap()
                    .into(),
                ScalarFuncSig::TimeIsNull,
                Some(0),
            ),
            (
                ScalarValue::Duration(None),
                ScalarFuncSig::DurationIsNull,
                Some(1),
            ),
            (
                Duration::from_nanos(1, 0).unwrap().into(),
                ScalarFuncSig::DurationIsNull,
                Some(0),
            ),
            (ScalarValue::Json(None), ScalarFuncSig::JsonIsNull, Some(1)),
            (
                Json::from_array(vec![]).unwrap().into(),
                ScalarFuncSig::JsonIsNull,
                Some(0),
            ),
        ];
        for (arg, sig, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.clone())
                .evaluate(sig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}, {:?}", arg, sig);
        }
    }

    #[test]
    fn test_bit_and() {
        let cases = vec![
            (Some(123), Some(321), Some(65)),
            (Some(-123), Some(321), Some(257)),
            (None, Some(1), None),
            (Some(1), None, None),
            (None, None, None),
        ];
        for (lhs, rhs, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::BitAndSig)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_bit_or() {
        let cases = vec![
            (Some(123), Some(321), Some(379)),
            (Some(-123), Some(321), Some(-59)),
            (None, Some(1), None),
            (Some(1), None, None),
            (None, None, None),
        ];
        for (lhs, rhs, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::BitOrSig)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_bit_xor() {
        let cases = vec![
            (Some(123), Some(321), Some(314)),
            (Some(-123), Some(321), Some(-316)),
            (None, Some(1), None),
            (Some(1), None, None),
            (None, None, None),
        ];
        for (lhs, rhs, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::BitXorSig)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_bit_neg() {
        let cases = vec![
            (Some(123), Some(-124)),
            (Some(-123), Some(122)),
            (Some(0), Some(-1)),
            (None, None),
        ];
        for (arg, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::BitNegSig)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_is_true() {
        let test_cases = vec![
            (ScalarValue::Int(None), ScalarFuncSig::IntIsTrue, Some(0)),
            (
                ScalarValue::Int(None),
                ScalarFuncSig::IntIsTrueWithNull,
                None,
            ),
            (0.into(), ScalarFuncSig::IntIsTrue, Some(0)),
            (0.into(), ScalarFuncSig::IntIsTrueWithNull, Some(0)),
            (1.into(), ScalarFuncSig::IntIsTrue, Some(1)),
            (1.into(), ScalarFuncSig::IntIsTrueWithNull, Some(1)),
            (ScalarValue::Real(None), ScalarFuncSig::RealIsTrue, Some(0)),
            (
                ScalarValue::Real(None),
                ScalarFuncSig::RealIsTrueWithNull,
                None,
            ),
            (0.0.into(), ScalarFuncSig::RealIsTrue, Some(0)),
            (0.0.into(), ScalarFuncSig::RealIsTrueWithNull, Some(0)),
            (1.0.into(), ScalarFuncSig::RealIsTrue, Some(1)),
            (1.0.into(), ScalarFuncSig::RealIsTrueWithNull, Some(1)),
            (
                ScalarValue::Decimal(None),
                ScalarFuncSig::DecimalIsTrue,
                Some(0),
            ),
            (
                ScalarValue::Decimal(None),
                ScalarFuncSig::DecimalIsTrueWithNull,
                None,
            ),
            (
                Decimal::zero().into(),
                ScalarFuncSig::DecimalIsTrue,
                Some(0),
            ),
            (
                Decimal::zero().into(),
                ScalarFuncSig::DecimalIsTrueWithNull,
                Some(0),
            ),
            (
                Decimal::from(1).into(),
                ScalarFuncSig::DecimalIsTrue,
                Some(1),
            ),
            (
                Decimal::from(1).into(),
                ScalarFuncSig::DecimalIsTrueWithNull,
                Some(1),
            ),
        ];
        for (arg, sig, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.clone())
                .evaluate(sig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}, {:?}", arg, sig);
        }
    }

    #[test]
    fn test_is_false() {
        let test_cases = vec![
            (ScalarValue::Int(None), ScalarFuncSig::IntIsFalse, Some(0)),
            (0.into(), ScalarFuncSig::IntIsFalse, Some(1)),
            (1.into(), ScalarFuncSig::IntIsFalse, Some(0)),
            (ScalarValue::Real(None), ScalarFuncSig::RealIsFalse, Some(0)),
            (0.0.into(), ScalarFuncSig::RealIsFalse, Some(1)),
            (1.0.into(), ScalarFuncSig::RealIsFalse, Some(0)),
            (
                ScalarValue::Decimal(None),
                ScalarFuncSig::DecimalIsFalse,
                Some(0),
            ),
            (
                Decimal::zero().into(),
                ScalarFuncSig::DecimalIsFalse,
                Some(1),
            ),
            (
                Decimal::from(1).into(),
                ScalarFuncSig::DecimalIsFalse,
                Some(0),
            ),
        ];
        for (arg, sig, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.clone())
                .evaluate(sig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}, {:?}", arg, sig);
        }
    }

    #[test]
    fn test_left_shift() {
        let cases = vec![
            (Some(123), Some(2), Some(492)),
            (Some(-123), Some(-1), Some(0)),
            (Some(123), Some(0), Some(123)),
            (None, Some(1), None),
            (Some(123), None, None),
            (Some(-123), Some(60), Some(5764607523034234880)),
            (None, None, None),
        ];
        for (lhs, rhs, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::LeftShift)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_right_shift() {
        let cases = vec![
            (Some(123), Some(2), Some(30)),
            (Some(-123), Some(-1), Some(0)),
            (Some(123), Some(0), Some(123)),
            (None, Some(1), None),
            (Some(123), None, None),
            (Some(-123), Some(2), Some(4611686018427387873)),
            (None, None, None),
        ];
        for (lhs, rhs, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::RightShift)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    /// Hand-computed MySQL three-valued logic over the nine `{0, 1, NULL}`
    /// operand pairs; the lazy kernels must reproduce the eager tables exactly.
    #[test]
    fn test_lazy_logical_three_valued_logic() {
        let lhs_values = [
            Some(0),
            Some(0),
            Some(0),
            Some(1),
            Some(1),
            Some(1),
            None,
            None,
            None,
        ];
        let rhs_values = [
            Some(0),
            Some(1),
            None,
            Some(0),
            Some(1),
            None,
            Some(0),
            Some(1),
            None,
        ];
        let cases = vec![
            (
                ScalarFuncSig::LogicalAnd,
                vec![
                    Some(0),
                    Some(0),
                    Some(0),
                    Some(0),
                    Some(1),
                    None,
                    Some(0),
                    None,
                    None,
                ],
            ),
            (
                ScalarFuncSig::LogicalOr,
                vec![
                    Some(0),
                    Some(1),
                    None,
                    Some(1),
                    Some(1),
                    Some(1),
                    None,
                    Some(1),
                    None,
                ],
            ),
            (
                ScalarFuncSig::LogicalXor,
                vec![
                    Some(0),
                    Some(1),
                    None,
                    Some(1),
                    Some(0),
                    None,
                    None,
                    None,
                    None,
                ],
            ),
        ];

        for (sig, expected) in cases {
            let expr = build_binary(
                sig,
                ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong),
                ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong),
            );
            let schema = [FieldTypeTp::LongLong.into(), FieldTypeTp::LongLong.into()];
            let mut columns = LazyBatchColumnVec::from(vec![
                decoded_int_column(lhs_values),
                decoded_int_column(rhs_values),
            ]);
            let rows: Vec<usize> = (0..9).collect();
            assert_eq!(
                eval_int(&expr, &schema, &mut columns, &rows),
                expected,
                "{sig:?}"
            );
        }
    }

    /// A reduced, permuted selection still maps each result position back to
    /// the right physical row of both operands.
    #[test]
    fn test_lazy_logical_respects_permuted_logical_rows() {
        let expr = build_binary(
            ScalarFuncSig::LogicalAnd,
            ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong),
            ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong),
        );
        let schema = [FieldTypeTp::LongLong.into(), FieldTypeTp::LongLong.into()];
        let mut columns = LazyBatchColumnVec::from(vec![
            decoded_int_column([Some(0), Some(1), None]),
            decoded_int_column([Some(1), Some(1), Some(1)]),
        ]);
        assert_eq!(
            eval_int(&expr, &schema, &mut columns, &[2, 0, 1]),
            [None, Some(0), Some(1)]
        );
    }

    /// `AND`: a non-NULL zero lhs skips the rhs, so `-i64::MIN` is never
    /// entered for that row; the mirror layout needs it and aborts.
    #[test]
    fn test_lazy_logical_and_skips_false_lhs() {
        let expr = build_binary(
            ScalarFuncSig::LogicalAnd,
            ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong),
            unary_minus(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong)),
        );
        let schema = [FieldTypeTp::LongLong.into(), FieldTypeTp::LongLong.into()];
        let mut columns = LazyBatchColumnVec::from(vec![
            decoded_int_column([Some(0), Some(1)]),
            decoded_int_column([Some(i64::MIN), Some(1)]),
        ]);
        assert_eq!(
            eval_int(&expr, &schema, &mut columns, &[0, 1]),
            [Some(0), Some(1)]
        );

        let mut columns = LazyBatchColumnVec::from(vec![
            decoded_int_column([Some(1), Some(0)]),
            decoded_int_column([Some(i64::MIN), Some(1)]),
        ]);
        let mut ctx = EvalContext::default();
        assert!(
            expr.eval(&mut ctx, &schema, &mut columns, &[0, 1], 2)
                .is_err()
        );
    }

    /// `OR`: a non-NULL nonzero lhs skips the rhs; the mirror layout needs it.
    #[test]
    fn test_lazy_logical_or_skips_true_lhs() {
        let expr = build_binary(
            ScalarFuncSig::LogicalOr,
            ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong),
            unary_minus(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong)),
        );
        let schema = [FieldTypeTp::LongLong.into(), FieldTypeTp::LongLong.into()];
        let mut columns = LazyBatchColumnVec::from(vec![
            decoded_int_column([Some(1), Some(0)]),
            decoded_int_column([Some(i64::MIN), Some(1)]),
        ]);
        assert_eq!(
            eval_int(&expr, &schema, &mut columns, &[0, 1]),
            [Some(1), Some(1)]
        );

        let mut columns = LazyBatchColumnVec::from(vec![
            decoded_int_column([Some(0), Some(1)]),
            decoded_int_column([Some(i64::MIN), Some(1)]),
        ]);
        let mut ctx = EvalContext::default();
        assert!(
            expr.eval(&mut ctx, &schema, &mut columns, &[0, 1], 2)
                .is_err()
        );
    }

    /// `XOR` is the one logical operand skipped for a NULL lhs (Go returns
    /// immediately), so the same child is only entered for the non-NULL row.
    #[test]
    fn test_lazy_logical_xor_skips_null_lhs() {
        let expr = build_binary(
            ScalarFuncSig::LogicalXor,
            ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong),
            unary_minus(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong)),
        );
        let schema = [FieldTypeTp::LongLong.into(), FieldTypeTp::LongLong.into()];
        let mut columns = LazyBatchColumnVec::from(vec![
            decoded_int_column([None, Some(1)]),
            decoded_int_column([Some(i64::MIN), Some(0)]),
        ]);
        assert_eq!(
            eval_int(&expr, &schema, &mut columns, &[0, 1]),
            [None, Some(1)]
        );

        let mut columns = LazyBatchColumnVec::from(vec![
            decoded_int_column([Some(0), None]),
            decoded_int_column([Some(i64::MIN), Some(0)]),
        ]);
        let mut ctx = EvalContext::default();
        assert!(
            expr.eval(&mut ctx, &schema, &mut columns, &[0, 1], 2)
                .is_err()
        );
    }
}
