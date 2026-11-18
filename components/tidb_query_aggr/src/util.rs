// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use tidb_query_common::Result;
use tidb_query_datatype::{EvalType, FieldTypeAccessor, FieldTypeTp, builder::FieldTypeBuilder};
use tidb_query_expr::{RpnExpression, RpnExpressionBuilder, impl_cast::get_cast_fn_rpn_node};
use tipb::{Expr, FieldType};

/// Checks whether or not there is only one child and the child expression is
/// supported.
pub fn check_aggr_exp_supported_one_child(aggr_def: &Expr) -> Result<()> {
    if aggr_def.get_children().len() != 1 {
        return Err(other_err!(
            "Expect 1 parameter, but got {}",
            aggr_def.get_children().len()
        ));
    }

    // Check whether parameter expression is supported.
    let child = &aggr_def.get_children()[0];
    RpnExpressionBuilder::check_expr_tree_supported(child)?;

    Ok(())
}

/// Rewrites the expression to insert necessary cast functions for SUM and AVG
/// aggregate functions.
///
/// See `typeInfer4Sum` and `typeInfer4Avg` in TiDB.
///
/// TODO: This logic should be performed by TiDB.
pub fn rewrite_exp_for_sum_avg(schema: &[FieldType], exp: &mut RpnExpression) -> Result<()> {
    let ret_field_type = exp.ret_field_type(schema);
    let ret_eval_type = box_try!(EvalType::try_from(ret_field_type.as_accessor().tp()));
    let new_ret_field_type = match ret_eval_type {
        EvalType::Decimal | EvalType::Real => {
            // No need to cast. Return directly without changing anything.
            return Ok(());
        }
        EvalType::Int => {
            // For type MysqlBit, the aggregation return type should be Double
            // which is also defined in in TiDB typeInfer4Sum() and typeInfer4Avg()
            if ret_field_type.tp() == FieldTypeTp::Bit {
                FieldTypeBuilder::new()
                    .tp(FieldTypeTp::Double)
                    .flen(tidb_query_datatype::MAX_REAL_WIDTH)
                    .decimal(tidb_query_datatype::UNSPECIFIED_LENGTH)
                    .build()
            } else {
                FieldTypeBuilder::new()
                    .tp(FieldTypeTp::NewDecimal)
                    .flen(tidb_query_datatype::MAX_DECIMAL_WIDTH)
                    .build()
            }
        }
        _ => FieldTypeBuilder::new()
            .tp(FieldTypeTp::Double)
            .flen(tidb_query_datatype::MAX_REAL_WIDTH)
            .decimal(tidb_query_datatype::UNSPECIFIED_LENGTH)
            .build(),
    };
    let node = get_cast_fn_rpn_node(exp.is_last_constant(), ret_field_type, new_ret_field_type)?;
    exp.push(node);
    Ok(())
}

/// Rewrites the expression to insert necessary cast functions for Bit operation
/// family functions.
pub fn rewrite_exp_for_bit_op(schema: &[FieldType], exp: &mut RpnExpression) -> Result<()> {
    let ret_field_type = exp.ret_field_type(schema);
    let ret_eval_type = box_try!(EvalType::try_from(ret_field_type.as_accessor().tp()));
    let new_ret_field_type = match ret_eval_type {
        EvalType::Int => {
            return Ok(());
        }
        _ => FieldTypeBuilder::new().tp(FieldTypeTp::LongLong).build(),
    };
    let node = get_cast_fn_rpn_node(exp.is_last_constant(), ret_field_type, new_ret_field_type)?;
    exp.push(node);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tidb_query_datatype::{
        codec::{batch::LazyBatchColumnVec, data_type::VectorValue},
        expr::{EvalConfig, EvalContext, Flag},
    };
    use tipb::ScalarFuncSig::*;
    use tipb_helper::ExprDefBuilder;

    use super::*;

    #[test]
    fn test_sum_appends_cast_to_pb_short_circuit_root() {
        let declaration = FieldTypeBuilder::new()
            .tp(FieldTypeTp::LongLong)
            .flen(19)
            .build();
        let mut expression = ExprDefBuilder::scalar_func(LogicalOr, FieldTypeTp::LongLong)
            .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
            .push_child(
                ExprDefBuilder::scalar_func(CastIntAsInt, FieldTypeTp::LongLong)
                    .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong)),
            )
            .build();
        expression.set_field_type(declaration.clone());
        let schema = [FieldTypeTp::LongLong.into()];
        let mut ctx = EvalContext::new(Arc::new(EvalConfig::from_flag(
            Flag::ENABLE_SHORT_CIRCUIT_EXPRESSION,
        )));
        let mut program =
            RpnExpressionBuilder::build_from_expr_tree(expression, &mut ctx, 1).unwrap();
        assert_eq!(program.ret_field_type(&schema), &declaration);
        assert!(!program.is_last_constant());
        assert_eq!(program.work_count(), 4);
        assert_eq!(program.column_ref_count(), 2);

        rewrite_exp_for_sum_avg(&schema, &mut program).unwrap();
        assert_eq!(
            program.ret_field_type(&schema).as_accessor().tp(),
            FieldTypeTp::NewDecimal
        );
        assert!(!program.is_last_constant());
        assert_eq!(program.work_count(), 5);
        assert_eq!(program.column_ref_count(), 2);
        let mut columns =
            LazyBatchColumnVec::from(vec![VectorValue::Int(vec![Some(9), None].into())]);
        let value = program
            .eval(&mut ctx, &schema, &mut columns, &[1, 0, 1], 3)
            .unwrap();
        assert!(value.get_logical_scalar_ref(0).as_decimal().is_none());
        assert_eq!(
            value
                .get_logical_scalar_ref(1)
                .as_decimal()
                .unwrap()
                .to_string(),
            "1"
        );
        assert!(value.get_logical_scalar_ref(2).as_decimal().is_none());
    }
}
