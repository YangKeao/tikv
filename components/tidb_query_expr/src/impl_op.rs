// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::{
    codec::{Error, batch::LazyBatchColumnVec, data_type::*},
    expr::EvalContext,
};
use tipb::FieldType;

use crate::{RpnExpression, RpnStackNode, RpnStackNodeVectorValue, types::function::ControlKind};

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

/// Evaluates logical AND from left to right and only evaluates each argument
/// for rows whose result has not been determined by previous arguments.
pub fn sc_logical_and(
    ctx: &mut EvalContext,
    schema: &[FieldType],
    input_physical_columns: &LazyBatchColumnVec,
    input_logical_rows: &[usize],
    output_rows: usize,
    args: &[RpnExpression],
) -> Result<VectorValue> {
    crate::types::expr_eval::eval_logical_entry(
        ControlKind::And,
        ctx,
        schema,
        input_physical_columns,
        input_logical_rows,
        output_rows,
        args,
    )
}

/// Evaluates logical OR from left to right and only evaluates each argument for
/// rows whose result has not been determined by previous arguments.
pub fn sc_logical_or(
    ctx: &mut EvalContext,
    schema: &[FieldType],
    input_physical_columns: &LazyBatchColumnVec,
    input_logical_rows: &[usize],
    output_rows: usize,
    args: &[RpnExpression],
) -> Result<VectorValue> {
    crate::types::expr_eval::eval_logical_entry(
        ControlKind::Or,
        ctx,
        schema,
        input_physical_columns,
        input_logical_rows,
        output_rows,
        args,
    )
}

trait ScLogicalOp {
    const IDENTITY: Int;

    #[inline]
    fn normalize_value(value: Option<Int>) -> Option<Int> {
        value.map(|v| if v != 0 { 1 } else { 0 })
    }

    #[inline]
    fn is_short_circuit(value: Option<Int>) -> bool {
        matches!(
            value,
            Some(value) if value != Self::IDENTITY
        )
    }

    #[inline]
    fn handle_res_value(
        result: &mut ChunkedVecSized<Int>,
        idx: usize,
        value: Option<Int>,
        resolved_count: &mut usize,
    ) {
        match Self::normalize_value(value) {
            Some(value) if value != Self::IDENTITY => {
                // An absorbing value determines the final result even if a
                // previous argument was NULL.
                result.set(idx, Some(value));
                (*resolved_count) += 1;
            }
            // An identity value does not change the accumulated result. In
            // particular, it must not overwrite a previous NULL.
            Some(_) => {}
            None => result.set(idx, None),
        }
    }
}

struct ScLogicalAnd;

impl ScLogicalOp for ScLogicalAnd {
    const IDENTITY: Int = 1;
}

struct ScLogicalOr;

impl ScLogicalOp for ScLogicalOr {
    const IDENTITY: Int = 0;
}

fn merge_short_circuit_arg_result<Op: ScLogicalOp>(
    arg_result: RpnStackNode,
    pending_positions: Option<&[usize]>,
    pending_len: usize,
    output_rows: usize,
    result: &mut ChunkedVecSized<Int>,
) -> Result<usize> {
    let mut resolved_count = 0;
    let is_first = result.is_empty();
    match arg_result {
        RpnStackNode::Scalar { value, .. } => {
            let value = match value {
                ScalarValue::Int(_) | ScalarValue::Enum(_) => value.as_int().copied(),
                _ => {
                    return Err(other_err!(
                        "logical expression must produce Int, got {}",
                        value.eval_type()
                    ));
                }
            };
            let value = Op::normalize_value(value);
            resolved_count = if Op::is_short_circuit(value) {
                pending_len
            } else {
                0
            };
            if is_first {
                *result = ChunkedVecSized::with_capacity(output_rows);
                for _ in 0..pending_len {
                    result.push(value);
                }
            } else if resolved_count == pending_len || value.is_none() {
                for i in 0..pending_len {
                    let output_index = pending_positions.map_or(i, |positions| positions[i]);
                    result.set(output_index, value);
                }
            }
        }
        RpnStackNode::Vector {
            value: RpnStackNodeVectorValue::Generated { physical_value },
            ..
        } => {
            let vec_result = match physical_value {
                VectorValue::Int(vec_result) => vec_result,
                VectorValue::Enum(vec_result) => vec_result.as_vec_int().clone(),
                _ => {
                    return Err(other_err!(
                        "logical expression must produce Int, got {}",
                        physical_value.eval_type()
                    ));
                }
            };
            if is_first {
                *result = vec_result;

                for i in 0..pending_len {
                    let value = result.get_option_ref(i).copied();
                    let normalized = Op::normalize_value(value);

                    if normalized != value {
                        result.set(i, normalized);
                    }

                    if Op::is_short_circuit(normalized) {
                        resolved_count += 1;
                    }
                }
            } else {
                for i in 0..pending_len {
                    let output_index = pending_positions.map_or(i, |positions| positions[i]);

                    Op::handle_res_value(
                        result,
                        output_index,
                        vec_result.get_option_ref(i).copied(),
                        &mut resolved_count,
                    );
                }
            }
        }
        RpnStackNode::Vector {
            value:
                RpnStackNodeVectorValue::Ref {
                    physical_value,
                    logical_rows,
                },
            ..
        } => {
            let vec_result = match physical_value {
                VectorValue::Int(vec_result) => vec_result,
                VectorValue::Enum(vec_result) => vec_result.as_vec_int(),
                _ => {
                    return Err(other_err!(
                        "logical expression must produce Int, got {}",
                        physical_value.eval_type()
                    ));
                }
            };
            if is_first {
                *result = ChunkedVecSized::<Int>::with_capacity(output_rows);
                for i in 0..pending_len {
                    let value =
                        Op::normalize_value(vec_result.get_option_ref(logical_rows[i]).copied());
                    if Op::is_short_circuit(value) {
                        resolved_count += 1;
                    }
                    result.push(value);
                }
            } else {
                for i in 0..pending_len {
                    let output_index = pending_positions.map_or(i, |positions| positions[i]);
                    Op::handle_res_value(
                        result,
                        output_index,
                        vec_result.get_option_ref(logical_rows[i]).copied(),
                        &mut resolved_count,
                    );
                }
            }
        }
    }

    Ok(resolved_count)
}

/// Retained SQL three-valued state for the official control-frame driver.
/// Argument scheduling and row-map ownership remain in `types::expr_eval`.
#[derive(Debug)]
pub(crate) struct LogicalAccumulator {
    kind: ControlKind,
    result: ChunkedVecSized<Int>,
}

impl LogicalAccumulator {
    pub(crate) fn new(kind: ControlKind) -> Self {
        assert!(kind.is_logical(), "logical accumulator requires AND or OR");
        Self {
            kind,
            result: ChunkedVecSized::with_capacity(0),
        }
    }

    pub(crate) fn merge(
        &mut self,
        arg_result: RpnStackNode<'_>,
        pending_positions: Option<&[usize]>,
        pending_len: usize,
        output_rows: usize,
    ) -> Result<usize> {
        match self.kind {
            ControlKind::And => merge_short_circuit_arg_result::<ScLogicalAnd>(
                arg_result,
                pending_positions,
                pending_len,
                output_rows,
                &mut self.result,
            ),
            ControlKind::Or => merge_short_circuit_arg_result::<ScLogicalOr>(
                arg_result,
                pending_positions,
                pending_len,
                output_rows,
                &mut self.result,
            ),
            _ => unreachable!("logical accumulator requires AND or OR"),
        }
    }

    pub(crate) fn is_resolved(&self, index: usize) -> bool {
        let identity = match self.kind {
            ControlKind::And => ScLogicalAnd::IDENTITY,
            ControlKind::Or => ScLogicalOr::IDENTITY,
            _ => unreachable!("logical accumulator requires AND or OR"),
        };
        matches!(
            (&self.result).get_option_ref(index),
            Some(value) if *value != identity
        )
    }

    pub(crate) fn into_vector(self) -> VectorValue {
        VectorValue::Int(self.result)
    }

    /// Conservatively accounts for retained heap payload, not this struct's
    /// inline storage or allocator bookkeeping.
    pub(crate) fn retained_bytes(&self) -> usize {
        let capacity = self.result.capacity();
        let values = capacity.saturating_mul(std::mem::size_of::<Int>());
        // BitVec's public capacity reports its initialized words, not its
        // allocation. Allow for Vec's twofold word growth and small-allocation
        // floor, including capacity retained after the result is truncated.
        let bitmap_words = if capacity == 0 {
            0
        } else {
            capacity
                .div_ceil(u64::BITS as usize)
                .saturating_mul(2)
                .max(4)
        };
        values.saturating_add(bitmap_words.saturating_mul(std::mem::size_of::<u64>()))
    }

    /// Exact retained Int/bitmap element-buffer capacity for lineage
    /// accounting.
    pub(crate) fn retained_bytes_exact(&self) -> Option<usize> {
        crate::local::runtime::int_vector_storage_bytes(&self.result)
    }
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
    Ok(arg.map(|val| -val.clone()))
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
    use tidb_query_codegen::rpn_fn;
    use tidb_query_datatype::{
        FieldTypeFlag, FieldTypeTp, builder::FieldTypeBuilder, codec::mysql::TimeType,
        expr::EvalContext,
    };
    use tipb::ScalarFuncSig;

    use super::*;
    use crate::test_util::RpnFnScalarEvaluator;

    #[rpn_fn(nullable)]
    fn enum_identity(arg: Option<EnumRef>) -> Result<Option<Enum>> {
        Ok(arg.map(EnumRef::to_owned))
    }

    #[test]
    fn test_logical_accumulator_scalar_truth_tables() {
        let field_type: FieldType = FieldTypeTp::LongLong.into();
        for kind in [ControlKind::And, ControlKind::Or] {
            for lhs in [None, Some(0), Some(2)] {
                for rhs in [None, Some(0), Some(-3)] {
                    let mut accumulator = LogicalAccumulator::new(kind);
                    let lhs_value = ScalarValue::Int(lhs);
                    let resolved = accumulator
                        .merge(
                            RpnStackNode::Scalar {
                                value: &lhs_value,
                                field_type: &field_type,
                            },
                            None,
                            1,
                            1,
                        )
                        .unwrap();
                    assert_eq!(resolved, usize::from(accumulator.is_resolved(0)));
                    if !accumulator.is_resolved(0) {
                        let rhs_value = ScalarValue::Int(rhs);
                        accumulator
                            .merge(
                                RpnStackNode::Scalar {
                                    value: &rhs_value,
                                    field_type: &field_type,
                                },
                                None,
                                1,
                                1,
                            )
                            .unwrap();
                    }
                    let expected = match kind {
                        ControlKind::And => logical_and(lhs.as_ref(), rhs.as_ref()).unwrap(),
                        ControlKind::Or => logical_or(lhs.as_ref(), rhs.as_ref()).unwrap(),
                        _ => unreachable!(),
                    };
                    assert_eq!(accumulator.into_vector().to_int_vec(), vec![expected]);
                }
            }
        }
    }

    #[test]
    fn test_logical_accumulator_merges_pending_output_positions() {
        let field_type: FieldType = FieldTypeTp::LongLong.into();
        let column = VectorValue::Int(vec![Some(0), None, Some(4)].into());
        let mut accumulator = LogicalAccumulator::new(ControlKind::And);
        assert_eq!(
            accumulator
                .merge(
                    RpnStackNode::Vector {
                        value: RpnStackNodeVectorValue::Ref {
                            physical_value: &column,
                            logical_rows: &[1, 0, 1, 2],
                        },
                        field_type: &field_type,
                    },
                    None,
                    4,
                    4,
                )
                .unwrap(),
            1
        );
        assert!(!accumulator.is_resolved(0));
        assert!(accumulator.is_resolved(1));
        assert!(!accumulator.is_resolved(2));
        assert!(!accumulator.is_resolved(3));
        assert_eq!(
            accumulator
                .merge(
                    RpnStackNode::Vector {
                        value: RpnStackNodeVectorValue::Generated {
                            physical_value: VectorValue::Int(vec![Some(0), Some(7), None].into()),
                        },
                        field_type: &field_type,
                    },
                    Some(&[0, 2, 3]),
                    3,
                    4,
                )
                .unwrap(),
            1
        );
        assert!(accumulator.is_resolved(0));
        assert!(accumulator.is_resolved(1));
        assert!(!accumulator.is_resolved(2));
        assert!(!accumulator.is_resolved(3));
        assert_eq!(
            accumulator.into_vector().to_int_vec(),
            vec![Some(0), Some(0), None, None]
        );
    }

    #[test]
    fn test_logical_accumulator_accounts_for_retained_capacity() {
        assert_eq!(LogicalAccumulator::new(ControlKind::Or).retained_bytes(), 0);
        let mut result = ChunkedVecSized::<Int>::with_capacity(1_024);
        result.push(Some(1));
        result.truncate(0);
        let capacity = result.capacity();
        let accumulator = LogicalAccumulator {
            kind: ControlKind::Or,
            result,
        };
        let values = capacity * std::mem::size_of::<Int>();
        let bitmap = capacity.div_ceil(u64::BITS as usize) * std::mem::size_of::<u64>();
        assert!(accumulator.retained_bytes() >= values + bitmap);
    }

    #[test]
    fn test_logical_accumulator_exact_retained_capacity() {
        assert_eq!(
            LogicalAccumulator::new(ControlKind::Or).retained_bytes_exact(),
            Some(0)
        );
        let mut result = ChunkedVecSized::<Int>::with_capacity(65);
        result.push(None);
        result.truncate(0);
        let expected = result.capacity() * std::mem::size_of::<Int>()
            + result.get_bit_vec().retained_heap_bytes().unwrap();
        let accumulator = LogicalAccumulator {
            kind: ControlKind::Or,
            result,
        };
        assert_eq!(accumulator.retained_bytes_exact(), Some(expected));
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
    fn test_logical_short_circuit_rejects_non_int_column() {
        let args = vec![
            crate::RpnExpressionBuilder::new_for_test()
                .push_column_ref_for_test(0)
                .build_for_test(),
            crate::RpnExpressionBuilder::new_for_test()
                .push_column_ref_for_test(1)
                .build_for_test(),
        ];
        let columns = LazyBatchColumnVec::from(vec![
            VectorValue::Real(vec![Real::new(1.0).ok()].into()),
            VectorValue::Int(vec![Some(0)].into()),
        ]);
        let schema = &[FieldTypeTp::Double.into(), FieldTypeTp::LongLong.into()];

        let err = sc_logical_or(
            &mut EvalContext::default(),
            schema,
            &columns,
            &[0],
            1,
            &args,
        )
        .unwrap_err();

        assert!(err.to_string().contains("must produce Int, got Real"));
    }

    #[test]
    fn test_logical_short_circuit_rejects_non_int_generated_vector() {
        let args = vec![
            crate::RpnExpressionBuilder::new_for_test()
                .push_constant_for_test(1.0)
                .push_fn_call_for_test(unary_minus_real_fn_meta(), 1, FieldTypeTp::Double)
                .build_for_test(),
            crate::RpnExpressionBuilder::new_for_test()
                .push_constant_for_test(0_i64)
                .build_for_test(),
        ];

        let err = sc_logical_or(
            &mut EvalContext::default(),
            &[],
            &LazyBatchColumnVec::empty(),
            &[],
            1,
            &args,
        )
        .unwrap_err();

        assert!(err.to_string().contains("must produce Int, got Real"));
    }

    #[test]
    fn test_logical_short_circuit_accepts_generated_enum_vector() {
        let args = vec![
            crate::RpnExpressionBuilder::new_for_test()
                .push_constant_for_test(ScalarValue::Enum(Some(Enum::new(b"one".to_vec(), 1))))
                .push_fn_call_for_test(enum_identity_fn_meta(), 1, FieldTypeTp::Enum)
                .build_for_test(),
            crate::RpnExpressionBuilder::new_for_test()
                .push_constant_for_test(0_i64)
                .build_for_test(),
        ];

        let result = sc_logical_or(
            &mut EvalContext::default(),
            &[],
            &LazyBatchColumnVec::empty(),
            &[],
            2,
            &args,
        )
        .unwrap();

        assert_eq!(result.to_int_vec(), vec![Some(1), Some(1)]);
    }

    #[test]
    fn test_logical_short_circuit_rejects_non_int_scalar() {
        let args = vec![
            crate::RpnExpressionBuilder::new_for_test()
                .push_constant_for_test(1.0)
                .build_for_test(),
            crate::RpnExpressionBuilder::new_for_test()
                .push_constant_for_test(0_i64)
                .build_for_test(),
        ];

        let err = sc_logical_or(
            &mut EvalContext::default(),
            &[],
            &LazyBatchColumnVec::empty(),
            &[],
            1,
            &args,
        )
        .unwrap_err();

        assert!(err.to_string().contains("must produce Int, got Real"));
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
                .push_param(arg.clone())
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
                .push_param(arg.clone())
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
}
