// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::any::Any;

use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::{codec::data_type::*, expr::EvalContext};

use crate::{LazyChildren, RpnFnCallExtra};

#[rpn_fn(nullable)]
#[inline]
fn if_null<T: Evaluable + EvaluableRet>(lhs: Option<&T>, rhs: Option<&T>) -> Result<Option<T>> {
    if lhs.is_some() {
        return Ok(lhs.cloned());
    }
    Ok(rhs.cloned())
}

/// Lazy `IFNULL(lhs, rhs)`.
///
/// `lhs` is evaluated for every row, but `rhs` is requested only for the rows
/// whose `lhs` is NULL. A row that never needs `rhs` never enters its subtree,
/// so an error, warning or RNG draw produced there is not observed. The eager
/// `if_null` kernel above stays as the fallback for the non-lazy path.
pub fn lazy_if_null<T>(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue>
where
    T: Evaluable + EvaluableRet,
{
    let all_rows: Vec<usize> = (0..output_rows).collect();
    let lhs = children.eval(ctx, 0, &all_rows)?;

    let mut output: Vec<Option<T>> = Vec::with_capacity(output_rows);
    let mut rhs_rows: Vec<usize> = Vec::new();
    for row in 0..output_rows {
        let value = <T as Evaluable>::borrow_scalar_value_ref(lhs.get_scalar_ref(row)).cloned();
        if value.is_none() {
            rhs_rows.push(row);
        }
        output.push(value);
    }

    if !rhs_rows.is_empty() {
        let rhs = children.eval(ctx, 1, &rhs_rows)?;
        for (index, &row) in rhs_rows.iter().enumerate() {
            output[row] =
                <T as Evaluable>::borrow_scalar_value_ref(rhs.get_scalar_ref(index)).cloned();
        }
    }

    let chunked = <<T as EvaluableRet>::ChunkedType as ChunkedVec<T>>::from_vec(output);
    Ok(T::cast_chunk_into_vector_value(chunked))
}

#[rpn_fn(nullable)]
#[inline]
fn if_null_json(lhs: Option<JsonRef>, rhs: Option<JsonRef>) -> Result<Option<Json>> {
    if lhs.is_some() {
        return Ok(lhs.map(|x| x.to_owned()));
    }
    Ok(rhs.map(|x| x.to_owned()))
}

#[rpn_fn(nullable)]
#[inline]
fn if_null_bytes(lhs: Option<BytesRef>, rhs: Option<BytesRef>) -> Result<Option<Bytes>> {
    if lhs.is_some() {
        return Ok(lhs.map(|x| x.to_vec()));
    }
    Ok(rhs.map(|x| x.to_vec()))
}

#[rpn_fn(nullable, raw_varg, extra_validator = case_when_validator::<T>)]
#[inline]
pub fn case_when<T: Evaluable + EvaluableRet>(args: &[ScalarValueRef<'_>]) -> Result<Option<T>> {
    for chunk in args.chunks(2) {
        if chunk.len() == 1 {
            // Else statement
            let ret: Option<&T> = Evaluable::borrow_scalar_value_ref(chunk[0]);
            return Ok(ret.cloned());
        }
        let cond: Option<&Int> = Evaluable::borrow_scalar_value_ref(chunk[0]);
        if cond.cloned().unwrap_or(0) != 0 {
            let ret: Option<&T> = Evaluable::borrow_scalar_value_ref(chunk[1]);
            return Ok(ret.cloned());
        }
    }
    Ok(None)
}

#[rpn_fn(nullable, raw_varg, extra_validator = case_when_validator::<Bytes>)]
#[inline]
pub fn case_when_bytes(args: &[ScalarValueRef<'_>]) -> Result<Option<Bytes>> {
    for chunk in args.chunks(2) {
        if chunk.len() == 1 {
            // Else statement
            let ret: Option<BytesRef> = EvaluableRef::borrow_scalar_value_ref(chunk[0]);
            return Ok(ret.map(|x| x.to_vec()));
        }
        let cond: Option<&Int> = Evaluable::borrow_scalar_value_ref(chunk[0]);
        if cond.cloned().unwrap_or(0) != 0 {
            let ret: Option<BytesRef> = EvaluableRef::borrow_scalar_value_ref(chunk[1]);
            return Ok(ret.map(|x| x.to_vec()));
        }
    }
    Ok(None)
}

#[rpn_fn(nullable, raw_varg, extra_validator = case_when_validator::<Json>)]
#[inline]
pub fn case_when_json(args: &[ScalarValueRef<'_>]) -> Result<Option<Json>> {
    for chunk in args.chunks(2) {
        if chunk.len() == 1 {
            // Else statement
            let ret: Option<JsonRef> = EvaluableRef::borrow_scalar_value_ref(chunk[0]);
            return Ok(ret.map(|x| x.to_owned()));
        }
        let cond: Option<&Int> = Evaluable::borrow_scalar_value_ref(chunk[0]);
        if cond.cloned().unwrap_or(0) != 0 {
            let ret: Option<JsonRef> = EvaluableRef::borrow_scalar_value_ref(chunk[1]);
            return Ok(ret.map(|x| x.to_owned()));
        }
    }
    Ok(None)
}

#[rpn_fn(nullable)]
#[inline]
fn if_condition<T: Evaluable + EvaluableRet>(
    condition: Option<&Int>,
    value_if_true: Option<&T>,
    value_if_false: Option<&T>,
) -> Result<Option<T>> {
    Ok(if condition.cloned().unwrap_or(0) != 0 {
        value_if_true.cloned()
    } else {
        value_if_false.cloned()
    })
}

#[rpn_fn(nullable)]
#[inline]
fn if_condition_json(
    condition: Option<&Int>,
    value_if_true: Option<JsonRef>,
    value_if_false: Option<JsonRef>,
) -> Result<Option<Json>> {
    Ok(if condition.cloned().unwrap_or(0) != 0 {
        value_if_true.map(|x| x.to_owned())
    } else {
        value_if_false.map(|x| x.to_owned())
    })
}

#[rpn_fn(nullable)]
#[inline]
fn if_condition_bytes(
    condition: Option<&Int>,
    value_if_true: Option<BytesRef>,
    value_if_false: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    Ok(if condition.cloned().unwrap_or(0) != 0 {
        value_if_true.map(|x| x.to_vec())
    } else {
        value_if_false.map(|x| x.to_vec())
    })
}

fn case_when_validator<T: EvaluableRet>(expr: &tipb::Expr) -> Result<()> {
    for chunk in expr.get_children().chunks(2) {
        if chunk.len() == 1 {
            super::function::validate_expr_return_type(&chunk[0], T::EVAL_TYPE)?;
        } else {
            super::function::validate_expr_return_type(&chunk[0], <Int as Evaluable>::EVAL_TYPE)?;
            super::function::validate_expr_return_type(&chunk[1], T::EVAL_TYPE)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tidb_query_datatype::{
        EvalType, FieldTypeTp,
        codec::batch::{LazyBatchColumn, LazyBatchColumnVec},
    };
    use tipb::ScalarFuncSig;
    use tipb_helper::ExprDefBuilder;

    use super::*;
    use crate::{RpnExpression, RpnExpressionBuilder, test_util::RpnFnScalarEvaluator};

    #[test]
    fn test_if_null() {
        let cases = vec![
            (None, None, None),
            (None, Some(1), Some(1)),
            (Some(2), None, Some(2)),
            (Some(2), Some(1), Some(2)),
        ];
        for (lhs, rhs, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::IfNullInt)
                .unwrap();
            assert_eq!(output, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
        }
    }

    /// `IFNULL(lhs, UnaryMinusInt(i64::MIN))`. The rhs is a node that fails
    /// only when it is actually entered (`unary_minus_int` returns an
    /// overflow error for `i64::MIN`).
    fn build_if_null_over_overflowing_rhs(lhs: ExprDefBuilder) -> RpnExpression {
        let node = ExprDefBuilder::scalar_func(ScalarFuncSig::IfNullInt, FieldTypeTp::LongLong)
            .push_child(lhs)
            .push_child(
                ExprDefBuilder::scalar_func(ScalarFuncSig::UnaryMinusInt, FieldTypeTp::LongLong)
                    .push_child(ExprDefBuilder::constant_int(i64::MIN)),
            )
            .build();
        RpnExpressionBuilder::build_from_expr_tree(node, &mut EvalContext::default(), 0).unwrap()
    }

    /// `IFNULL(Col0, UnaryMinusInt(Col1))`.
    fn build_if_null_over_columns(max_columns: usize) -> RpnExpression {
        let node = ExprDefBuilder::scalar_func(ScalarFuncSig::IfNullInt, FieldTypeTp::LongLong)
            .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
            .push_child(
                ExprDefBuilder::scalar_func(ScalarFuncSig::UnaryMinusInt, FieldTypeTp::LongLong)
                    .push_child(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong)),
            )
            .build();
        RpnExpressionBuilder::build_from_expr_tree(node, &mut EvalContext::default(), max_columns)
            .unwrap()
    }

    fn decoded_int_column(values: impl IntoIterator<Item = Option<i64>>) -> LazyBatchColumn {
        let values: Vec<Option<i64>> = values.into_iter().collect();
        let mut column = LazyBatchColumn::decoded_with_capacity_and_tp(values.len(), EvalType::Int);
        for value in values {
            column.mut_decoded().push_int(value);
        }
        column
    }

    /// A non-NULL lhs must short-circuit: the rhs subtree is never entered, so
    /// its overflow error is never raised.
    #[test]
    fn test_lazy_if_null_skips_unneeded_child() {
        let expr = build_if_null_over_overflowing_rhs(ExprDefBuilder::constant_int(1));
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        let result = expr.eval(&mut ctx, &[], &mut columns, &[0], 1).unwrap();
        assert_eq!(
            result.vector_value().unwrap().as_ref().to_int_vec(),
            [Some(1)]
        );
    }

    /// A NULL lhs needs the rhs, so the same overflow error aborts the batch.
    #[test]
    fn test_lazy_if_null_needed_child_still_errors() {
        let expr = build_if_null_over_overflowing_rhs(ExprDefBuilder::constant_null(
            FieldTypeTp::LongLong,
        ));
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        let error = expr
            .eval(&mut ctx, &[], &mut columns, &[0], 1)
            .expect_err("the needed rhs must abort the batch");
        assert!(
            error.to_string().contains("out of range"),
            "unexpected error: {error}"
        );
    }

    /// Row selection is per row: `UnaryMinusInt` runs only for the NULL-lhs
    /// row, so the `i64::MIN` values in the other rows are never negated.
    #[test]
    fn test_lazy_if_null_evaluates_only_rows_that_need_rhs() {
        let expr = build_if_null_over_columns(2);
        let mut columns = LazyBatchColumnVec::from(vec![
            decoded_int_column([Some(1), None, Some(2)]),
            decoded_int_column([Some(i64::MIN), Some(5), Some(i64::MIN)]),
        ]);
        let schema = [FieldTypeTp::LongLong.into(), FieldTypeTp::LongLong.into()];
        let mut ctx = EvalContext::default();
        let result = expr
            .eval(&mut ctx, &schema, &mut columns, &[0, 1, 2], 3)
            .unwrap();
        assert_eq!(
            result.vector_value().unwrap().as_ref().to_int_vec(),
            [Some(1), Some(-5), Some(2)]
        );
    }

    /// The mirror image: when a row that *does* need the rhs overflows, the
    /// whole batch fails even though other rows would not have entered it.
    #[test]
    fn test_lazy_if_null_errors_when_a_needed_row_fails() {
        let expr = build_if_null_over_columns(2);
        let mut columns = LazyBatchColumnVec::from(vec![
            decoded_int_column([Some(1), None, Some(2)]),
            decoded_int_column([Some(5), Some(i64::MIN), Some(5)]),
        ]);
        let schema = [FieldTypeTp::LongLong.into(), FieldTypeTp::LongLong.into()];
        let mut ctx = EvalContext::default();
        assert!(
            expr.eval(&mut ctx, &schema, &mut columns, &[0, 1, 2], 3)
                .is_err()
        );
    }

    /// A lazy meta must keep `borrowed_fn_ptr: None`; otherwise the borrowed
    /// facade would eagerly evaluate the children it cannot schedule. Note that
    /// `with_lazy` is applied at dispatch, so the generated
    /// `if_null_fn_meta()` stays lazy-free while the node the evaluator sees is
    /// not.
    #[test]
    fn test_lazy_if_null_keeps_borrowed_path_disabled() {
        let expr = build_if_null_over_overflowing_rhs(ExprDefBuilder::constant_int(1));
        let meta = expr.last().unwrap().fn_call_func();
        assert!(
            meta.lazy_fn_ptr.is_some(),
            "IfNullInt must be dispatched with a lazy kernel"
        );
        assert!(meta.borrowed_fn_ptr.is_none());
    }

    #[test]
    fn test_case_when() {
        let cases: Vec<(Vec<ScalarValue>, Option<Real>)> = vec![
            (
                vec![1.into(), (3.0).into(), 1.into(), (5.0).into()],
                Real::new(3.0).ok(),
            ),
            (
                vec![0.into(), (3.0).into(), 1.into(), (5.0).into()],
                Real::new(5.0).ok(),
            ),
            (
                vec![ScalarValue::Int(None), (2.0).into(), 1.into(), (6.0).into()],
                Real::new(6.0).ok(),
            ),
            (vec![(7.0).into()], Real::new(7.0).ok()),
            (vec![0.into(), ScalarValue::Real(None)], None),
            (vec![1.into(), ScalarValue::Real(None)], None),
            (vec![1.into(), (3.5).into()], Real::new(3.5).ok()),
            (vec![2.into(), (3.5).into()], Real::new(3.5).ok()),
            (
                vec![
                    0.into(),
                    ScalarValue::Real(None),
                    ScalarValue::Int(None),
                    ScalarValue::Real(None),
                    (5.5).into(),
                ],
                Real::new(5.5).ok(),
            ),
        ];

        for (args, expected) in cases {
            let mut evaluator = RpnFnScalarEvaluator::new();
            for arg in args {
                evaluator = evaluator.push_param(arg);
            }
            let output = evaluator.evaluate(ScalarFuncSig::CaseWhenReal).unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_case_when_bytes() {
        let cases: Vec<(Vec<ScalarValue>, Option<Bytes>)> = vec![
            (
                vec![
                    1.into(),
                    vec![1, 2, 3].into(),
                    1.into(),
                    vec![4, 5, 6].into(),
                ],
                Some(vec![1, 2, 3]),
            ),
            (
                vec![
                    0.into(),
                    vec![1, 2, 3].into(),
                    1.into(),
                    vec![4, 5, 6].into(),
                ],
                Some(vec![4, 5, 6]),
            ),
        ];

        for (args, expected) in cases {
            let mut evaluator = RpnFnScalarEvaluator::new();
            for arg in args {
                evaluator = evaluator.push_param(arg);
            }
            let output = evaluator.evaluate(ScalarFuncSig::CaseWhenString).unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_if() {
        use std::f64::consts::{E, PI};

        let cases = vec![
            ((Some(0), E, PI), Real::new(PI).ok()),
            ((Some(1), E, PI), Real::new(E).ok()),
            ((None, E, PI), Real::new(PI).ok()),
        ];

        for ((condition, value1, value2), expected) in cases {
            assert_eq!(
                expected,
                RpnFnScalarEvaluator::new()
                    .push_param(condition)
                    .push_param(value1)
                    .push_param(value2)
                    .evaluate(ScalarFuncSig::IfReal)
                    .unwrap()
            );
        }
    }
}
