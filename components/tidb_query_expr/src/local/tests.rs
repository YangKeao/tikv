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

#[test]
fn local_evaluated_bytes_integer_operations_reuse_worker() {
    let cases: &[(Option<&[u8]>, [Option<i64>; 3])] = &[
        (None, [None, None, None]),
        (Some(b""), [Some(0), Some(0), Some(0)]),
        (Some(b"\0x"), [Some(0), Some(2), Some(16)]),
        (Some(b"\xff\0a"), [Some(255), Some(3), Some(24)]),
        (Some("é".as_bytes()), [Some(195), Some(2), Some(16)]),
    ];
    for (column, operation) in [
        EvaluatedBytesOp::Ascii,
        EvaluatedBytesOp::Length,
        EvaluatedBytesOp::BitLength,
    ]
    .into_iter()
    .enumerate()
    {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        for (index, &(input, expected)) in cases.iter().enumerate() {
            let ComputedValue::Int(value) =
                worker.eval_one(input.map(|bytes| bytes.to_vec())).unwrap()
            else {
                panic!("integer operation returned Bytes");
            };
            assert_eq!(value.value(), expected[column]);
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), expected[column]);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[test]
fn local_evaluated_bytes_trim_only_spaces() {
    let cases: &[(Option<&[u8]>, [Option<&[u8]>; 2])] = &[
        (None, [None, None]),
        (Some(b""), [Some(b""), Some(b"")]),
        (Some(b"   "), [Some(b""), Some(b"")]),
        (
            Some(b"  \t\r\n\0\xff\n\r\t  "),
            [Some(b"\t\r\n\0\xff\n\r\t  "), Some(b"  \t\r\n\0\xff\n\r\t")],
        ),
        (Some(b"  a b  "), [Some(b"a b  "), Some(b"  a b")]),
    ];
    for (column, operation) in [EvaluatedBytesOp::LTrim, EvaluatedBytesOp::RTrim]
        .into_iter()
        .enumerate()
    {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        for (index, &(input, expected)) in cases.iter().enumerate() {
            let ComputedValue::Bytes(value) =
                worker.eval_one(input.map(|bytes| bytes.to_vec())).unwrap()
            else {
                panic!("trim operation returned Int");
            };
            assert_eq!(value.value(), expected[column]);
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(
                value.into_option(),
                expected[column].map(|bytes| bytes.to_vec())
            );
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[test]
fn local_evaluated_bytes_unhex_cases() {
    let cases: &[(Option<&[u8]>, Option<&[u8]>)] = &[
        (None, None),
        (Some(b""), Some(b"")),
        (Some(b"f"), Some(b"\x0f")),
        (Some(b"aBc"), Some(b"\x0a\xbc")),
        (Some(b"aBcD"), Some(b"\xab\xcd")),
        (Some(b"FF00"), Some(b"\xff\0")),
        (Some(b"g1"), None),
        (Some(b"\xff"), None),
        (Some(b"0\0"), None),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::UnHex,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::UnHex);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, &(input, expected)) in cases.iter().enumerate() {
        let ComputedValue::Bytes(value) =
            worker.eval_one(input.map(|bytes| bytes.to_vec())).unwrap()
        else {
            panic!("UNHEX returned Int");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.into_option(), expected.map(|bytes| bytes.to_vec()));
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_bytes_output_outlives_worker() {
    let cases: &[(EvaluatedBytesOp, &[u8], &[u8])] = &[
        (EvaluatedBytesOp::LTrim, b" \xff\0 x ", b"\xff\0 x "),
        (EvaluatedBytesOp::RTrim, b" \xff\0 x ", b" \xff\0 x"),
        (EvaluatedBytesOp::UnHex, b"fF00", b"\xff\0"),
    ];
    for &(operation, input, expected) in cases {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        let storage = worker.retained_storage().unwrap();
        let ComputedValue::Bytes(first) = worker.eval_one(Some(input.to_vec())).unwrap() else {
            panic!("byte operation returned Int");
        };
        let ComputedValue::Bytes(second) = worker.eval_one(Some(b"00".to_vec())).unwrap() else {
            panic!("byte operation returned Int");
        };
        assert_eq!(first.value(), Some(expected));
        assert_eq!(worker.kernel_invocations(), 2);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        drop(worker);
        assert_eq!(first.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(first.into_option(), Some(expected.to_vec()));
        let second_expected: &[u8] = match operation {
            EvaluatedBytesOp::UnHex => b"\0",
            _ => b"00",
        };
        assert_eq!(second.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(second.into_option(), Some(second_expected.to_vec()));
    }
}

#[test]
fn local_evaluated_bytes_crc32_preserves_unsigned_bits() {
    let cases: &[(Option<&[u8]>, Option<i64>)] = &[
        (None, None),
        (Some(b""), Some(0)),
        (Some(b"123456789"), Some(0xcbf4_3926)),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::Crc32,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::Crc32);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, &(input, expected)) in cases.iter().enumerate() {
        let ComputedValue::Int(value) = worker.eval_one(input.map(|bytes| bytes.to_vec())).unwrap()
        else {
            panic!("CRC32 returned Bytes");
        };
        assert_eq!(value.value(), expected);
        if input == Some(b"123456789".as_slice()) {
            assert!(value.value().unwrap() > i64::from(i32::MAX));
        }
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_bytes_reverse_respects_units() {
    let binary_cases: &[(Option<&[u8]>, Option<&[u8]>)] = &[
        (None, None),
        (Some(b""), Some(b"")),
        (Some("aé🦀".as_bytes()), Some(b"\x80\xa6\x9f\xf0\xa9\xc3a")),
        (Some(b"\xff\0a"), Some(b"a\0\xff")),
    ];
    let utf8_cases: &[(Option<&[u8]>, Option<&[u8]>)] = &[
        (None, None),
        (Some(b""), Some(b"")),
        (Some("aé🦀".as_bytes()), Some("🦀éa".as_bytes())),
        (Some("a\u{fffd}".as_bytes()), Some("\u{fffd}a".as_bytes())),
    ];
    for (operation, cases) in [
        (EvaluatedBytesOp::Reverse, binary_cases),
        (EvaluatedBytesOp::ReverseUtf8, utf8_cases),
    ] {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        for (index, &(input, expected)) in cases.iter().enumerate() {
            let ComputedValue::Bytes(value) =
                worker.eval_one(input.map(|bytes| bytes.to_vec())).unwrap()
            else {
                panic!("REVERSE returned Int");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(value.into_option(), expected.map(|bytes| bytes.to_vec()));
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[test]
fn local_evaluated_bytes_char_length_respects_units() {
    let cases: &[(Option<&[u8]>, [Option<i64>; 2])] = &[
        (None, [None, None]),
        (Some(b""), [Some(0), Some(0)]),
        (Some("aé🦀".as_bytes()), [Some(7), Some(3)]),
        (Some("a\u{fffd}".as_bytes()), [Some(4), Some(2)]),
    ];
    for (column, operation) in [
        EvaluatedBytesOp::CharLength,
        EvaluatedBytesOp::CharLengthUtf8,
    ]
    .into_iter()
    .enumerate()
    {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        for (index, &(input, expected)) in cases.iter().enumerate() {
            let ComputedValue::Int(value) =
                worker.eval_one(input.map(|bytes| bytes.to_vec())).unwrap()
            else {
                panic!("CHAR_LENGTH returned Bytes");
            };
            assert_eq!(value.value(), expected[column]);
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), expected[column]);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[test]
fn local_evaluated_bytes_quote_preserves_raw_bytes_and_null_text() {
    let cases: &[(Option<&[u8]>, &[u8])] = &[
        (None, b"NULL"),
        (Some(b""), b"''"),
        (Some(b"'"), b"'\\''"),
        (Some(b"\\"), b"'\\\\'"),
        (Some(b"\0\x1a"), b"'\\0\\Z'"),
        (Some(b"\xff"), b"'\xff'"),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::Quote,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::Quote);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, &(input, expected)) in cases.iter().enumerate() {
        let ComputedValue::Bytes(value) =
            worker.eval_one(input.map(|bytes| bytes.to_vec())).unwrap()
        else {
            panic!("QUOTE returned Int");
        };
        assert_eq!(value.value(), Some(expected));
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.into_option(), Some(expected.to_vec()));
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_integer_formatters_preserve_bits() {
    let cases: &[(Option<i64>, [Option<&[u8]>; 2])] = &[
        (None, [None, None]),
        (Some(0), [Some(b"0"), Some(b"0")]),
        (
            Some(u64::MAX as i64),
            [
                Some(b"FFFFFFFFFFFFFFFF"),
                Some(b"1111111111111111111111111111111111111111111111111111111111111111"),
            ],
        ),
        (
            Some(i64::MIN),
            [
                Some(b"8000000000000000"),
                Some(b"1000000000000000000000000000000000000000000000000000000000000000"),
            ],
        ),
    ];
    for (column, operation) in [EvaluatedBytesOp::HexInt, EvaluatedBytesOp::Bin]
        .into_iter()
        .enumerate()
    {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        for (index, &(input, expected)) in cases.iter().enumerate() {
            let ComputedValue::Bytes(value) = worker.eval_args(EvaluatedArgs::Int(input)).unwrap()
            else {
                panic!("integer formatter returned Int");
            };
            assert_eq!(value.value(), expected[column]);
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(
                value.into_option(),
                expected[column].map(|bytes| bytes.to_vec())
            );
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[test]
fn local_evaluated_args_hex_str_preserves_bytes() {
    let cases: &[(Option<&[u8]>, Option<&[u8]>)] = &[
        (None, None),
        (Some(b""), Some(b"")),
        (Some(b"\xff\0"), Some(b"FF00")),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::HexStr,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::HexStr);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, &(input, expected)) in cases.iter().enumerate() {
        let args = EvaluatedArgs::Bytes(input.map(|bytes| bytes.to_vec()));
        let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
            panic!("HEX string operation returned Int");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.into_option(), expected.map(|bytes| bytes.to_vec()));
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_left_right_respect_units() {
    let cases: &[(Option<&[u8]>, Option<i64>, [Option<&[u8]>; 4])] = &[
        (None, Some(1), [None; 4]),
        (Some("aé🦀".as_bytes()), None, [None; 4]),
        (Some(b""), Some(2), [Some(b""); 4]),
        (Some("aé🦀".as_bytes()), Some(-1), [Some(b""); 4]),
        (Some("aé🦀".as_bytes()), Some(0), [Some(b""); 4]),
        (
            Some("aé🦀".as_bytes()),
            Some(2),
            [
                Some(b"a\xc3"),
                Some("aé".as_bytes()),
                Some(b"\xa6\x80"),
                Some("é🦀".as_bytes()),
            ],
        ),
        (
            Some("aé🦀".as_bytes()),
            Some(i64::MAX),
            [Some("aé🦀".as_bytes()); 4],
        ),
    ];
    for (column, operation) in [
        EvaluatedBytesOp::Left,
        EvaluatedBytesOp::LeftUtf8,
        EvaluatedBytesOp::Right,
        EvaluatedBytesOp::RightUtf8,
    ]
    .into_iter()
    .enumerate()
    {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        for (index, &(input, length, expected)) in cases.iter().enumerate() {
            let args = EvaluatedArgs::BytesInt(input.map(|bytes| bytes.to_vec()), length);
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!("LEFT or RIGHT returned Int");
            };
            assert_eq!(value.value(), expected[column]);
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(
                value.into_option(),
                expected[column].map(|bytes| bytes.to_vec())
            );
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[test]
fn local_evaluated_args_replace_is_owned_and_nullable() {
    let cases: &[([Option<&[u8]>; 3], Option<&[u8]>)] = &[
        ([Some(b"aaaaa"), Some(b"aa"), Some(b"b")], Some(b"bba")),
        ([Some(b"abc"), Some(b""), Some(b"x")], Some(b"abc")),
        ([Some(b"aaaaa"), Some(b"aa"), Some(b"")], Some(b"a")),
        (
            [Some(b"\xff\0\xff"), Some(b"\xff"), Some(b"x")],
            Some(b"x\0x"),
        ),
        ([None, Some(b"x"), Some(b"y")], None),
        ([Some(b"x"), None, Some(b"y")], None),
        ([Some(b"x"), Some(b"x"), None], None),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::Replace,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::Replace);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    let mut outputs = Vec::new();
    for (index, &(inputs, expected)) in cases.iter().enumerate() {
        let args = EvaluatedArgs::Bytes3(inputs.map(|input| input.map(|bytes| bytes.to_vec())));
        let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
            panic!("REPLACE returned Int");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        outputs.push(value);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    drop(worker);
    for (value, &(_, expected)) in outputs.into_iter().zip(cases) {
        assert_eq!(value.into_option(), expected.map(|bytes| bytes.to_vec()));
    }
}

#[test]
fn local_evaluated_args_shape_errors_are_preflight() {
    let cases: [(EvaluatedBytesOp, EvaluatedArgs, EvaluatedArgs, &[u8]); 3] = [
        (
            EvaluatedBytesOp::HexInt,
            EvaluatedArgs::Bytes(Some(b"1".to_vec())),
            EvaluatedArgs::Int(Some(1)),
            b"1",
        ),
        (
            EvaluatedBytesOp::Left,
            EvaluatedArgs::Bytes(Some(b"xy".to_vec())),
            EvaluatedArgs::BytesInt(Some(b"xy".to_vec()), Some(1)),
            b"x",
        ),
        (
            EvaluatedBytesOp::Replace,
            EvaluatedArgs::BytesInt(Some(b"x".to_vec()), Some(1)),
            EvaluatedArgs::Bytes3([
                Some(b"x".to_vec()),
                Some(b"x".to_vec()),
                Some(b"y".to_vec()),
            ]),
            b"y",
        ),
    ];
    for (operation, invalid, valid, expected) in cases {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        assert!(matches!(
            worker.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        assert!(matches!(
            worker.eval_one(None),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let ComputedValue::Bytes(value) = worker.eval_args(valid).unwrap() else {
            panic!("valid operation returned Int");
        };
        assert_eq!(value.value(), Some(expected));
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.into_option(), Some(expected.to_vec()));
        assert_eq!(worker.kernel_invocations(), 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_charge_all_ready_capacities_before_null() {
    let needle = Vec::<u8>::with_capacity(16 * 1024);
    let replacement = Vec::<u8>::with_capacity(16 * 1024);
    let limit = needle.capacity() + replacement.capacity() - 1;
    assert!(needle.capacity() < limit && replacement.capacity() < limit);
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::Replace,
        LocalCompileContext::default(),
        ExecutionLimits {
            max_retained_bytes: limit,
            ..ExecutionLimits::default()
        },
        usize::MAX,
    )
    .unwrap();
    let storage = worker.retained_storage().unwrap();
    // Empty buffers still own their full capacity even though a NULL subject
    // will make the official wrapper skip the non-null replacement body.
    assert!(matches!(
        worker.eval_args(EvaluatedArgs::Bytes3([
            None,
            Some(needle),
            Some(replacement)
        ])),
        Err(LocalError::ResourceLimit(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let ComputedValue::Bytes(value) = worker
        .eval_args(EvaluatedArgs::Bytes3([
            None,
            Some(Vec::new()),
            Some(Vec::new()),
        ]))
        .unwrap()
    else {
        panic!("REPLACE returned Int");
    };
    assert_eq!(value.value(), None);
    assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
    assert_eq!(worker.kernel_invocations(), 1);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
}

#[test]
fn local_evaluated_args_unary_bits_preserve_signed_carrier() {
    let cases: &[(Option<i64>, [Option<i64>; 2])] = &[
        (None, [None, None]),
        (Some(0), [Some(0), Some(-1)]),
        (Some(-1), [Some(64), Some(0)]),
        (Some(-2), [Some(63), Some(1)]),
        (Some(i64::MIN), [Some(1), Some(i64::MAX)]),
    ];
    for (column, operation) in [EvaluatedBytesOp::BitCount, EvaluatedBytesOp::BitNeg]
        .into_iter()
        .enumerate()
    {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        for (index, &(input, expected)) in cases.iter().enumerate() {
            let ComputedValue::Int(value) = worker.eval_args(EvaluatedArgs::Int(input)).unwrap()
            else {
                panic!("unary bit operation returned Bytes");
            };
            assert_eq!(value.value(), expected[column]);
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), expected[column]);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[test]
fn local_evaluated_args_binary_bits_preserve_high_bit_and_nulls() {
    let cases: &[(Option<i64>, Option<i64>, [Option<i64>; 3])] = &[
        (None, Some(-1), [None; 3]),
        (Some(i64::MIN), None, [None; 3]),
        (
            Some(i64::MIN),
            Some(-1),
            [Some(i64::MIN), Some(-1), Some(i64::MAX)],
        ),
        (
            Some(i64::MIN),
            Some(1),
            [
                Some(0),
                Some(-9_223_372_036_854_775_807),
                Some(-9_223_372_036_854_775_807),
            ],
        ),
    ];
    for (column, operation) in [
        EvaluatedBytesOp::BitAnd,
        EvaluatedBytesOp::BitOr,
        EvaluatedBytesOp::BitXor,
    ]
    .into_iter()
    .enumerate()
    {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        for (index, &(lhs, rhs, expected)) in cases.iter().enumerate() {
            let ComputedValue::Int(value) =
                worker.eval_args(EvaluatedArgs::Int2(lhs, rhs)).unwrap()
            else {
                panic!("binary bit operation returned Bytes");
            };
            assert_eq!(value.value(), expected[column]);
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), expected[column]);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[test]
fn local_evaluated_args_shifts_are_logical_and_not_modulo() {
    let cases: &[(Option<i64>, Option<i64>, [Option<i64>; 2])] = &[
        (None, Some(1), [None, None]),
        (Some(-1), None, [None, None]),
        (Some(i64::MIN), Some(0), [Some(i64::MIN), Some(i64::MIN)]),
        (Some(-1), Some(1), [Some(-2), Some(i64::MAX)]),
        (Some(-1), Some(63), [Some(i64::MIN), Some(1)]),
        (Some(-1), Some(64), [Some(0), Some(0)]),
        (Some(-1), Some(65), [Some(0), Some(0)]),
        (Some(-1), Some(4_294_967_296), [Some(0), Some(0)]),
        (Some(-1), Some(-1), [Some(0), Some(0)]),
    ];
    for (column, operation) in [EvaluatedBytesOp::LeftShift, EvaluatedBytesOp::RightShift]
        .into_iter()
        .enumerate()
    {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        for (index, &(lhs, rhs, expected)) in cases.iter().enumerate() {
            let ComputedValue::Int(value) =
                worker.eval_args(EvaluatedArgs::Int2(lhs, rhs)).unwrap()
            else {
                panic!("shift returned Bytes");
            };
            assert_eq!(value.value(), expected[column]);
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), expected[column]);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[test]
fn local_evaluated_args_int2_shape_errors_are_preflight() {
    let cases = [
        (
            EvaluatedBytesOp::BitNeg,
            EvaluatedArgs::Int2(None, None),
            EvaluatedArgs::Int(Some(0)),
            Some(-1),
        ),
        (
            EvaluatedBytesOp::BitAnd,
            EvaluatedArgs::Int(None),
            EvaluatedArgs::Int2(Some(i64::MIN), Some(-1)),
            Some(i64::MIN),
        ),
        (
            EvaluatedBytesOp::RightShift,
            EvaluatedArgs::BytesInt(None, Some(1)),
            EvaluatedArgs::Int2(Some(-1), Some(1)),
            Some(i64::MAX),
        ),
    ];
    for (operation, invalid, valid, expected) in cases {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        assert!(matches!(
            worker.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let ComputedValue::Int(value) = worker.eval_args(valid).unwrap() else {
            panic!("valid bit operation returned Bytes");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}
