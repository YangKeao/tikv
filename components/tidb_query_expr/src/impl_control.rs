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

/// How a lazy control kernel reads one element of a child's dense result and
/// rebuilds an output vector of the same concrete type.
///
/// Every control kernel moves values from the child vectors it pulls into one
/// output vector, so the two ends must share a representation. `GenericElem`
/// covers every `Evaluable + EvaluableRet` type; `Bytes` and `Json` are
/// owned-result types without an `Evaluable` impl and get their own adapters.
trait LazyValue {
    /// The owned element type the kernel moves between child and output.
    type Value;

    fn read(value: &VectorValue, row: usize) -> Option<Self::Value>;
    fn build(values: Vec<Option<Self::Value>>) -> VectorValue;
}

struct GenericElem<T>(std::marker::PhantomData<T>);

impl<T: Evaluable + EvaluableRet> LazyValue for GenericElem<T> {
    type Value = T;

    #[inline]
    fn read(value: &VectorValue, row: usize) -> Option<T> {
        <T as Evaluable>::borrow_scalar_value_ref(value.get_scalar_ref(row)).cloned()
    }

    #[inline]
    fn build(values: Vec<Option<T>>) -> VectorValue {
        let chunked = <<T as EvaluableRet>::ChunkedType as ChunkedVec<T>>::from_vec(values);
        T::cast_chunk_into_vector_value(chunked)
    }
}

struct BytesElem;

impl LazyValue for BytesElem {
    type Value = Bytes;

    #[inline]
    fn read(value: &VectorValue, row: usize) -> Option<Bytes> {
        let value: Option<BytesRef> =
            EvaluableRef::borrow_scalar_value_ref(value.get_scalar_ref(row));
        value.map(|x| x.to_vec())
    }

    #[inline]
    fn build(values: Vec<Option<Bytes>>) -> VectorValue {
        VectorValue::from(ChunkedVecBytes::from_vec(values))
    }
}

struct JsonElem;

impl LazyValue for JsonElem {
    type Value = Json;

    #[inline]
    fn read(value: &VectorValue, row: usize) -> Option<Json> {
        let value: Option<JsonRef> =
            EvaluableRef::borrow_scalar_value_ref(value.get_scalar_ref(row));
        value.map(|x| x.to_owned())
    }

    #[inline]
    fn build(values: Vec<Option<Json>>) -> VectorValue {
        VectorValue::from(ChunkedVecJson::from_vec(values))
    }
}

/// A dense `rows`-element output vector of NULLs; unlike `vec![None; rows]` it
/// does not require `T: Clone`.
fn null_output<T>(rows: usize) -> Vec<Option<T>> {
    let mut output = Vec::with_capacity(rows);
    output.resize_with(rows, || None);
    output
}

/// Reads one `Int` operand (a condition or a boolean operand) out of a dense
/// child result. `None` is SQL NULL.
#[inline]
fn int_at(value: &VectorValue, row: usize) -> Option<i64> {
    <Int as Evaluable>::borrow_scalar_value_ref(value.get_scalar_ref(row)).copied()
}

/// Lazy `IFNULL(lhs, rhs)`.
///
/// `lhs` is evaluated for every row, but `rhs` is requested only for the rows
/// whose `lhs` is NULL. A row that never needs `rhs` never enters its subtree,
/// so an error, warning or RNG draw produced there is not observed. The eager
/// `if_null` kernel above stays as the fallback for the non-lazy path.
fn lazy_if_null_impl<T: LazyValue>(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
) -> Result<VectorValue> {
    let all_rows: Vec<usize> = (0..output_rows).collect();
    let lhs = children.eval(ctx, 0, &all_rows)?;

    let mut output = null_output::<T::Value>(output_rows);
    let mut rhs_rows: Vec<usize> = Vec::new();
    for row in 0..output_rows {
        let value = T::read(&lhs, row);
        if value.is_none() {
            rhs_rows.push(row);
        }
        output[row] = value;
    }

    if !rhs_rows.is_empty() {
        let rhs = children.eval(ctx, 1, &rhs_rows)?;
        for (index, &row) in rhs_rows.iter().enumerate() {
            output[row] = T::read(&rhs, index);
        }
    }

    Ok(T::build(output))
}

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
    lazy_if_null_impl::<GenericElem<T>>(ctx, output_rows, children)
}

pub fn lazy_if_null_bytes(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_if_null_impl::<BytesElem>(ctx, output_rows, children)
}

pub fn lazy_if_null_json(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_if_null_impl::<JsonElem>(ctx, output_rows, children)
}

/// Lazy `IF(condition, if_true, if_false)`.
///
/// MySQL selects the else branch for a NULL or zero condition
/// (`builtinIfIntSig.evalInt`, `pkg/expression/builtin_control.go`: only
/// `!isNull0 && arg0 != 0` takes `args[1]`), matching the eager `if_condition`
/// kernel. Each branch is entered only for the rows that select it.
fn lazy_if_impl<T: LazyValue>(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
) -> Result<VectorValue> {
    let all_rows: Vec<usize> = (0..output_rows).collect();
    let condition = children.eval(ctx, 0, &all_rows)?;

    let mut true_rows: Vec<usize> = Vec::new();
    let mut false_rows: Vec<usize> = Vec::new();
    for row in 0..output_rows {
        if int_at(&condition, row).is_some_and(|value| value != 0) {
            true_rows.push(row);
        } else {
            false_rows.push(row);
        }
    }

    let mut output = null_output::<T::Value>(output_rows);
    if !true_rows.is_empty() {
        let values = children.eval(ctx, 1, &true_rows)?;
        for (index, &row) in true_rows.iter().enumerate() {
            output[row] = T::read(&values, index);
        }
    }
    if !false_rows.is_empty() {
        let values = children.eval(ctx, 2, &false_rows)?;
        for (index, &row) in false_rows.iter().enumerate() {
            output[row] = T::read(&values, index);
        }
    }

    Ok(T::build(output))
}

pub fn lazy_if<T>(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue>
where
    T: Evaluable + EvaluableRet,
{
    lazy_if_impl::<GenericElem<T>>(ctx, output_rows, children)
}

pub fn lazy_if_bytes(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_if_impl::<BytesElem>(ctx, output_rows, children)
}

pub fn lazy_if_json(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_if_impl::<JsonElem>(ctx, output_rows, children)
}

/// Lazy `COALESCE(arg0, ..., argN)`.
///
/// The first argument is evaluated for every row; every later argument is
/// requested only for the rows still NULL at that point, and evaluation stops
/// entirely once no undecided rows remain. Go's `builtinCoalesce*Sig.eval*`
/// returns on the first non-NULL *or erroring* argument
/// (`pkg/expression/builtin_compare.go`), so an error in an argument a row
/// needs still aborts the batch.
fn lazy_coalesce_impl<T: LazyValue>(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
) -> Result<VectorValue> {
    let mut output = null_output::<T::Value>(output_rows);
    let mut active: Vec<usize> = (0..output_rows).collect();
    let mut arg = 0;
    while arg < children.len() && !active.is_empty() {
        let values = children.eval(ctx, arg, &active)?;
        let mut still: Vec<usize> = Vec::new();
        for (index, &row) in active.iter().enumerate() {
            let value = T::read(&values, index);
            if value.is_some() {
                output[row] = value;
            } else {
                still.push(row);
            }
        }
        active = still;
        arg += 1;
    }

    Ok(T::build(output))
}

pub fn lazy_coalesce<T>(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue>
where
    T: Evaluable + EvaluableRet,
{
    lazy_coalesce_impl::<GenericElem<T>>(ctx, output_rows, children)
}

pub fn lazy_coalesce_bytes(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_coalesce_impl::<BytesElem>(ctx, output_rows, children)
}

pub fn lazy_coalesce_json(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_coalesce_impl::<JsonElem>(ctx, output_rows, children)
}

/// Lazy `CASE WHEN cond0 THEN res0 [WHEN cond1 THEN res1 ...] [ELSE res]`.
///
/// Conditions are requested pair by pair over the rows still undecided; a
/// result is entered only for the rows whose condition selected it, and once
/// every row is decided no later condition or result is entered at all. A NULL
/// or zero condition is false, matching `builtinCaseWhen*Sig.eval*`
/// (`pkg/expression/builtin_control.go`). The odd trailing child, if present,
/// is the else branch and is evaluated over whatever rows remain.
fn lazy_case_when_impl<T: LazyValue>(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
) -> Result<VectorValue> {
    let len = children.len();
    let mut output = null_output::<T::Value>(output_rows);
    let mut active: Vec<usize> = (0..output_rows).collect();
    let mut pair = 0;
    while pair + 1 < len && !active.is_empty() {
        let conditions = children.eval(ctx, pair, &active)?;
        let mut chosen: Vec<usize> = Vec::new();
        let mut still: Vec<usize> = Vec::new();
        for (index, &row) in active.iter().enumerate() {
            if int_at(&conditions, index).is_some_and(|value| value != 0) {
                chosen.push(row);
            } else {
                still.push(row);
            }
        }
        if !chosen.is_empty() {
            let values = children.eval(ctx, pair + 1, &chosen)?;
            for (index, &row) in chosen.iter().enumerate() {
                output[row] = T::read(&values, index);
            }
        }
        active = still;
        pair += 2;
    }

    if len % 2 == 1 && !active.is_empty() {
        let values = children.eval(ctx, len - 1, &active)?;
        for (index, &row) in active.iter().enumerate() {
            output[row] = T::read(&values, index);
        }
    }

    Ok(T::build(output))
}

pub fn lazy_case_when<T>(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue>
where
    T: Evaluable + EvaluableRet,
{
    lazy_case_when_impl::<GenericElem<T>>(ctx, output_rows, children)
}

pub fn lazy_case_when_bytes(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_case_when_impl::<BytesElem>(ctx, output_rows, children)
}

pub fn lazy_case_when_json(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_case_when_impl::<JsonElem>(ctx, output_rows, children)
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
    use tipb::{FieldType, ScalarFuncSig};
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

    /// `-i64::MIN`, which overflows only when it is actually entered.
    fn unary_minus_min() -> ExprDefBuilder {
        ExprDefBuilder::scalar_func(ScalarFuncSig::UnaryMinusInt, FieldTypeTp::LongLong)
            .push_child(ExprDefBuilder::constant_int(i64::MIN))
    }

    fn unary_minus(child: ExprDefBuilder) -> ExprDefBuilder {
        ExprDefBuilder::scalar_func(ScalarFuncSig::UnaryMinusInt, FieldTypeTp::LongLong)
            .push_child(child)
    }

    fn build_expr(node: ExprDefBuilder, max_columns: usize) -> RpnExpression {
        RpnExpressionBuilder::build_from_expr_tree(
            node.build(),
            &mut EvalContext::default(),
            max_columns,
        )
        .unwrap()
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

    /// `IF(condition, 7, -i64::MIN)`.
    fn build_if_over_overflowing_false_branch(condition: ExprDefBuilder) -> RpnExpression {
        build_expr(
            ExprDefBuilder::scalar_func(ScalarFuncSig::IfInt, FieldTypeTp::LongLong)
                .push_child(condition)
                .push_child(ExprDefBuilder::constant_int(7))
                .push_child(unary_minus_min()),
            0,
        )
    }

    #[test]
    fn test_lazy_if_skips_unneeded_branch() {
        let expr = build_if_over_overflowing_false_branch(ExprDefBuilder::constant_int(1));
        let mut columns = LazyBatchColumnVec::empty();
        assert_eq!(eval_int(&expr, &[], &mut columns, &[0]), [Some(7)]);
    }

    #[test]
    fn test_lazy_if_needed_branch_still_errors() {
        let expr = build_if_over_overflowing_false_branch(ExprDefBuilder::constant_int(0));
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        assert!(
            expr.eval(&mut ctx, &[], &mut columns, &[0], 1).is_err(),
            "the selected branch must abort the batch"
        );
    }

    /// `IF(Col0, Col1, -Col2)` selects one branch per row. Rows 0 and 3 take
    /// the true branch, so the `i64::MIN` in `Col2` is never negated; rows
    /// 1 and 2 (zero and NULL condition) take the false branch.
    #[test]
    fn test_lazy_if_selects_branch_per_row() {
        let expr = build_expr(
            ExprDefBuilder::scalar_func(ScalarFuncSig::IfInt, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
                .push_child(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong))
                .push_child(unary_minus(ExprDefBuilder::column_ref(
                    2,
                    FieldTypeTp::LongLong,
                ))),
            3,
        );
        let schema = [
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
        ];
        let mut columns = LazyBatchColumnVec::from(vec![
            decoded_int_column([Some(1), Some(0), None, Some(2)]),
            decoded_int_column([Some(10), Some(20), Some(30), Some(40)]),
            decoded_int_column([Some(i64::MIN), Some(5), Some(7), Some(i64::MIN)]),
        ]);
        assert_eq!(
            eval_int(&expr, &schema, &mut columns, &[0, 1, 2, 3]),
            [Some(10), Some(-5), Some(-7), Some(40)]
        );
    }

    /// The mirror image: when a row that selects the false branch overflows,
    /// the whole batch fails even though the true-branch rows never entered
    /// it.
    #[test]
    fn test_lazy_if_errors_when_a_needed_row_fails() {
        let expr = build_expr(
            ExprDefBuilder::scalar_func(ScalarFuncSig::IfInt, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
                .push_child(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong))
                .push_child(unary_minus(ExprDefBuilder::column_ref(
                    2,
                    FieldTypeTp::LongLong,
                ))),
            3,
        );
        let schema = [
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
        ];
        let mut columns = LazyBatchColumnVec::from(vec![
            decoded_int_column([Some(1), Some(0)]),
            decoded_int_column([Some(5), Some(5)]),
            decoded_int_column([Some(5), Some(i64::MIN)]),
        ]);
        let mut ctx = EvalContext::default();
        assert!(
            expr.eval(&mut ctx, &schema, &mut columns, &[0, 1], 2)
                .is_err()
        );
    }

    /// A skipped branch records no warning. `CastStringAsTime("not-a-time")`
    /// yields NULL plus a truncation warning in the default (non-strict)
    /// context, so its presence is observable even though the value is NULL.
    #[test]
    fn test_lazy_if_warns_only_for_taken_branch() {
        fn cast_string_as_time(value: &str) -> ExprDefBuilder {
            ExprDefBuilder::scalar_func(ScalarFuncSig::CastStringAsTime, FieldTypeTp::DateTime)
                .push_child(ExprDefBuilder::constant_bytes(value.as_bytes().to_vec()))
        }
        fn build(condition: ExprDefBuilder) -> RpnExpression {
            build_expr(
                ExprDefBuilder::scalar_func(ScalarFuncSig::IfTime, FieldTypeTp::DateTime)
                    .push_child(condition)
                    .push_child(cast_string_as_time("2024-01-01"))
                    .push_child(cast_string_as_time("not-a-time")),
                0,
            )
        }

        let expr = build(ExprDefBuilder::constant_int(1));
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        expr.eval(&mut ctx, &[], &mut columns, &[0], 1).unwrap();
        assert_eq!(ctx.warnings.warning_cnt, 0, "skipped branch must not warn");

        let expr = build(ExprDefBuilder::constant_int(0));
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        expr.eval(&mut ctx, &[], &mut columns, &[0], 1).unwrap();
        assert_eq!(
            ctx.warnings.warning_cnt, 1,
            "the taken branch must still warn"
        );
    }

    /// The newly lazy `IfNull*` families that are not `Int` use the `BytesElem`
    /// adapters; check them against the eager result shape.
    #[test]
    fn test_lazy_if_null_other_families() {
        let real_cases = vec![
            (ScalarValue::Real(None), ScalarValue::Real(None), None),
            (
                ScalarValue::Real(None),
                ScalarValue::Real(Some(Real::new(1.5).unwrap())),
                Some(Real::new(1.5).unwrap()),
            ),
            (
                ScalarValue::Real(Some(Real::new(2.5).unwrap())),
                ScalarValue::Real(None),
                Some(Real::new(2.5).unwrap()),
            ),
        ];
        for (lhs, rhs, expected) in real_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate::<Real>(ScalarFuncSig::IfNullReal)
                .unwrap();
            assert_eq!(output, expected);
        }

        let bytes_cases = vec![
            (ScalarValue::Bytes(None), ScalarValue::Bytes(None), None),
            (
                ScalarValue::Bytes(None),
                ScalarValue::Bytes(Some(vec![3, 4])),
                Some(vec![3, 4]),
            ),
            (
                ScalarValue::Bytes(Some(vec![1, 2])),
                ScalarValue::Bytes(Some(vec![3, 4])),
                Some(vec![1, 2]),
            ),
        ];
        for (lhs, rhs, expected) in bytes_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate::<Bytes>(ScalarFuncSig::IfNullString)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    /// `COALESCE(1, -i64::MIN)` never enters the second argument.
    #[test]
    fn test_lazy_coalesce_skips_unneeded_args() {
        let expr = build_expr(
            ExprDefBuilder::scalar_func(ScalarFuncSig::CoalesceInt, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::constant_int(1))
                .push_child(unary_minus_min()),
            0,
        );
        let mut columns = LazyBatchColumnVec::empty();
        assert_eq!(eval_int(&expr, &[], &mut columns, &[0]), [Some(1)]);
    }

    /// `COALESCE(NULL, NULL, -i64::MIN)` reaches the third argument, so the
    /// overflow aborts the batch.
    #[test]
    fn test_lazy_coalesce_needed_arg_still_errors() {
        let expr = build_expr(
            ExprDefBuilder::scalar_func(ScalarFuncSig::CoalesceInt, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::constant_null(FieldTypeTp::LongLong))
                .push_child(ExprDefBuilder::constant_null(FieldTypeTp::LongLong))
                .push_child(unary_minus_min()),
            0,
        );
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        assert!(expr.eval(&mut ctx, &[], &mut columns, &[0], 1).is_err());
    }

    /// `COALESCE(Col0, Col1, -Col2)`: each row stops at its first non-NULL
    /// argument, so `i64::MIN` in `Col2` is only negated for the row that
    /// actually reaches it.
    #[test]
    fn test_lazy_coalesce_selects_per_row() {
        let expr = build_expr(
            ExprDefBuilder::scalar_func(ScalarFuncSig::CoalesceInt, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
                .push_child(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong))
                .push_child(unary_minus(ExprDefBuilder::column_ref(
                    2,
                    FieldTypeTp::LongLong,
                ))),
            3,
        );
        let schema = [
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
        ];
        let mut columns = LazyBatchColumnVec::from(vec![
            decoded_int_column([Some(1), None, None, Some(4)]),
            decoded_int_column([None, Some(2), None, None]),
            decoded_int_column([Some(i64::MIN), Some(i64::MIN), Some(7), Some(i64::MIN)]),
        ]);
        assert_eq!(
            eval_int(&expr, &schema, &mut columns, &[0, 1, 2, 3]),
            [Some(1), Some(2), Some(-7), Some(4)]
        );
    }

    /// `CASE WHEN 1 THEN 7 WHEN -i64::MIN THEN 9 END`: the second condition is
    /// never entered once every row is decided by the first pair.
    #[test]
    fn test_lazy_case_when_skips_undecided_pairs() {
        let expr = build_expr(
            ExprDefBuilder::scalar_func(ScalarFuncSig::CaseWhenInt, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::constant_int(1))
                .push_child(ExprDefBuilder::constant_int(7))
                .push_child(unary_minus_min())
                .push_child(ExprDefBuilder::constant_int(9)),
            0,
        );
        let mut columns = LazyBatchColumnVec::empty();
        assert_eq!(eval_int(&expr, &[], &mut columns, &[0]), [Some(7)]);
    }

    /// The same shape with a false first condition has to enter the second
    /// condition, which overflows.
    #[test]
    fn test_lazy_case_when_needed_pair_still_errors() {
        let expr = build_expr(
            ExprDefBuilder::scalar_func(ScalarFuncSig::CaseWhenInt, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::constant_int(0))
                .push_child(ExprDefBuilder::constant_int(7))
                .push_child(unary_minus_min())
                .push_child(ExprDefBuilder::constant_int(9)),
            0,
        );
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        assert!(expr.eval(&mut ctx, &[], &mut columns, &[0], 1).is_err());
    }

    /// The else arm is entered only for rows no `WHEN` selected.
    #[test]
    fn test_lazy_case_when_else_only_for_undecided_rows() {
        let taken = build_expr(
            ExprDefBuilder::scalar_func(ScalarFuncSig::CaseWhenInt, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::constant_int(1))
                .push_child(ExprDefBuilder::constant_int(7))
                .push_child(unary_minus_min()),
            0,
        );
        let mut columns = LazyBatchColumnVec::empty();
        assert_eq!(eval_int(&taken, &[], &mut columns, &[0]), [Some(7)]);

        let needed = build_expr(
            ExprDefBuilder::scalar_func(ScalarFuncSig::CaseWhenInt, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::constant_int(0))
                .push_child(ExprDefBuilder::constant_int(7))
                .push_child(unary_minus_min()),
            0,
        );
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        assert!(
            needed.eval(&mut ctx, &[], &mut columns, &[0], 1).is_err(),
            "the else arm is needed when nothing matches"
        );
    }

    /// `CASE WHEN Col0 THEN Col1 WHEN Col2 THEN Col3 ELSE -Col4 END` chooses a
    /// different pair per row; rows 0 and 1 never enter the else, so the
    /// `i64::MIN` values they do not need are never negated.
    #[test]
    fn test_lazy_case_when_selects_per_row() {
        let expr = build_expr(
            ExprDefBuilder::scalar_func(ScalarFuncSig::CaseWhenInt, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
                .push_child(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong))
                .push_child(ExprDefBuilder::column_ref(2, FieldTypeTp::LongLong))
                .push_child(ExprDefBuilder::column_ref(3, FieldTypeTp::LongLong))
                .push_child(unary_minus(ExprDefBuilder::column_ref(
                    4,
                    FieldTypeTp::LongLong,
                ))),
            5,
        );
        let schema = [
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
        ];
        let mut columns = LazyBatchColumnVec::from(vec![
            decoded_int_column([Some(1), Some(0), Some(0), None]),
            decoded_int_column([Some(10), Some(20), Some(30), Some(40)]),
            decoded_int_column([Some(0), Some(1), Some(0), Some(0)]),
            decoded_int_column([Some(50), Some(60), Some(70), Some(80)]),
            decoded_int_column([Some(i64::MIN), Some(i64::MIN), Some(5), Some(6)]),
        ]);
        assert_eq!(
            eval_int(&expr, &schema, &mut columns, &[0, 1, 2, 3]),
            [Some(10), Some(60), Some(-5), Some(-6)]
        );
    }
}
