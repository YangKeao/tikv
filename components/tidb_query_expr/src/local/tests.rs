// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use std::sync::Arc;

use tidb_query_datatype::{
    EvalType, FieldTypeAccessor, FieldTypeFlag, FieldTypeTp,
    codec::{
        batch::{LazyBatchColumn, LazyBatchColumnVec},
        data_type::{ScalarValue, VectorValue},
    },
    expr::EvalContext,
};
use tipb::{FieldType, ScalarFuncSig};
use tipb_helper::ExprDefBuilder;

use super::*;
use crate::types::function::{CallBuild, prepare_call};

fn ft() -> FieldType {
    FieldTypeTp::LongLong.into()
}
fn constant(value: Option<i64>) -> LocalExpr {
    LocalExpr::Constant {
        value: ScalarValue::Int(value),
        field_type: ft(),
        literal_kind: LiteralKind::Typed,
    }
}
fn input(slot: usize) -> LocalExpr {
    LocalExpr::InputSlot {
        slot,
        field_type: ft(),
    }
}
fn call(function: FunctionRef, args: Vec<LocalExpr>) -> LocalExpr {
    LocalExpr::Call {
        function,
        args: args.into_boxed_slice(),
        return_type: ft(),
        metadata: CallMetadata::None,
    }
}
fn plus(lhs: LocalExpr, rhs: LocalExpr) -> LocalExpr {
    call(
        FunctionRef::TiPb(ScalarFuncSig::PlusIntSignedSigned),
        vec![lhs, rhs],
    )
}
fn columns(values: &[Option<i64>]) -> LazyBatchColumnVec {
    let mut value = VectorValue::with_capacity(values.len(), EvalType::Int);
    for item in values {
        value.push_int(*item);
    }
    LazyBatchColumnVec::from(vec![value])
}
fn compile(spec: &LocalExpr, schema: &[FieldType]) -> LocalProgram {
    compile_local(spec, schema, LocalCompileContext::default()).unwrap()
}
fn evaluate(
    program: &mut LocalProgram,
    columns: &LazyBatchColumnVec,
    physical_rows: usize,
    selection: &[usize],
) -> Vec<Option<i64>> {
    program
        .eval(
            &mut LocalEvalState::default(),
            &mut EvalContext::default(),
            LocalBatch {
                columns,
                physical_rows,
                selection,
            },
        )
        .unwrap()
        .to_int_vec()
}

#[test]
fn local_compile_eager_slice_and_rebinding() {
    let expr = plus(input(0), constant(Some(1)));
    let mut program = compile(&expr, &[ft()]);
    assert_eq!(program.return_type(), &ft());
    assert_eq!(
        evaluate(
            &mut program,
            &columns(&[Some(8), None, Some(-3)]),
            3,
            &[2, 0, 2, 1]
        ),
        vec![Some(-2), Some(9), Some(-2), None]
    );
    assert_eq!(
        evaluate(&mut program, &columns(&[Some(20)]), 1, &[0]),
        vec![Some(21)]
    );
    let mut absolute = compile(
        &call(FunctionRef::TiPb(ScalarFuncSig::AbsInt), vec![input(0)]),
        &[ft()],
    );
    assert_eq!(
        evaluate(&mut absolute, &columns(&[Some(-3), None]), 2, &[1, 0]),
        vec![None, Some(3)]
    );
}

#[test]
fn local_output_shapes_and_selections() {
    for count in [0, 1, 1024, 1025] {
        let selection: Vec<_> = (0..count).rev().collect();
        let mut scalar = compile(&constant(Some(9)), &[]);
        assert_eq!(
            evaluate(&mut scalar, &LazyBatchColumnVec::empty(), count, &selection),
            vec![Some(9); count]
        );
        let values: Vec<_> = (0..count).map(|n| Some(n as i64)).collect();
        let data = columns(&values);
        let mut identity = compile(&input(0), &[ft()]);
        assert_eq!(
            evaluate(&mut identity, &data, count, &selection),
            values.into_iter().rev().collect::<Vec<_>>()
        );
        let mut generated = compile(&plus(input(0), constant(Some(1))), &[ft()]);
        assert_eq!(
            evaluate(&mut generated, &data, count, &selection),
            (0..count)
                .rev()
                .map(|n| Some(n as i64 + 1))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn local_registered_id_without_tipb() {
    let mut program = compile(
        &call(
            FunctionRef::Local(LocalFunctionId::NullIfIntSignedSigned),
            vec![input(0), constant(Some(4))],
        ),
        &[ft()],
    );
    assert_eq!(
        evaluate(
            &mut program,
            &columns(&[Some(4), Some(3), None]),
            3,
            &[0, 1, 2, 0]
        ),
        vec![None, Some(3), None, None]
    );
}

#[test]
fn local_rejects_bad_spec_and_unadmitted_domains() {
    assert!(matches!(
        compile_local(&input(1), &[ft()], LocalCompileContext::default()),
        Err(LocalError::InvalidSpec(_))
    ));
    let mismatched = LocalExpr::Constant {
        value: ScalarValue::Bytes(Some(b"x".to_vec())),
        field_type: ft(),
        literal_kind: LiteralKind::Text,
    };
    assert!(compile_local(&mismatched, &[], LocalCompileContext::default()).is_err());
    let mut unsigned = ft();
    unsigned.as_mut_accessor().set_flag(FieldTypeFlag::UNSIGNED);
    assert!(compile_local(&input(0), &[unsigned], LocalCompileContext::default()).is_err());
    assert!(
        compile_local(
            &call(
                FunctionRef::TiPb(ScalarFuncSig::PlusIntSignedSigned),
                vec![constant(Some(1))]
            ),
            &[],
            LocalCompileContext::default()
        )
        .is_err()
    );
    // Control admission does not admit other operators or value domains.
    for sig in [
        ScalarFuncSig::LogicalXor,
        ScalarFuncSig::IfReal,
        ScalarFuncSig::InInt,
    ] {
        assert!(
            compile_local(
                &call(
                    FunctionRef::TiPb(sig),
                    vec![constant(Some(1)), constant(Some(0))]
                ),
                &[],
                LocalCompileContext::default()
            )
            .is_err()
        );
    }
    let with_metadata = LocalExpr::Call {
        function: FunctionRef::TiPb(ScalarFuncSig::AbsInt),
        args: vec![constant(Some(1))].into_boxed_slice(),
        return_type: ft(),
        metadata: CallMetadata::InUnion { in_union: true },
    };
    assert!(compile_local(&with_metadata, &[], LocalCompileContext::default()).is_err());
}

#[test]
fn local_batch_validation_precedes_execution() {
    let mut program = compile(&plus(input(0), constant(Some(i64::MAX))), &[ft()]);
    let mut ctx = EvalContext::default();
    let mut state = LocalEvalState::default();
    let data = columns(&[Some(1)]);
    for batch in [
        LocalBatch {
            columns: &data,
            physical_rows: 1,
            selection: &[1],
        },
        LocalBatch {
            columns: &data,
            physical_rows: 2,
            selection: &[0],
        },
    ] {
        assert!(matches!(
            program.eval(&mut state, &mut ctx, batch),
            Err(LocalError::InvalidBatch(_))
        ));
    }
    assert_eq!(ctx.warnings.warning_cnt, 0);
    assert!(evaluate(&mut program, &data, 1, &[]).is_empty());
    let raw = LazyBatchColumnVec::from(vec![LazyBatchColumn::raw_with_capacity(0)]);
    assert!(matches!(
        program.eval(
            &mut state,
            &mut ctx,
            LocalBatch {
                columns: &raw,
                physical_rows: 0,
                selection: &[]
            }
        ),
        Err(LocalError::InvalidBatch(_))
    ));
    let wrong = LazyBatchColumnVec::from(vec![VectorValue::with_capacity(0, EvalType::Bytes)]);
    assert!(matches!(
        program.eval(
            &mut state,
            &mut ctx,
            LocalBatch {
                columns: &wrong,
                physical_rows: 0,
                selection: &[]
            }
        ),
        Err(LocalError::InvalidBatch(_))
    ));
    assert!(matches!(
        program.eval(
            &mut state,
            &mut ctx,
            LocalBatch {
                columns: &data,
                physical_rows: 1,
                selection: &[0]
            }
        ),
        Err(LocalError::Evaluation(_))
    ));
}

#[test]
fn local_limits_are_errors_not_alternate_execution() {
    let expr = plus(constant(Some(1)), constant(Some(2)));
    for limits in [
        CompileLimits {
            max_nodes: 2,
            max_depth: 2,
        },
        CompileLimits {
            max_nodes: 3,
            max_depth: 1,
        },
    ] {
        assert!(matches!(
            compile_local(&expr, &[], LocalCompileContext { limits }),
            Err(LocalError::ResourceLimit(_))
        ));
    }
    let mut program = compile(&expr, &[]);
    assert!(matches!(
        program.eval(
            &mut LocalEvalState::new(2),
            &mut EvalContext::default(),
            LocalBatch {
                columns: &LazyBatchColumnVec::empty(),
                physical_rows: 1,
                selection: &[0]
            }
        ),
        Err(LocalError::ResourceLimit(_))
    ));
}

#[test]
fn local_bounds_descriptors_before_examining_payloads() {
    let invalid_literal = LocalExpr::Constant {
        value: ScalarValue::Bytes(Some(vec![0; 4096])),
        field_type: ft(),
        literal_kind: LiteralKind::Typed,
    };
    let expr = plus(invalid_literal, constant(Some(1)));
    let limits = CompileLimits {
        max_nodes: 1,
        max_depth: 2,
    };
    assert!(matches!(
        compile_local(&expr, &[], LocalCompileContext { limits }),
        Err(LocalError::ResourceLimit(_))
    ));
    assert!(matches!(
        compile_local(&expr, &[], LocalCompileContext::default()),
        Err(LocalError::InvalidSpec(_))
    ));
}

#[test]
fn common_preparation_preserves_wire_in_retained_order() {
    let expr = ExprDefBuilder::scalar_func(ScalarFuncSig::InInt, FieldTypeTp::LongLong)
        .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
        .push_child(ExprDefBuilder::constant_int(1))
        .push_child(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong))
        .push_child(ExprDefBuilder::constant_int(3))
        .push_child(ExprDefBuilder::column_ref(2, FieldTypeTp::LongLong))
        .build();
    let mut call = CallBuild::from_expr(&expr);
    let prepared = prepare_call(&mut call).unwrap();
    assert_eq!(prepared.retained_args(), &[0, 4, 2]);
    // Preparation is independent of the input tree's physical storage.
    assert_eq!(expr.get_children().len(), 5);
}

#[test]
fn common_selector_rejects_missing_arguments_without_indexing() {
    let expr =
        ExprDefBuilder::scalar_func(ScalarFuncSig::CastIntAsInt, FieldTypeTp::LongLong).build();
    let error = crate::map_expr_node_to_rpn_func(&expr).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Unexpected arguments: sig CastIntAsInt with 0 args")
    );
    for sig in [ScalarFuncSig::LikeSig, ScalarFuncSig::ToBinary] {
        let expr = ExprDefBuilder::scalar_func(sig, FieldTypeTp::LongLong).build();
        assert!(crate::map_expr_node_to_rpn_func(&expr).is_err());
    }
}

#[test]
fn local_worker_instances_share_only_specification() {
    fn send<T: Send>() {}
    fn sync<T: Sync>() {}
    send::<LocalProgram>();
    send::<LocalExpr>();
    sync::<LocalExpr>();
    static_assertions::assert_not_impl_any!(LocalProgram: Sync);
    let spec = Arc::new(plus(input(0), constant(Some(1))));
    let workers: Vec<_> = [10, 20]
        .into_iter()
        .map(|value| {
            let spec = Arc::clone(&spec);
            std::thread::spawn(move || {
                let mut program = compile(&spec, &[ft()]);
                evaluate(&mut program, &columns(&[Some(value)]), 1, &[0])
            })
        })
        .collect();
    let result: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(result, vec![vec![Some(11)], vec![Some(21)]]);
}
