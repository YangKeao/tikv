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

#[test]
fn local_evaluated_args_truth_predicates_preserve_null_semantics() {
    let cases: &[(EvaluatedBytesOp, [Option<i64>; 3], u64)] = &[
        (EvaluatedBytesOp::UnaryNot, [None, Some(1), Some(0)], 1),
        (EvaluatedBytesOp::IsNull, [Some(1), Some(0), Some(0)], 1),
        (EvaluatedBytesOp::IsTrue, [Some(0), Some(0), Some(1)], 1),
        (EvaluatedBytesOp::IsFalse, [Some(0), Some(1), Some(0)], 1),
        (
            EvaluatedBytesOp::IsTrueWithNull,
            [None, Some(0), Some(1)],
            1,
        ),
        (EvaluatedBytesOp::IsNotNull, [Some(0), Some(1), Some(1)], 2),
        (EvaluatedBytesOp::IsNotTrue, [Some(1), Some(1), Some(0)], 2),
        (EvaluatedBytesOp::IsNotFalse, [Some(1), Some(0), Some(1)], 2),
    ];
    for &(operation, expected, invocations_per_call) in cases {
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
        for (index, input) in [None, Some(0), Some(1)].into_iter().enumerate() {
            let ComputedValue::Int(value) = worker.eval_args(EvaluatedArgs::Int(input)).unwrap()
            else {
                panic!("truth predicate returned Bytes");
            };
            assert_eq!(value.value(), expected[index]);
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), expected[index]);
            assert_eq!(
                worker.kernel_invocations(),
                (index as u64 + 1) * invocations_per_call
            );
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[test]
fn local_evaluated_args_composite_truth_predicates_require_depth_three() {
    for operation in [
        EvaluatedBytesOp::IsNotNull,
        EvaluatedBytesOp::IsNotTrue,
        EvaluatedBytesOp::IsNotFalse,
    ] {
        assert!(matches!(
            prepare_evaluated_bytes(
                operation,
                LocalCompileContext {
                    limits: CompileLimits {
                        max_nodes: 4,
                        max_depth: 2,
                    },
                },
                ExecutionLimits::default(),
                usize::MAX,
            ),
            Err(LocalError::ResourceLimit(_))
        ));
        let worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext {
                limits: CompileLimits {
                    max_nodes: 4,
                    max_depth: 3,
                },
            },
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        assert_eq!(worker.kernel_invocations(), 0);
    }
}

#[test]
fn local_evaluated_bytes_digests_preserve_raw_bytes_and_nulls() {
    let cases: &[(Option<&[u8]>, [Option<&[u8]>; 2])] = &[
        (None, [None, None]),
        (
            Some(b""),
            [
                Some(b"d41d8cd98f00b204e9800998ecf8427e"),
                Some(b"da39a3ee5e6b4b0d3255bfef95601890afd80709"),
            ],
        ),
        (
            Some(b"abc"),
            [
                Some(b"900150983cd24fb0d6963f7d28e17f72"),
                Some(b"a9993e364706816aba3e25717850c26c9cd0d89d"),
            ],
        ),
        (
            Some(b"\xc0\x80"),
            [
                Some(b"b26555f33aedac7b2684438cc5d4d05e"),
                Some(b"8bf4822782a21d7ac68ece130ac36987548003bd"),
            ],
        ),
    ];
    for (column, operation) in [EvaluatedBytesOp::Md5, EvaluatedBytesOp::Sha1]
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
                panic!("digest operation returned Int");
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
fn local_evaluated_args_logical_ops_use_ready_three_valued_inputs() {
    // Both operands are ready values; None here is an actual SQL NULL.
    let cases: &[(Option<i64>, Option<i64>, [Option<i64>; 3])] = &[
        (None, None, [None, None, None]),
        (None, Some(0), [Some(0), None, None]),
        (None, Some(1), [None, Some(1), None]),
        (Some(0), None, [Some(0), None, None]),
        (Some(0), Some(0), [Some(0), Some(0), Some(0)]),
        (Some(0), Some(1), [Some(0), Some(1), Some(1)]),
        (Some(1), None, [None, Some(1), None]),
        (Some(1), Some(0), [Some(0), Some(1), Some(1)]),
        (Some(1), Some(1), [Some(1), Some(1), Some(0)]),
    ];
    for (column, operation) in [
        EvaluatedBytesOp::LogicalAnd,
        EvaluatedBytesOp::LogicalOr,
        EvaluatedBytesOp::LogicalXor,
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
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::BytesInt(None, None)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        for (index, &(lhs, rhs, expected)) in cases.iter().enumerate() {
            let ComputedValue::Int(value) =
                worker.eval_args(EvaluatedArgs::Int2(lhs, rhs)).unwrap()
            else {
                panic!("logical operation returned Bytes");
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
fn local_evaluated_bytes_inet_aton_preserves_ipv4_forms() {
    let cases: &[(Option<&[u8]>, Option<i64>)] = &[
        (None, None),
        (Some(b""), None),
        (Some(b"127.0.0.1"), Some(2_130_706_433)),
        (Some(b"255.255.255.255"), Some(4_294_967_295)),
        (Some(b"1"), Some(1)),
        (Some(b"1.2"), Some(16_777_218)),
        (Some(b"0.1.2"), Some(65_538)),
        (Some(b"1..2"), Some(16_777_218)),
        (Some(b"256"), None),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::InetAton,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::InetAton);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, &(input, expected)) in cases.iter().enumerate() {
        let ComputedValue::Int(value) = worker.eval_one(input.map(|bytes| bytes.to_vec())).unwrap()
        else {
            panic!("INET_ATON returned Bytes");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_inet_ntoa_checks_unsigned_range() {
    let cases: &[(Option<i64>, Option<&[u8]>)] = &[
        (None, None),
        (Some(0), Some(b"0.0.0.0")),
        (Some(4_294_967_295), Some(b"255.255.255.255")),
        (Some(4_294_967_296), None),
        (Some(u64::MAX as i64), None),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::InetNtoa,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::InetNtoa);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, &(input, expected)) in cases.iter().enumerate() {
        let ComputedValue::Bytes(value) = worker.eval_args(EvaluatedArgs::Int(input)).unwrap()
        else {
            panic!("INET_NTOA returned Int");
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
fn local_evaluated_bytes_inet6_preserves_binary_and_text_forms() {
    let ipv4: &[u8] = &[10, 0, 5, 9];
    let mapped: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 1, 2, 3, 4];
    let compatible: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4];
    let aton_cases: &[(Option<&[u8]>, Option<&[u8]>)] = &[
        (None, None),
        (Some(b""), None),
        (Some(b"10.0.5.9"), Some(ipv4)),
        (Some(b"::FFFF:1.2.3.4"), Some(mapped)),
        (Some(b"::1.2.3.4"), Some(compatible)),
    ];
    let ntoa_cases: &[(Option<&[u8]>, Option<&[u8]>)] = &[
        (None, None),
        (Some(b""), None),
        (Some(ipv4), Some(b"10.0.5.9")),
        (Some(mapped), Some(b"::ffff:1.2.3.4")),
        (Some(compatible), Some(b"::102:304")),
        (Some(b"\x01\x02\x03"), None),
    ];
    for (operation, cases) in [
        (EvaluatedBytesOp::Inet6Aton, aton_cases),
        (EvaluatedBytesOp::Inet6Ntoa, ntoa_cases),
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
                panic!("INET6 operation returned Int");
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
fn local_evaluated_args_raw_math_preserves_ieee754_classes() {
    let asin_cases: &[(Option<f64>, Option<f64>)] = &[
        (None, None),
        (Some(2.0), Some(f64::NAN)),
        (Some(0.0), Some(0.0)),
    ];
    let acos_cases: &[(Option<f64>, Option<f64>)] = &[
        (None, None),
        (Some(f64::NAN), Some(f64::NAN)),
        (Some(2.0), Some(f64::NAN)),
        (Some(1.0), Some(0.0)),
    ];
    let sqrt_cases: &[(Option<f64>, Option<f64>)] = &[
        (None, None),
        (Some(-1.0), None),
        (Some(f64::NAN), Some(f64::NAN)),
        (Some(f64::INFINITY), Some(f64::INFINITY)),
        (Some(-0.0), Some(-0.0)),
        (Some(4.0), Some(2.0)),
    ];
    let angle_cases: &[(Option<f64>, Option<f64>)] = &[
        (None, None),
        (Some(f64::NAN), Some(f64::NAN)),
        (Some(f64::INFINITY), Some(f64::INFINITY)),
        (Some(f64::NEG_INFINITY), Some(f64::NEG_INFINITY)),
        (Some(-0.0), Some(-0.0)),
        (Some(0.0), Some(0.0)),
    ];
    for (operation, cases) in [
        (EvaluatedBytesOp::AsinRaw, asin_cases),
        (EvaluatedBytesOp::AcosRaw, acos_cases),
        (EvaluatedBytesOp::SqrtRaw, sqrt_cases),
        (EvaluatedBytesOp::RadiansRaw, angle_cases),
        (EvaluatedBytesOp::DegreesRaw, angle_cases),
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
            let args = EvaluatedArgs::Ieee754Bits(input.map(f64::to_bits));
            let ComputedValue::Ieee754Bits(value) = worker.eval_args(args).unwrap() else {
                panic!("raw math returned a non-IEEE-754 value");
            };
            let bits = value.value();
            match expected {
                Some(expected) if expected.is_nan() => {
                    assert!(bits.is_some_and(|bits| f64::from_bits(bits).is_nan()));
                }
                _ => assert_eq!(bits, expected.map(f64::to_bits)),
            }
            assert_eq!(
                value.metadata(),
                ComputedIeee754BitsMetadata::OwnIeee754Bits
            );
            assert_eq!(value.into_option(), bits);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[test]
fn local_evaluated_args_sign_raw_returns_owned_signed_int() {
    let cases: &[(Option<f64>, Option<i64>)] = &[
        (None, None),
        (Some(f64::NAN), Some(0)),
        (Some(f64::NEG_INFINITY), Some(-1)),
        (Some(f64::INFINITY), Some(1)),
        (Some(-0.0), Some(0)),
        (Some(-3.0), Some(-1)),
        (Some(3.0), Some(1)),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::SignRaw,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::SignRaw);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, &(input, expected)) in cases.iter().enumerate() {
        let args = EvaluatedArgs::Ieee754Bits(input.map(f64::to_bits));
        let ComputedValue::Int(value) = worker.eval_args(args).unwrap() else {
            panic!("SIGN raw returned a non-Int value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_ieee754_roles_reject_other_carriers() {
    let mut raw = prepare_evaluated_bytes(
        EvaluatedBytesOp::AsinRaw,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(raw.operation(), EvaluatedBytesOp::AsinRaw);
    assert_eq!(raw.kernel_invocations(), 0);
    let raw_storage = raw.retained_storage().unwrap();
    for invalid in [
        EvaluatedArgs::Bytes(None),
        EvaluatedArgs::Bytes(Some(vec![0; 8])),
        EvaluatedArgs::Int(Some(1.0_f64.to_bits() as i64)),
    ] {
        assert!(matches!(
            raw.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(raw.kernel_invocations(), 0);
        assert!(raw.is_healthy());
        assert_eq!(raw.retained_storage().unwrap(), raw_storage);
    }
    let ComputedValue::Ieee754Bits(value) = raw
        .eval_args(EvaluatedArgs::Ieee754Bits(Some(0.0_f64.to_bits())))
        .unwrap()
    else {
        panic!("raw worker returned a non-IEEE-754 value");
    };
    assert_eq!(value.value(), Some(0.0_f64.to_bits()));
    assert_eq!(
        value.metadata(),
        ComputedIeee754BitsMetadata::OwnIeee754Bits
    );
    assert_eq!(value.into_option(), Some(0.0_f64.to_bits()));
    assert_eq!(raw.kernel_invocations(), 1);
    assert!(raw.is_healthy());
    assert_eq!(raw.retained_storage().unwrap(), raw_storage);

    let mut bytes = prepare_evaluated_bytes(
        EvaluatedBytesOp::Md5,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(bytes.operation(), EvaluatedBytesOp::Md5);
    assert_eq!(bytes.kernel_invocations(), 0);
    let bytes_storage = bytes.retained_storage().unwrap();
    for input in [None, Some(1.0_f64.to_bits())] {
        assert!(matches!(
            bytes.eval_args(EvaluatedArgs::Ieee754Bits(input)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(bytes.kernel_invocations(), 0);
        assert!(bytes.is_healthy());
        assert_eq!(bytes.retained_storage().unwrap(), bytes_storage);
    }
    let ComputedValue::Bytes(value) = bytes.eval_one(None).unwrap() else {
        panic!("MD5 returned a non-Bytes value");
    };
    assert_eq!(value.value(), None);
    assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
    assert_eq!(value.into_option(), None);
    assert_eq!(bytes.kernel_invocations(), 1);
    assert!(bytes.is_healthy());
    assert_eq!(bytes.retained_storage().unwrap(), bytes_storage);
}

#[test]
fn local_evaluated_args_pi_uses_no_args_and_rejects_other_roles() {
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::PiRaw,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::PiRaw);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for invalid in [
        EvaluatedArgs::Bytes(None),
        EvaluatedArgs::Ieee754Bits(None),
        EvaluatedArgs::Int(None),
    ] {
        assert!(matches!(
            worker.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    for invocations in 1..=3_u64 {
        let ComputedValue::Ieee754Bits(value) = worker.eval_args(EvaluatedArgs::NoArgs).unwrap()
        else {
            panic!("PI returned a non-IEEE-754 value");
        };
        assert_eq!(value.value(), Some(std::f64::consts::PI.to_bits()));
        assert_eq!(
            value.metadata(),
            ComputedIeee754BitsMetadata::OwnIeee754Bits
        );
        assert_eq!(value.into_option(), Some(std::f64::consts::PI.to_bits()));
        assert_eq!(worker.kernel_invocations(), invocations);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }

    let mut bytes = prepare_evaluated_bytes(
        EvaluatedBytesOp::Md5,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(bytes.operation(), EvaluatedBytesOp::Md5);
    assert_eq!(bytes.kernel_invocations(), 0);
    let bytes_storage = bytes.retained_storage().unwrap();
    assert!(matches!(
        bytes.eval_args(EvaluatedArgs::NoArgs),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(bytes.kernel_invocations(), 0);
    assert!(bytes.is_healthy());
    assert_eq!(bytes.retained_storage().unwrap(), bytes_storage);
    let ComputedValue::Bytes(value) = bytes.eval_one(None).unwrap() else {
        panic!("MD5 returned a non-Bytes value");
    };
    assert_eq!(value.value(), None);
    assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
    assert_eq!(value.into_option(), None);
    assert_eq!(bytes.kernel_invocations(), 1);
    assert!(bytes.is_healthy());
    assert_eq!(bytes.retained_storage().unwrap(), bytes_storage);
}

#[test]
fn local_evaluated_bytes_ip_text_predicates_keep_null_private() {
    assert_eq!(crate::impl_miscellaneous::is_ipv4(None).unwrap(), Some(0));
    assert_eq!(crate::impl_miscellaneous::is_ipv6(None).unwrap(), Some(0));
    let cases: &[(Option<&[u8]>, [Option<i64>; 2])] = &[
        (None, [None, None]),
        (Some(b"127.0.0.1"), [Some(1), Some(0)]),
        (Some(b"::1"), [Some(0), Some(1)]),
        (Some(b"bad"), [Some(0), Some(0)]),
    ];
    for (column, operation) in [
        EvaluatedBytesOp::IsIpv4Nullable,
        EvaluatedBytesOp::IsIpv6Nullable,
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
                panic!("IP text predicate returned a non-Int value");
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
fn local_evaluated_bytes_ip_prefix_predicates_keep_null_private() {
    assert_eq!(
        crate::impl_miscellaneous::is_ipv4_compat(None).unwrap(),
        Some(0)
    );
    assert_eq!(
        crate::impl_miscellaneous::is_ipv4_mapped(None).unwrap(),
        Some(0)
    );
    let mapped: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 1, 2, 3, 4];
    let compatible: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4];
    let cases: &[(Option<&[u8]>, [Option<i64>; 2])] = &[
        (None, [None, None]),
        (Some(mapped), [Some(0), Some(1)]),
        (Some(compatible), [Some(1), Some(0)]),
        (Some(b"\x01\x02\x03"), [Some(0), Some(0)]),
    ];
    for (column, operation) in [
        EvaluatedBytesOp::IsIpv4CompatNullable,
        EvaluatedBytesOp::IsIpv4MappedNullable,
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
                panic!("IP prefix predicate returned a non-Int value");
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
fn local_evaluated_args_space_native_respects_packet_disposition() {
    use OutputDisposition::{Allow, SuppressByPacket};

    let cases: [(Option<i64>, OutputDisposition, Option<&[u8]>); 5] = [
        (None, Allow, None),
        (Some(0), Allow, Some(b"")),
        (Some(3), Allow, Some(b"   ")),
        (Some(16_777_217), Allow, None),
        (Some(i64::MAX), SuppressByPacket, None),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::SpaceNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::SpaceNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, (input, disposition, expected)) in cases.into_iter().enumerate() {
        let args = EvaluatedArgs::PacketInt {
            value: input,
            disposition,
        };
        let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
            panic!("SPACE native returned a non-Bytes value");
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
fn local_evaluated_args_repeat_native_handles_empty_and_suppressed_output() {
    use OutputDisposition::{Allow, SuppressByPacket};

    let cases: [(Option<&[u8]>, ReadyIntArg, OutputDisposition, Option<&[u8]>); 5] = [
        (
            Some(b"ab"),
            ReadyIntArg::Value(Some(3)),
            Allow,
            Some(b"ababab"),
        ),
        (
            Some(b""),
            ReadyIntArg::Value(Some(i64::MAX)),
            Allow,
            Some(b""),
        ),
        (Some(b"ab"), ReadyIntArg::Value(None), Allow, None),
        (
            Some(b"ab"),
            ReadyIntArg::Value(Some(i64::MAX)),
            SuppressByPacket,
            None,
        ),
        (None, ReadyIntArg::Undemanded, Allow, None),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::RepeatNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::RepeatNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (bytes, disposition) in [(Some(b"ab".to_vec()), Allow), (None, SuppressByPacket)] {
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::PacketBytesInt {
                bytes,
                count: ReadyIntArg::Undemanded,
                disposition,
            }),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    for (index, (input, count, disposition, expected)) in cases.into_iter().enumerate() {
        let args = EvaluatedArgs::PacketBytesInt {
            bytes: input.map(|bytes| bytes.to_vec()),
            count,
            disposition,
        };
        let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
            panic!("REPEAT native returned a non-Bytes value");
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
fn local_evaluated_args_base64_native_keeps_wire_behavior_separate() {
    use OutputDisposition::{Allow, SuppressByPacket};
    use tidb_query_datatype::codec::data_type::Bytes;

    for (input, expected) in [
        (b"YQ".as_slice(), b"".as_slice()),
        (b"YQ==\x0b".as_slice(), b"a".as_slice()),
    ] {
        let output = crate::test_util::RpnFnScalarEvaluator::new()
            .push_param(Some(input.to_vec()))
            .evaluate::<Bytes>(ScalarFuncSig::FromBase64)
            .unwrap();
        assert_eq!(output, Some(expected.to_vec()));
    }
    let encode_cases: Vec<(Option<&[u8]>, OutputDisposition, Option<&[u8]>)> = vec![
        (None, Allow, None),
        (Some(b""), Allow, Some(b"")),
        (Some(b"a"), Allow, Some(b"YQ==")),
        (Some(b"a"), SuppressByPacket, None),
    ];
    let decode_cases: Vec<(Option<&[u8]>, OutputDisposition, Option<&[u8]>)> = vec![
        (None, Allow, None),
        (Some(b"YQ=="), Allow, Some(b"a")),
        (Some(b"YQ"), Allow, None),
        (Some(b"YQ==\x0b"), Allow, None),
        (Some(b"YR=="), Allow, None),
        (Some(b" \t\r\nYQ== \t\r\n"), Allow, Some(b"a")),
        (Some(b"YQ=="), SuppressByPacket, None),
    ];
    let mut value_decoder = prepare_evaluated_bytes(
        EvaluatedBytesOp::FromBase64ValueNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(
        value_decoder.operation(),
        EvaluatedBytesOp::FromBase64ValueNative
    );
    assert_eq!(value_decoder.kernel_invocations(), 0);
    let value_storage = value_decoder.retained_storage().unwrap();
    let mut value_invocations = 0;
    for (operation, cases) in [
        (EvaluatedBytesOp::ToBase64Native, encode_cases),
        (EvaluatedBytesOp::FromBase64Native, decode_cases),
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
        for (index, (input, disposition, expected)) in cases.into_iter().enumerate() {
            let compare_value = operation == EvaluatedBytesOp::FromBase64Native
                && matches!(&disposition, OutputDisposition::Allow);
            let args = EvaluatedArgs::PacketBytes {
                value: input.map(|bytes| bytes.to_vec()),
                disposition,
            };
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!("Base64 native returned a non-Bytes value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(value.into_option(), expected.map(|bytes| bytes.to_vec()));
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            if compare_value {
                let ComputedValue::Bytes(value) = value_decoder
                    .eval_one(input.map(|bytes| bytes.to_vec()))
                    .unwrap()
                else {
                    panic!("value-only Base64 returned a non-Bytes value");
                };
                assert_eq!(value.value(), expected);
                assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
                assert_eq!(value.into_option(), expected.map(|bytes| bytes.to_vec()));
                value_invocations += 1;
                assert_eq!(value_decoder.kernel_invocations(), value_invocations);
                assert!(value_decoder.is_healthy());
                assert_eq!(value_decoder.retained_storage().unwrap(), value_storage);
            }
        }
    }
}

#[test]
fn local_evaluated_args_packet_roles_reject_plain_carriers() {
    use OutputDisposition::Allow;

    let cases = [
        (
            EvaluatedBytesOp::SpaceNative,
            EvaluatedArgs::Int2(Some(1), Some(1)),
            EvaluatedArgs::PacketInt {
                value: None,
                disposition: Allow,
            },
        ),
        (
            EvaluatedBytesOp::RepeatNative,
            EvaluatedArgs::BytesInt(Some(b"ab".to_vec()), Some(1)),
            EvaluatedArgs::PacketBytesInt {
                bytes: Some(b"ab".to_vec()),
                count: ReadyIntArg::Value(None),
                disposition: Allow,
            },
        ),
        (
            EvaluatedBytesOp::BitAnd,
            EvaluatedArgs::PacketInt {
                value: Some(1),
                disposition: Allow,
            },
            EvaluatedArgs::Int2(None, Some(1)),
        ),
        (
            EvaluatedBytesOp::Left,
            EvaluatedArgs::PacketBytes {
                value: Some(b"abc".to_vec()),
                disposition: Allow,
            },
            EvaluatedArgs::BytesInt(None, Some(1)),
        ),
    ];
    for (operation, invalid, valid) in cases {
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
        match worker.eval_args(valid).unwrap() {
            ComputedValue::Int(value) => {
                assert_eq!(operation, EvaluatedBytesOp::BitAnd);
                assert_eq!(value.value(), None);
                assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
                assert_eq!(value.into_option(), None);
            }
            ComputedValue::Bytes(value) => {
                assert_ne!(operation, EvaluatedBytesOp::BitAnd);
                assert_eq!(value.value(), None);
                assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
                assert_eq!(value.into_option(), None);
            }
            ComputedValue::Ieee754Bits(_)
            | ComputedValue::Decimal(_)
            | ComputedValue::Int128(_)
            | ComputedValue::Uncompress(_)
            | ComputedValue::JsonReport(_) => {
                panic!("packet role check returned an unexpected output type")
            }
        }
        assert_eq!(worker.kernel_invocations(), 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_sha2_native_keeps_invalid_bits_warning_free() {
    let (wire_result, wire_ctx) = crate::test_util::RpnFnScalarEvaluator::new()
        .push_param(Some(b"pingcap".to_vec()))
        .push_param(Some(-1_i64))
        .evaluate_raw(FieldTypeTp::VarString, ScalarFuncSig::Sha2);
    assert!(matches!(wire_result.unwrap(), ScalarValue::Bytes(None)));
    assert_eq!(wire_ctx.warnings.warning_cnt, 1);
    assert_eq!(wire_ctx.warnings.warnings.len(), 1);
    assert_eq!(wire_ctx.warnings.warnings[0].get_code(), 1583);

    // Fixed vectors from the official SHA2 tests, including 0 == 256.
    let sha256: &[u8] = b"2871823be240f8ecd1d72f24c99eaa2e58af18b4b8ba99a4fc2823ba5c43930a";
    let cases: [(Option<&[u8]>, ReadyIntArg, Option<&[u8]>); 9] = [
        (None, ReadyIntArg::Value(Some(256)), None),
        (Some(b"pingcap"), ReadyIntArg::Value(None), None),
        (None, ReadyIntArg::Undemanded, None),
        (Some(b"pingcap"), ReadyIntArg::Value(Some(-1)), None),
        (Some(b"pingcap"), ReadyIntArg::Value(Some(0)), Some(sha256)),
        (
            Some(b"pingcap"),
            ReadyIntArg::Value(Some(224)),
            Some(b"cd036dc9bec69e758401379c522454ea24a6327b48724b449b40c6b7"),
        ),
        (Some(b"pingcap"), ReadyIntArg::Value(Some(256)), Some(sha256)),
        (
            Some(b"pingcap"),
            ReadyIntArg::Value(Some(384)),
            Some(b"c50955b6b0c7b9919740d956849eedcb0f0f90bf8a34e8c1f4e071e3773f53bd6f8f16c04425ff728bed04de1b63db51"),
        ),
        (
            Some(b"pingcap"),
            ReadyIntArg::Value(Some(512)),
            Some(b"ea903c574370774c4844a83b7122105a106e04211673810e1baae7c2ae7aba2cf07465e02f6c413126111ef74a417232683ce7ba210052e63c15fc82204aad80"),
        ),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::Sha2Native,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::Sha2Native);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, (input, count, expected)) in cases.into_iter().enumerate() {
        let args = EvaluatedArgs::BytesIntReady {
            bytes: input.map(|bytes| bytes.to_vec()),
            count,
        };
        let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
            panic!("SHA2 native returned a non-Bytes value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.into_option(), expected.map(|bytes| bytes.to_vec()));
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        // These observations require the private context's warning count to stay zero.
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_bytes_case_conversion_preserves_binary_and_simple_unicode() {
    let binary_cases: &[(Option<&[u8]>, Option<&[u8]>)] = &[
        (None, None),
        (Some(b""), Some(b"")),
        (Some(b"Ab\xff"), Some(b"Ab\xff")),
    ];
    let lower_cases: &[(Option<&[u8]>, Option<&[u8]>)] = &[
        (None, None),
        (Some(b""), Some(b"")),
        (Some("İİIIÅI".as_bytes()), Some("iiiiåi".as_bytes())),
    ];
    let upper_cases: &[(Option<&[u8]>, Option<&[u8]>)] = &[
        (None, None),
        (Some(b""), Some(b"")),
        (Some("ßßåı".as_bytes()), Some("ßßÅI".as_bytes())),
    ];
    for (operation, cases) in [
        (EvaluatedBytesOp::Lower, binary_cases),
        (EvaluatedBytesOp::Upper, binary_cases),
        (EvaluatedBytesOp::LowerUtf8Ready, lower_cases),
        (EvaluatedBytesOp::UpperUtf8Ready, upper_cases),
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
                panic!("case conversion returned a non-Bytes value");
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
fn local_evaluated_bytes_ord_native_folds_prepared_bytes() {
    // The frontend already selected and encoded the first character.
    let cases: &[(Option<&[u8]>, Option<i64>)] = &[
        (None, None),
        (Some(b""), Some(0)),
        (Some(b"\xff"), Some(255)),
        (Some(b"\xe4\xbd\xa0"), Some(14_990_752)),
        (Some(b"\xc4\xe3"), Some(50_403)),
        (Some(b"\xff\xff\xff\xff"), Some(4_294_967_295)),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::OrdNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::OrdNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    assert!(matches!(
        worker.eval_one(Some(vec![0; 5])),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    for (index, &(input, expected)) in cases.iter().enumerate() {
        let ComputedValue::Int(value) = worker.eval_one(input.map(|bytes| bytes.to_vec())).unwrap()
        else {
            panic!("ORD native returned a non-Int value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_ready_int_roles_reject_plain_carriers() {
    let cases: [(
        EvaluatedBytesOp,
        EvaluatedArgs,
        EvaluatedArgs,
        Option<&[u8]>,
    ); 3] = [
        (
            EvaluatedBytesOp::Sha2Native,
            EvaluatedArgs::BytesIntReady {
                bytes: Some(b"pingcap".to_vec()),
                count: ReadyIntArg::Undemanded,
            },
            EvaluatedArgs::BytesIntReady {
                bytes: None,
                count: ReadyIntArg::Undemanded,
            },
            None,
        ),
        (
            EvaluatedBytesOp::Sha2Native,
            EvaluatedArgs::BytesInt(Some(b"pingcap".to_vec()), Some(256)),
            EvaluatedArgs::BytesIntReady {
                bytes: Some(b"pingcap".to_vec()),
                count: ReadyIntArg::Value(None),
            },
            None,
        ),
        (
            EvaluatedBytesOp::Left,
            EvaluatedArgs::BytesIntReady {
                bytes: Some(b"ab".to_vec()),
                count: ReadyIntArg::Value(Some(1)),
            },
            EvaluatedArgs::BytesInt(Some(b"ab".to_vec()), Some(1)),
            Some(b"a"),
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
        let ComputedValue::Bytes(value) = worker.eval_args(valid).unwrap() else {
            panic!("ready-int role check returned a non-Bytes value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.into_option(), expected.map(|bytes| bytes.to_vec()));
        assert_eq!(worker.kernel_invocations(), 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_trim_native_handles_overlapping_ends() {
    use tidb_query_datatype::codec::data_type::Bytes;

    let wire = crate::test_util::RpnFnScalarEvaluator::new()
        .push_param(Some(b"ababa".to_vec()))
        .push_param(Some(b"aba".to_vec()))
        .evaluate::<Bytes>(ScalarFuncSig::Trim2Args)
        .unwrap();
    assert_eq!(wire, Some(Vec::new()));
    let cases: &[(Option<&[u8]>, Option<&[u8]>, [Option<&[u8]>; 3])] = &[
        (
            Some(b"ababa"),
            Some(b"aba"),
            [Some(b"ba"), Some(b"ba"), Some(b"ab")],
        ),
        (
            Some(b" a "),
            Some(b" "),
            [Some(b"a"), Some(b"a "), Some(b" a")],
        ),
        (Some(b"abc"), Some(b""), [Some(b"abc"); 3]),
        (None, Some(b" "), [None; 3]),
        (Some(b"abc"), None, [None; 3]),
    ];
    for (column, operation) in [
        EvaluatedBytesOp::TrimBothNative,
        EvaluatedBytesOp::TrimLeadingNative,
        EvaluatedBytesOp::TrimTrailingNative,
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
        for (index, &(input, remove, expected)) in cases.iter().enumerate() {
            let args = EvaluatedArgs::Bytes2(
                input.map(|bytes| bytes.to_vec()),
                remove.map(|bytes| bytes.to_vec()),
            );
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!("TRIM native returned a non-Bytes value");
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
fn local_evaluated_args_substring_index_native_preserves_count_roles() {
    use tidb_query_datatype::codec::data_type::Bytes;

    let wire = crate::test_util::RpnFnScalarEvaluator::new()
        .push_param(Some(b"aaa".to_vec()))
        .push_param(Some(b"aa".to_vec()))
        .push_param(Some(-1_i64))
        .evaluate::<Bytes>(ScalarFuncSig::SubstringIndex)
        .unwrap();
    assert_eq!(wire, Some(Vec::new()));
    for (column, operation) in [
        EvaluatedBytesOp::SubstringIndexSignedNative,
        EvaluatedBytesOp::SubstringIndexUnsignedNative,
    ]
    .into_iter()
    .enumerate()
    {
        let cases: [(
            Option<&[u8]>,
            Option<&[u8]>,
            ReadyIntArg,
            [Option<&[u8]>; 2],
        ); 7] = [
            (
                Some(b"a.b.c"),
                Some(b"."),
                ReadyIntArg::Value(Some(2)),
                [Some(b"a.b"); 2],
            ),
            (
                Some(b"aaa"),
                Some(b"aa"),
                ReadyIntArg::Value(Some(-1)),
                [Some(b"a"), Some(b"aaa")],
            ),
            (
                Some(b"aaa"),
                Some(b"aa"),
                ReadyIntArg::Value(Some(i64::MIN)),
                [Some(b"aaa"); 2],
            ),
            (Some(b"abc"), Some(b""), ReadyIntArg::Value(None), [None; 2]),
            (
                Some(b"abc"),
                Some(b""),
                ReadyIntArg::Undemanded,
                [Some(b""); 2],
            ),
            (None, Some(b"."), ReadyIntArg::Undemanded, [None; 2]),
            (Some(b"abc"), None, ReadyIntArg::Undemanded, [None; 2]),
        ];
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
        for invalid in [
            EvaluatedArgs::BytesBytesIntReady {
                bytes: Some(b"aaa".to_vec()),
                delimiter: Some(b"aa".to_vec()),
                count: ReadyIntArg::Undemanded,
            },
            EvaluatedArgs::Bytes3([
                Some(b"aaa".to_vec()),
                Some(b"aa".to_vec()),
                Some(b"2".to_vec()),
            ]),
        ] {
            assert!(matches!(
                worker.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        for (index, (input, delimiter, count, expected)) in cases.into_iter().enumerate() {
            let args = EvaluatedArgs::BytesBytesIntReady {
                bytes: input.map(|bytes| bytes.to_vec()),
                delimiter: delimiter.map(|bytes| bytes.to_vec()),
                count,
            };
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!("SUBSTRING_INDEX native returned a non-Bytes value");
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
fn local_evaluated_args_pad_native_preserves_units_and_empty_pad() {
    use OutputDisposition::{Allow, SuppressByPacket};
    use ReadyBytesArg::{Undemanded, Value};
    use tidb_query_datatype::codec::data_type::Bytes;

    // Growth with an empty pad is safe to compare; do not call the wire
    // equal-length case.
    let wire = crate::test_util::RpnFnScalarEvaluator::new()
        .push_param(Some(b"ab".to_vec()))
        .push_param(Some(3_i64))
        .push_param(Some(Vec::<u8>::new()))
        .evaluate::<Bytes>(ScalarFuncSig::Lpad)
        .unwrap();
    assert_eq!(wire, None);
    for (column, operation) in [
        EvaluatedBytesOp::LpadBytesNative,
        EvaluatedBytesOp::RpadBytesNative,
        EvaluatedBytesOp::LpadUtf8Native,
        EvaluatedBytesOp::RpadUtf8Native,
    ]
    .into_iter()
    .enumerate()
    {
        let cases: [(
            ReadyBytesArg,
            Option<i64>,
            ReadyBytesArg,
            OutputDisposition,
            [Option<&[u8]>; 4],
        ); 7] = [
            (
                Value(Some("é".as_bytes().to_vec())),
                Some(3),
                Value(Some(b"x".to_vec())),
                Allow,
                [
                    Some("xé".as_bytes()),
                    Some("éx".as_bytes()),
                    Some("xxé".as_bytes()),
                    Some("éxx".as_bytes()),
                ],
            ),
            (
                Value(Some("éab".as_bytes().to_vec())),
                Some(2),
                Value(Some(b"x".to_vec())),
                Allow,
                [
                    Some("é".as_bytes()),
                    Some("é".as_bytes()),
                    Some("éa".as_bytes()),
                    Some("éa".as_bytes()),
                ],
            ),
            (
                Value(Some(b"ab".to_vec())),
                Some(3),
                Value(Some(Vec::new())),
                Allow,
                [Some(b""); 4],
            ),
            (
                Value(Some(b"a".to_vec())),
                Some(4_194_305),
                Value(Some(Vec::new())),
                Allow,
                [Some(b""); 4],
            ),
            (
                Value(Some(b"ab".to_vec())),
                Some(2),
                Value(Some(Vec::new())),
                Allow,
                [Some(b"ab"); 4],
            ),
            (
                Value(None),
                Some(2),
                Value(Some(b"x".to_vec())),
                Allow,
                [None; 4],
            ),
            (
                Undemanded,
                Some(i64::MAX),
                Undemanded,
                SuppressByPacket,
                [None; 4],
            ),
        ];
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
        for (index, (bytes, count, pad, disposition, expected)) in cases.into_iter().enumerate() {
            let args = EvaluatedArgs::PacketBytesIntBytes {
                bytes,
                count,
                pad,
                disposition,
            };
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!("padding native returned a non-Bytes value");
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
fn local_evaluated_args_pad_marker_admission_is_preflight() {
    use OutputDisposition::{Allow, SuppressByPacket};
    use ReadyBytesArg::{Undemanded, Value};

    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::LpadBytesNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::LpadBytesNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (bytes, count, pad, disposition) in [
        (Undemanded, Some(0), Undemanded, Allow),
        (Undemanded, Some(1), Undemanded, Allow),
        (Value(None), None, Undemanded, Allow),
        (
            Undemanded,
            Some(i64::MAX),
            Value(Some(b"x".to_vec())),
            SuppressByPacket,
        ),
    ] {
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::PacketBytesIntBytes {
                bytes,
                count,
                pad,
                disposition,
            }),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    for (index, count) in [None, Some(-1), Some(16_777_217)].into_iter().enumerate() {
        let ComputedValue::Bytes(value) = worker
            .eval_args(EvaluatedArgs::PacketBytesIntBytes {
                bytes: Undemanded,
                count,
                pad: Undemanded,
                disposition: Allow,
            })
            .unwrap()
        else {
            panic!("padding marker returned a non-Bytes value");
        };
        assert_eq!(value.value(), None);
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.into_option(), None);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_native_logarithms_and_power_preserve_ieee754_classes() {
    use tidb_query_datatype::codec::data_type::Real;

    assert!(
        crate::test_util::RpnFnScalarEvaluator::new()
            .push_param(Some(Real::new(1e308).unwrap()))
            .push_param(Some(Real::new(2.0).unwrap()))
            .evaluate::<Real>(ScalarFuncSig::Pow)
            .is_err()
    );
    let wire_nan = crate::test_util::RpnFnScalarEvaluator::new()
        .push_param(Some(Real::new(-1.0).unwrap()))
        .push_param(Some(Real::new(0.5).unwrap()))
        .evaluate::<Real>(ScalarFuncSig::Pow)
        .unwrap();
    assert_eq!(wire_nan, None);

    let ln_cases: &[(Option<f64>, Option<f64>)] = &[
        (None, None),
        (Some(1.0), Some(0.0)),
        (Some(0.0), Some(f64::NEG_INFINITY)),
        (Some(-1.0), Some(f64::NAN)),
        (Some(f64::NAN), Some(f64::NAN)),
        (Some(f64::INFINITY), Some(f64::INFINITY)),
    ];
    let log2_cases: &[(Option<f64>, Option<f64>)] = &[
        (None, None),
        (Some(8.0), Some(3.0)),
        (Some(0.0), Some(f64::NEG_INFINITY)),
        (Some(-1.0), Some(f64::NAN)),
        (Some(f64::NAN), Some(f64::NAN)),
        (Some(f64::INFINITY), Some(f64::INFINITY)),
    ];
    for (operation, cases) in [
        (EvaluatedBytesOp::LnNative, ln_cases),
        (EvaluatedBytesOp::Log2Native, log2_cases),
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
            let args = EvaluatedArgs::Ieee754Bits(input.map(f64::to_bits));
            let ComputedValue::Ieee754Bits(value) = worker.eval_args(args).unwrap() else {
                panic!("native logarithm returned a non-IEEE-754 value");
            };
            let bits = value.value();
            match expected {
                Some(expected) if expected.is_nan() => {
                    assert!(bits.is_some_and(|bits| f64::from_bits(bits).is_nan()));
                }
                _ => assert_eq!(bits, expected.map(f64::to_bits)),
            }
            assert_eq!(
                value.metadata(),
                ComputedIeee754BitsMetadata::OwnIeee754Bits
            );
            assert_eq!(value.into_option(), bits);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }

    let log_cases: &[(Option<f64>, Option<f64>, Option<f64>)] = &[
        (Some(2.0), Some(4.0), Some(2.0)),
        (Some(2.0), None, None),
        (Some(2.0), Some(-1.0), Some(f64::NAN)),
        (Some(1.0), Some(2.0), Some(f64::INFINITY)),
        (Some(2.0), Some(f64::INFINITY), Some(f64::INFINITY)),
        (Some(f64::NAN), Some(2.0), Some(f64::NAN)),
    ];
    let pow_cases: &[(Option<f64>, Option<f64>, Option<f64>)] = &[
        (Some(2.0), Some(3.0), Some(8.0)),
        (Some(2.0), None, None),
        (Some(1e308), Some(2.0), Some(f64::INFINITY)),
        (Some(-1.0), Some(0.5), Some(f64::NAN)),
        (Some(f64::NAN), Some(0.0), Some(1.0)),
        (Some(f64::INFINITY), Some(-1.0), Some(0.0)),
    ];
    for (operation, cases) in [
        (EvaluatedBytesOp::LogNative, log_cases),
        (EvaluatedBytesOp::PowNative, pow_cases),
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
        for (index, &(left, right, expected)) in cases.iter().enumerate() {
            let args = EvaluatedArgs::Ieee754Bits2 {
                left: ReadyIeee754Arg::Value(left.map(f64::to_bits)),
                right: ReadyIeee754Arg::Value(right.map(f64::to_bits)),
            };
            let ComputedValue::Ieee754Bits(value) = worker.eval_args(args).unwrap() else {
                panic!("native binary math returned a non-IEEE-754 value");
            };
            let bits = value.value();
            match expected {
                Some(expected) if expected.is_nan() => {
                    assert!(bits.is_some_and(|bits| f64::from_bits(bits).is_nan()));
                }
                _ => assert_eq!(bits, expected.map(f64::to_bits)),
            }
            assert_eq!(
                value.metadata(),
                ComputedIeee754BitsMetadata::OwnIeee754Bits
            );
            assert_eq!(value.into_option(), bits);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[test]
fn local_evaluated_args_binary_ieee754_markers_are_role_checked() {
    use ReadyIeee754Arg::{Undemanded, Value};

    for operation in [EvaluatedBytesOp::LogNative, EvaluatedBytesOp::PowNative] {
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
        for invalid in [
            EvaluatedArgs::Ieee754Bits2 {
                left: Undemanded,
                right: Undemanded,
            },
            EvaluatedArgs::Ieee754Bits2 {
                left: Undemanded,
                right: Value(Some(f64::NAN.to_bits())),
            },
            EvaluatedArgs::Ieee754Bits2 {
                left: Value(Some(2.0_f64.to_bits())),
                right: Undemanded,
            },
            EvaluatedArgs::Bytes2(None, None),
        ] {
            assert!(matches!(
                worker.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let mut invocations = 0_u64;
        for (left, right) in [(Undemanded, Value(None)), (Value(None), Undemanded)] {
            let result = worker.eval_args(EvaluatedArgs::Ieee754Bits2 { left, right });
            if operation == EvaluatedBytesOp::PowNative {
                let ComputedValue::Ieee754Bits(value) = result.unwrap() else {
                    panic!("POW marker returned a non-IEEE-754 value");
                };
                assert_eq!(value.value(), None);
                assert_eq!(
                    value.metadata(),
                    ComputedIeee754BitsMetadata::OwnIeee754Bits
                );
                assert_eq!(value.into_option(), None);
                invocations += 1;
            } else {
                assert!(matches!(result, Err(LocalError::InvalidBatch(_))));
            }
            assert_eq!(worker.kernel_invocations(), invocations);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let ComputedValue::Ieee754Bits(value) = worker
            .eval_args(EvaluatedArgs::Ieee754Bits2 {
                left: Value(Some(2.0_f64.to_bits())),
                right: Value(Some(4.0_f64.to_bits())),
            })
            .unwrap()
        else {
            panic!("binary IEEE-754 role reuse returned a non-IEEE-754 value");
        };
        let expected = if operation == EvaluatedBytesOp::PowNative {
            16.0_f64
        } else {
            2.0_f64
        };
        assert_eq!(value.value(), Some(expected.to_bits()));
        assert_eq!(
            value.metadata(),
            ComputedIeee754BitsMetadata::OwnIeee754Bits
        );
        assert_eq!(value.into_option(), Some(expected.to_bits()));
        assert_eq!(worker.kernel_invocations(), invocations + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_bytes_uncompressed_length_native_is_quiet_and_unsigned() {
    let (wire_result, wire_ctx) = crate::test_util::RpnFnScalarEvaluator::new()
        .push_param(Some(b"\x78\x56\x34\x12".to_vec()))
        .evaluate_raw(FieldTypeTp::LongLong, ScalarFuncSig::UncompressedLength);
    assert!(matches!(wire_result.unwrap(), ScalarValue::Int(Some(0))));
    assert_eq!(wire_ctx.warnings.warning_cnt, 1);
    assert_eq!(wire_ctx.warnings.warnings.len(), 1);
    assert_eq!(wire_ctx.warnings.warnings[0].get_code(), 1259);
    let cases: &[(Option<&[u8]>, Option<i64>)] = &[
        (None, None),
        (Some(b""), Some(0)),
        (Some(b"x"), Some(0)),
        (Some(b"\x78\x56\x34\x12"), Some(0)),
        (Some(b"\x78\x56\x34\x12\x00"), Some(305_419_896)),
        (Some(b"\xff\xff\xff\xff\x00"), Some(4_294_967_295)),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::UncompressedLengthNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(
        worker.operation(),
        EvaluatedBytesOp::UncompressedLengthNative
    );
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, &(input, expected)) in cases.iter().enumerate() {
        let ComputedValue::Int(value) = worker.eval_one(input.map(|bytes| bytes.to_vec())).unwrap()
        else {
            panic!("UNCOMPRESSED_LENGTH native returned a non-Int value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_insert_preserves_byte_and_character_boundaries() {
    use tidb_query_datatype::codec::data_type::Bytes;

    for signature in [ScalarFuncSig::Insert, ScalarFuncSig::InsertUtf8] {
        let wire = crate::test_util::RpnFnScalarEvaluator::new()
            .push_param(Some("中a".as_bytes().to_vec()))
            .push_param(Some(2_i64))
            .push_param(Some(1_i64))
            .push_param(Some(b"X".to_vec()))
            .evaluate::<Bytes>(signature)
            .unwrap();
        assert_eq!(wire, Some(b"\xe4X\xada".to_vec()));
    }
    assert!(
        crate::test_util::RpnFnScalarEvaluator::new()
            .push_param(Some("中a".as_bytes().to_vec()))
            .push_param(Some(2_i64))
            .push_param(Some(1_i64))
            .push_param(Some(b"\xff".to_vec()))
            .evaluate::<Bytes>(ScalarFuncSig::InsertUtf8)
            .is_err()
    );
    let cases: &[(
        Option<&[u8]>,
        Option<i64>,
        Option<i64>,
        Option<&[u8]>,
        [Option<&[u8]>; 2],
    )] = &[
        (
            Some("中a".as_bytes()),
            Some(2),
            Some(1),
            Some(b"X"),
            [Some(b"\xe4X\xada"), Some("中X".as_bytes())],
        ),
        (
            Some("中a".as_bytes()),
            Some(2),
            Some(1),
            Some(b"\xff"),
            [Some(b"\xe4\xff\xada"), Some(b"\xe4\xb8\xad\xff")],
        ),
        (
            Some("中a".as_bytes()),
            Some(i64::MAX),
            Some(1),
            Some(b"X"),
            [Some("中a".as_bytes()); 2],
        ),
        (
            Some("中a".as_bytes()),
            Some(0),
            Some(1),
            Some(b"X"),
            [Some("中a".as_bytes()); 2],
        ),
        (
            Some("中a".as_bytes()),
            Some(2),
            Some(i64::MIN),
            Some(b"X"),
            [Some(b"\xe4X"), Some("中X".as_bytes())],
        ),
        (None, Some(2), Some(1), Some(b"X"), [None; 2]),
        (Some("中a".as_bytes()), Some(2), Some(1), None, [None; 2]),
    ];
    for (column, operation) in [EvaluatedBytesOp::Insert, EvaluatedBytesOp::InsertUtf8Native]
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
        for (index, &(input, position, length, replacement, expected)) in cases.iter().enumerate() {
            let args = EvaluatedArgs::BytesIntIntBytes(
                input.map(|bytes| bytes.to_vec()),
                position,
                length,
                replacement.map(|bytes| bytes.to_vec()),
            );
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!("INSERT returned a non-Bytes value");
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
fn local_evaluated_bytes_ascii_case_conversion_preserves_high_bytes() {
    let cases: &[(Option<&[u8]>, [Option<&[u8]>; 2])] = &[
        (None, [None, None]),
        (Some(b""), [Some(b""), Some(b"")]),
        (
            Some(b"aZ\xff\xc3\xa9"),
            [Some(b"az\xff\xc3\xa9"), Some(b"AZ\xff\xc3\xa9")],
        ),
    ];
    for (column, operation) in [
        EvaluatedBytesOp::LowerAsciiNative,
        EvaluatedBytesOp::UpperAsciiNative,
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
            let args = EvaluatedArgs::Bytes(input.map(|bytes| bytes.to_vec()));
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!("ASCII case conversion returned a non-Bytes value");
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
fn local_evaluated_args_native_substrings_preserve_units_and_nulls() {
    let two_cases: &[([Option<&[u8]>; 2], Option<i64>, [Option<&[u8]>; 2])] = &[
        ([Some(b"abcd"); 2], Some(2), [Some(b"bcd"); 2]),
        (
            [Some("中abc".as_bytes()); 2],
            Some(2),
            [Some(b"\xb8\xadabc"), Some(b"abc")],
        ),
        (
            [Some(b"\xe2\x82a"), Some("\u{fffd}\u{fffd}a".as_bytes())],
            Some(2),
            [Some(b"\x82a"), Some("\u{fffd}a".as_bytes())],
        ),
        ([Some(b"abcd"); 2], Some(0), [Some(b""); 2]),
        ([None; 2], Some(2), [None; 2]),
        ([Some(b"abcd"); 2], None, [None; 2]),
    ];
    for (column, operation) in [
        EvaluatedBytesOp::Substring2BytesNative,
        EvaluatedBytesOp::Substring2Utf8Native,
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
        for (index, &(input, position, expected)) in two_cases.iter().enumerate() {
            let args = EvaluatedArgs::Substring2Ready {
                bytes: ReadyBytesArg::Value(input[column].map(|bytes| bytes.to_vec())),
                pos: ReadyIntArg::Value(position),
            };
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!("native SUBSTRING2 returned a non-Bytes value");
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
    let three_cases: &[(
        [Option<&[u8]>; 2],
        Option<i64>,
        Option<i64>,
        [Option<&[u8]>; 2],
    )] = &[
        (
            [Some("中abc".as_bytes()); 2],
            Some(2),
            Some(1),
            [Some(b"\xb8"), Some(b"a")],
        ),
        (
            [Some(b"\xe2\x82a"), Some("\u{fffd}\u{fffd}a".as_bytes())],
            Some(2),
            Some(1),
            [Some(b"\x82"), Some("\u{fffd}".as_bytes())],
        ),
        ([Some(b"abcd"); 2], Some(2), Some(i64::MAX), [Some(b""); 2]),
        ([Some(b"abcd"); 2], Some(2), Some(-1), [Some(b""); 2]),
        ([Some(b"abcd"); 2], Some(0), Some(1), [Some(b""); 2]),
        ([Some(b"abcd"); 2], Some(2), None, [None; 2]),
    ];
    for (column, operation) in [
        EvaluatedBytesOp::Substring3BytesNative,
        EvaluatedBytesOp::Substring3Utf8Native,
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
        for (index, &(input, position, length, expected)) in three_cases.iter().enumerate() {
            let args = EvaluatedArgs::Substring3Ready {
                bytes: ReadyBytesArg::Value(input[column].map(|bytes| bytes.to_vec())),
                pos: ReadyIntArg::Value(position),
                len: ReadyIntArg::Value(length),
            };
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!("native SUBSTRING3 returned a non-Bytes value");
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
fn local_evaluated_args_native_substring_markers_require_true_null() {
    let groups: [(
        EvaluatedBytesOp,
        Vec<EvaluatedArgs>,
        Vec<(EvaluatedArgs, Option<&[u8]>)>,
    ); 2] = [
        (
            EvaluatedBytesOp::Substring2BytesNative,
            vec![
                EvaluatedArgs::Substring2Ready {
                    bytes: ReadyBytesArg::Undemanded,
                    pos: ReadyIntArg::Undemanded,
                },
                EvaluatedArgs::Substring2Ready {
                    bytes: ReadyBytesArg::Value(Some(Vec::new())),
                    pos: ReadyIntArg::Undemanded,
                },
                EvaluatedArgs::Substring2Ready {
                    bytes: ReadyBytesArg::Undemanded,
                    pos: ReadyIntArg::Value(Some(0)),
                },
            ],
            vec![
                (
                    EvaluatedArgs::Substring2Ready {
                        bytes: ReadyBytesArg::Value(None),
                        pos: ReadyIntArg::Undemanded,
                    },
                    None,
                ),
                (
                    EvaluatedArgs::Substring2Ready {
                        bytes: ReadyBytesArg::Undemanded,
                        pos: ReadyIntArg::Value(None),
                    },
                    None,
                ),
                (
                    EvaluatedArgs::Substring2Ready {
                        bytes: ReadyBytesArg::Value(Some(b"abcd".to_vec())),
                        pos: ReadyIntArg::Value(Some(2)),
                    },
                    Some(b"bcd"),
                ),
            ],
        ),
        (
            EvaluatedBytesOp::Substring3Utf8Native,
            vec![
                EvaluatedArgs::Substring3Ready {
                    bytes: ReadyBytesArg::Undemanded,
                    pos: ReadyIntArg::Undemanded,
                    len: ReadyIntArg::Undemanded,
                },
                EvaluatedArgs::Substring3Ready {
                    bytes: ReadyBytesArg::Undemanded,
                    pos: ReadyIntArg::Value(Some(0)),
                    len: ReadyIntArg::Value(Some(1)),
                },
                EvaluatedArgs::Substring3Ready {
                    bytes: ReadyBytesArg::Value(Some(Vec::new())),
                    pos: ReadyIntArg::Undemanded,
                    len: ReadyIntArg::Value(Some(1)),
                },
                EvaluatedArgs::Substring3Ready {
                    bytes: ReadyBytesArg::Value(Some(b"abcd".to_vec())),
                    pos: ReadyIntArg::Value(Some(2)),
                    len: ReadyIntArg::Undemanded,
                },
            ],
            vec![
                (
                    EvaluatedArgs::Substring3Ready {
                        bytes: ReadyBytesArg::Value(None),
                        pos: ReadyIntArg::Undemanded,
                        len: ReadyIntArg::Undemanded,
                    },
                    None,
                ),
                (
                    EvaluatedArgs::Substring3Ready {
                        bytes: ReadyBytesArg::Undemanded,
                        pos: ReadyIntArg::Value(None),
                        len: ReadyIntArg::Undemanded,
                    },
                    None,
                ),
                (
                    EvaluatedArgs::Substring3Ready {
                        bytes: ReadyBytesArg::Undemanded,
                        pos: ReadyIntArg::Undemanded,
                        len: ReadyIntArg::Value(None),
                    },
                    None,
                ),
                (
                    EvaluatedArgs::Substring3Ready {
                        bytes: ReadyBytesArg::Value(Some(b"abcd".to_vec())),
                        pos: ReadyIntArg::Value(Some(2)),
                        len: ReadyIntArg::Value(Some(1)),
                    },
                    Some(b"b"),
                ),
            ],
        ),
    ];
    for (operation, invalid, valid) in groups {
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
        for args in invalid {
            assert!(matches!(
                worker.eval_args(args),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        for (index, (args, expected)) in valid.into_iter().enumerate() {
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!("native substring marker returned a non-Bytes value");
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
fn local_evaluated_args_legacy_substring_length_demand_is_explicit() {
    use ReadySubstringI128::{Undemanded, Value};

    let wide = i128::from(i64::MAX) + 1;
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::Substring3BytesLegacy,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::Substring3BytesLegacy);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    let demands: &[(&[u8], i128, bool, bool)] = &[
        (b"abcd", 0, false, false),
        (b"abcd", 2, false, true),
        (b"abcd", -2, true, true),
        (b"abcd", wide, false, false),
        (b"abcd", 6, true, false),
        ("中a".as_bytes(), 4, false, true),
        ("中a".as_bytes(), 4, true, false),
    ];
    for &(source, position, utf8, expected) in demands {
        assert_eq!(
            super::legacy_substring_needs_len(source, position, utf8),
            expected
        );
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    for invalid in [
        EvaluatedArgs::LegacySubstring3Ready {
            bytes: Some(b"abcd".to_vec()),
            pos: Value(Some(2)),
            len: Undemanded,
        },
        EvaluatedArgs::LegacySubstring3Ready {
            bytes: Some(b"abcd".to_vec()),
            pos: Value(Some(0)),
            len: Value(Some(1)),
        },
        EvaluatedArgs::LegacySubstring3Ready {
            bytes: Some(Vec::new()),
            pos: Undemanded,
            len: Undemanded,
        },
        EvaluatedArgs::LegacySubstring3Ready {
            bytes: None,
            pos: Undemanded,
            len: Value(None),
        },
    ] {
        assert!(matches!(
            worker.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let cases: [(
        Option<&[u8]>,
        ReadySubstringI128,
        ReadySubstringI128,
        Option<&[u8]>,
    ); 10] = [
        (None, Undemanded, Undemanded, None),
        (None, Value(Some(2)), Undemanded, None),
        (Some(b"abcd"), Value(None), Undemanded, None),
        (Some(b"abcd"), Value(Some(0)), Undemanded, None),
        (Some(b"abcd"), Value(Some(wide)), Undemanded, None),
        (Some(b"abcd"), Value(Some(6)), Undemanded, Some(b"")),
        (Some(b"abcd"), Value(Some(2)), Value(None), None),
        (Some(b"abcd"), Value(Some(2)), Value(Some(wide)), None),
        (Some(b"abcd"), Value(Some(-2)), Value(Some(1)), Some(b"c")),
        (Some(b"abcd"), Value(Some(2)), Value(Some(1)), Some(b"b")),
    ];
    for (index, (input, pos, len, expected)) in cases.into_iter().enumerate() {
        let args = EvaluatedArgs::LegacySubstring3Ready {
            bytes: input.map(|bytes| bytes.to_vec()),
            pos,
            len,
        };
        let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
            panic!("legacy substring demand returned a non-Bytes value");
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
fn local_evaluated_args_legacy_substrings_preserve_i128_and_lossy_units() {
    use ReadySubstringI128::{Undemanded, Value};

    let wide = i128::from(i64::MAX) + 1;
    for (column, operation) in [
        EvaluatedBytesOp::Substring2BytesLegacy,
        EvaluatedBytesOp::Substring2Utf8Legacy,
    ]
    .into_iter()
    .enumerate()
    {
        let cases: [(Option<&[u8]>, ReadySubstringI128, [Option<&[u8]>; 2]); 7] = [
            (
                Some(b"\xe2\x82a"),
                Value(Some(2)),
                [Some(b"\x82a"), Some(b"a")],
            ),
            (Some(b"abcd"), Value(Some(-2)), [Some(b"cd"); 2]),
            (Some(b"abcd"), Value(Some(0)), [None; 2]),
            (Some(b"abcd"), Value(Some(wide)), [None; 2]),
            (Some(b"abcd"), Value(None), [None; 2]),
            (None, Undemanded, [None; 2]),
            (None, Value(Some(2)), [None; 2]),
        ];
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
            worker.eval_args(EvaluatedArgs::LegacySubstring2Ready {
                bytes: Some(Vec::new()),
                pos: Undemanded,
            }),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        for (index, (input, pos, expected)) in cases.into_iter().enumerate() {
            let args = EvaluatedArgs::LegacySubstring2Ready {
                bytes: input.map(|bytes| bytes.to_vec()),
                pos,
            };
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!("legacy SUBSTRING2 returned a non-Bytes value");
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
    for (column, operation) in [
        EvaluatedBytesOp::Substring3BytesLegacy,
        EvaluatedBytesOp::Substring3Utf8Legacy,
    ]
    .into_iter()
    .enumerate()
    {
        let unit_length = if column == 0 {
            Value(Some(1))
        } else {
            Undemanded
        };
        let cases: [(
            Option<&[u8]>,
            ReadySubstringI128,
            ReadySubstringI128,
            [Option<&[u8]>; 2],
        ); 3] = [
            (
                Some(b"\xe2\x82a"),
                Value(Some(2)),
                Value(Some(1)),
                [Some(b"\x82"), Some(b"a")],
            ),
            (
                Some("中a".as_bytes()),
                Value(Some(4)),
                unit_length,
                [Some(b"a"), Some(b"")],
            ),
            (None, Undemanded, Undemanded, [None; 2]),
        ];
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
        for (index, (input, pos, len, expected)) in cases.into_iter().enumerate() {
            let args = EvaluatedArgs::LegacySubstring3Ready {
                bytes: input.map(|bytes| bytes.to_vec()),
                pos,
                len,
            };
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!("legacy SUBSTRING3 returned a non-Bytes value");
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
fn local_evaluated_args_strcmp_and_locate2_keep_policies_explicit() {
    use tidb_query_datatype::{Collation, builder::FieldTypeBuilder, codec::data_type::Int};

    let wire = crate::test_util::RpnFnScalarEvaluator::new()
        .return_field_type(
            FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .collation(Collation::Utf8Mb4GeneralCi)
                .build(),
        )
        .push_param(Some(b"e".to_vec()))
        .push_param(Some("é".as_bytes().to_vec()))
        .evaluate::<Int>(ScalarFuncSig::Locate2ArgsUtf8)
        .unwrap();
    assert_eq!(wire, Some(0));
    let comparisons: [(Option<&[u8]>, Option<&[u8]>, NativeCollation, Option<i64>); 4] = [
        (
            Some(b"a"),
            Some(b"a "),
            NativeCollation::Utf8Mb4Bin,
            Some(0),
        ),
        (Some(b"a"), Some(b"a "), NativeCollation::Binary, Some(-1)),
        (
            Some(b"A"),
            Some(b"a "),
            NativeCollation::Utf8Mb4GeneralCi,
            Some(0),
        ),
        (None, Some(b"a"), NativeCollation::Binary, None),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::StrcmpNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::StrcmpNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, (left, right, collation, expected)) in comparisons.into_iter().enumerate() {
        let args = EvaluatedArgs::CollatedBytes2 {
            left: left.map(|bytes| bytes.to_vec()),
            right: right.map(|bytes| bytes.to_vec()),
            collation,
        };
        let ComputedValue::Int(value) = worker.eval_args(args).unwrap() else {
            panic!("native STRCMP returned a non-Int value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let searches: [(
        Option<&[u8]>,
        Option<&[u8]>,
        NativeSearchPolicy,
        Option<i64>,
    ); 4] = [
        (
            Some(b"a"),
            Some("中a".as_bytes()),
            NativeSearchPolicy::Bytes,
            Some(4),
        ),
        (
            Some(b"a"),
            Some("中a".as_bytes()),
            NativeSearchPolicy::Utf8(NativeCollation::Binary),
            Some(2),
        ),
        (
            Some(b"e"),
            Some("é".as_bytes()),
            NativeSearchPolicy::Utf8(NativeCollation::Utf8Mb4GeneralCi),
            Some(1),
        ),
        (None, Some(b"abc"), NativeSearchPolicy::Bytes, None),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::Locate2Native,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::Locate2Native);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, (needle, haystack, policy, expected)) in searches.into_iter().enumerate() {
        let args = EvaluatedArgs::SearchBytes2 {
            needle: needle.map(|bytes| bytes.to_vec()),
            haystack: haystack.map(|bytes| bytes.to_vec()),
            policy,
        };
        let ComputedValue::Int(value) = worker.eval_args(args).unwrap() else {
            panic!("native LOCATE2 returned a non-Int value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_locate3_preserves_position_roles_and_ext_units() {
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::Locate3Native,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::Locate3Native);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (needle, haystack) in [
        (b"a".as_slice(), b"abc".as_slice()),
        (b"".as_slice(), b"".as_slice()),
    ] {
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::SearchBytes2IntReady {
                needle: Some(needle.to_vec()),
                haystack: Some(haystack.to_vec()),
                pos: ReadyIntArg::Undemanded,
                policy: NativeSearchPolicy::Bytes,
            }),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let cases: [(
        Option<&[u8]>,
        Option<&[u8]>,
        ReadyIntArg,
        NativeSearchPolicy,
        Option<i64>,
    ); 6] = [
        (
            Some(b"a"),
            Some(b"abca"),
            ReadyIntArg::Value(Some(2)),
            NativeSearchPolicy::Bytes,
            Some(4),
        ),
        (
            Some(b"a"),
            Some("中a".as_bytes()),
            ReadyIntArg::Value(Some(1)),
            NativeSearchPolicy::Utf8(NativeCollation::Binary),
            Some(2),
        ),
        (
            Some(b"a"),
            Some(b"abc"),
            ReadyIntArg::Value(Some(0)),
            NativeSearchPolicy::Bytes,
            Some(0),
        ),
        (
            Some(b"a"),
            Some(b"abc"),
            ReadyIntArg::Value(None),
            NativeSearchPolicy::Bytes,
            None,
        ),
        (
            None,
            Some(b"abc"),
            ReadyIntArg::Undemanded,
            NativeSearchPolicy::Bytes,
            None,
        ),
        (
            Some(b"a"),
            None,
            ReadyIntArg::Undemanded,
            NativeSearchPolicy::Utf8(NativeCollation::Binary),
            None,
        ),
    ];
    for (index, (needle, haystack, pos, policy, expected)) in cases.into_iter().enumerate() {
        let args = EvaluatedArgs::SearchBytes2IntReady {
            needle: needle.map(|bytes| bytes.to_vec()),
            haystack: haystack.map(|bytes| bytes.to_vec()),
            pos,
            policy,
        };
        let ComputedValue::Int(value) = worker.eval_args(args).unwrap() else {
            panic!("native LOCATE3 returned a non-Int value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let ext_cases: &[(Option<&[u8]>, Option<&[u8]>, Option<i64>, [Option<i64>; 2])] = &[
        (
            Some("\u{fffd}".as_bytes()),
            Some("\u{fffd}\u{fffd}a".as_bytes()),
            Some(2),
            [Some(4), Some(2)],
        ),
        (Some(b"a"), Some(b"abc"), Some(i64::MIN), [Some(0); 2]),
        (Some(b"a"), Some(b"abc"), None, [None; 2]),
    ];
    for (column, operation) in [
        EvaluatedBytesOp::Locate3BytesExtNative,
        EvaluatedBytesOp::Locate3Utf8ExtNative,
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
        for (index, &(needle, haystack, pos, expected)) in ext_cases.iter().enumerate() {
            let args = EvaluatedArgs::BytesBytesInt(
                needle.map(|bytes| bytes.to_vec()),
                haystack.map(|bytes| bytes.to_vec()),
                pos,
            );
            let ComputedValue::Int(value) = worker.eval_args(args).unwrap() else {
                panic!("extended LOCATE3 returned a non-Int value");
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
fn local_evaluated_args_find_in_set_native_uses_no_pad_keys() {
    use tidb_query_datatype::{Collation, builder::FieldTypeBuilder, codec::data_type::Int};

    let wire = crate::test_util::RpnFnScalarEvaluator::new()
        .return_field_type(
            FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .collation(Collation::Utf8Mb4GeneralCi)
                .build(),
        )
        .push_param(Some(b"a".to_vec()))
        .push_param(Some(b"a ,a".to_vec()))
        .evaluate::<Int>(ScalarFuncSig::FindInSet)
        .unwrap();
    assert_eq!(wire, Some(1));
    let cases: &[(Option<&[u8]>, Option<&[u8]>, Option<i64>)] = &[
        (Some(b"a"), Some(b"a ,a"), Some(2)),
        (Some(b""), Some(b""), Some(0)),
        (Some(b""), Some(b",x"), Some(1)),
        (None, Some(b"a"), None),
        (Some(b"a"), None, None),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::FindInSetNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::FindInSetNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, &(needle, list, expected)) in cases.iter().enumerate() {
        let args = EvaluatedArgs::CollatedBytes2 {
            left: needle.map(|bytes| bytes.to_vec()),
            right: list.map(|bytes| bytes.to_vec()),
            collation: NativeCollation::Utf8Mb4GeneralCi,
        };
        let ComputedValue::Int(value) = worker.eval_args(args).unwrap() else {
            panic!("native FIND_IN_SET returned a non-Int value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_prepared_find_in_set_keeps_captured_keys() {
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::FindInSetPreparedNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(
        worker.operation(),
        EvaluatedBytesOp::FindInSetPreparedNative
    );
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    let captured =
        prepare_find_in_set_keys(Some(b"B,x,B"), NativeCollation::Binary, usize::MAX).unwrap();
    let no_pad_keys =
        prepare_find_in_set_keys(Some(b"a ,a"), NativeCollation::Utf8Mb4GeneralCi, usize::MAX)
            .unwrap();
    let null_keys = prepare_find_in_set_keys(None, NativeCollation::Binary, usize::MAX).unwrap();
    let empty_keys =
        prepare_find_in_set_keys(Some(b""), NativeCollation::Binary, usize::MAX).unwrap();
    let leading_empty =
        prepare_find_in_set_keys(Some(b",x"), NativeCollation::Binary, usize::MAX).unwrap();
    assert!(!captured.is_null());
    assert!(!no_pad_keys.is_null());
    assert!(null_keys.is_null());
    assert!(!empty_keys.is_null());
    assert!(!leading_empty.is_null());
    assert!(matches!(
        prepare_find_in_set_keys(Some(b"B"), NativeCollation::Binary, 0),
        Err(LocalError::ResourceLimit(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    assert!(matches!(
        worker.eval_args(EvaluatedArgs::FindInSetPreparedReady {
            needle: ReadyBytesArg::Undemanded,
            keys: empty_keys.clone(),
            collation: NativeCollation::Binary,
        }),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let cases = [
        (
            ReadyBytesArg::Value(Some(b"B".to_vec())),
            captured.clone(),
            NativeCollation::Binary,
            Some(1),
        ),
        // GeneralCi probes use two-byte weights, not the captured Binary key B.
        (
            ReadyBytesArg::Value(Some(b"B".to_vec())),
            captured.clone(),
            NativeCollation::Utf8Mb4GeneralCi,
            Some(0),
        ),
        (
            ReadyBytesArg::Value(Some(b"a".to_vec())),
            no_pad_keys,
            NativeCollation::Utf8Mb4GeneralCi,
            Some(2),
        ),
        (
            ReadyBytesArg::Value(None),
            captured,
            NativeCollation::Binary,
            None,
        ),
        (
            ReadyBytesArg::Undemanded,
            null_keys,
            NativeCollation::Binary,
            None,
        ),
        (
            ReadyBytesArg::Value(Some(Vec::new())),
            empty_keys,
            NativeCollation::Binary,
            Some(0),
        ),
        (
            ReadyBytesArg::Value(Some(Vec::new())),
            leading_empty,
            NativeCollation::Binary,
            Some(1),
        ),
    ];
    for (index, (needle, keys, collation, expected)) in cases.into_iter().enumerate() {
        let args = EvaluatedArgs::FindInSetPreparedReady {
            needle,
            keys,
            collation,
        };
        let ComputedValue::Int(value) = worker.eval_args(args).unwrap() else {
            panic!("prepared FIND_IN_SET returned a non-Int value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_oct_preserves_raw_bits_and_native_whitespace() {
    use tidb_query_datatype::codec::data_type::Bytes;

    let wire = crate::test_util::RpnFnScalarEvaluator::new()
        .push_param(Some("\u{a0}8".as_bytes().to_vec()))
        .evaluate::<Bytes>(ScalarFuncSig::OctString)
        .unwrap();
    assert_eq!(wire, Some(b"0".to_vec()));
    let integer_cases: &[(Option<i64>, Option<&[u8]>)] = &[
        (None, None),
        (Some(i64::MIN), Some(b"1000000000000000000000")),
        (Some(u64::MAX as i64), Some(b"1777777777777777777777")),
        (Some(8), Some(b"10")),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::OctInt,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::OctInt);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, &(input, expected)) in integer_cases.iter().enumerate() {
        let ComputedValue::Bytes(value) = worker.eval_args(EvaluatedArgs::Int(input)).unwrap()
        else {
            panic!("OCT integer returned a non-Bytes value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.into_option(), expected.map(|bytes| bytes.to_vec()));
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let string_cases: &[(Option<&[u8]>, Option<&[u8]>)] = &[
        (None, None),
        (Some(b""), None),
        (Some("\u{a0}8".as_bytes()), Some(b"10")),
        (Some("\u{a0} \t\n".as_bytes()), Some(b"0")),
        (Some(b"\xff"), Some(b"0")),
        (Some(b"-18446744073709551615"), Some(b"1")),
        (
            Some(b"-184467440737095516151"),
            Some(b"1777777777777777777777"),
        ),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::OctStringNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::OctStringNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, &(input, expected)) in string_cases.iter().enumerate() {
        let args = EvaluatedArgs::Bytes(input.map(|bytes| bytes.to_vec()));
        let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
            panic!("OCT native string returned a non-Bytes value");
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
fn local_evaluated_args_concat_preserves_coerced_prefix_and_terminal() {
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::ConcatNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::ConcatNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (total, prefix, terminal) in [
        (0, vec![], ConcatTerminal::Complete),
        (2, vec![Some(b"a".to_vec())], ConcatTerminal::Complete),
        (
            2,
            vec![Some(b"a".to_vec()), Some(b"b".to_vec())],
            ConcatTerminal::InputNull,
        ),
        (
            2,
            vec![Some(b"a".to_vec())],
            ConcatTerminal::PacketExceeded { limit: 1 },
        ),
    ] {
        assert!(matches!(
            prepare_concat_args(ConcatKind::Concat, total, prefix, terminal, usize::MAX),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    assert!(matches!(
        prepare_concat_args(
            ConcatKind::Concat,
            1,
            vec![Some(b"a".to_vec())],
            ConcatTerminal::Complete,
            0,
        ),
        Err(LocalError::ResourceLimit(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let cases: [(usize, Vec<Option<Vec<u8>>>, ConcatTerminal, Option<&[u8]>); 4] = [
        (
            5,
            vec![
                Some(b"a".to_vec()),
                Some(b"b".to_vec()),
                Some(b"c".to_vec()),
                Some(b"d".to_vec()),
                Some(b"e".to_vec()),
            ],
            ConcatTerminal::Complete,
            Some(b"abcde"),
        ),
        (
            1,
            vec![Some(Vec::new())],
            ConcatTerminal::Complete,
            Some(b""),
        ),
        (
            5,
            vec![Some(b"a".to_vec()), None],
            ConcatTerminal::InputNull,
            None,
        ),
        (
            5,
            vec![Some(b"ab".to_vec())],
            ConcatTerminal::PacketExceeded { limit: 1 },
            None,
        ),
    ];
    for (index, (total, prefix, terminal, expected)) in cases.into_iter().enumerate() {
        let prepared =
            prepare_concat_args(ConcatKind::Concat, total, prefix, terminal, usize::MAX).unwrap();
        assert_eq!(worker.kernel_invocations(), index as u64);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let ComputedValue::Bytes(value) = worker
            .eval_args(EvaluatedArgs::ConcatReady(prepared))
            .unwrap()
        else {
            panic!("CONCAT ready prefix returned a non-Bytes value");
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
fn local_evaluated_args_concat_ws_keeps_null_slots_in_packet_budget() {
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::ConcatWsNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::ConcatWsNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (total, prefix, terminal) in [
        (1, vec![Some(b",".to_vec())], ConcatTerminal::Complete),
        (2, vec![None, Some(b"a".to_vec())], ConcatTerminal::Complete),
        (
            4,
            vec![Some(b"xx".to_vec()), None, Some(b"a".to_vec())],
            ConcatTerminal::PacketExceeded { limit: 3 },
        ),
    ] {
        assert!(matches!(
            prepare_concat_args(ConcatKind::ConcatWs, total, prefix, terminal, usize::MAX),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let cases: [(usize, Vec<Option<Vec<u8>>>, ConcatTerminal, Option<&[u8]>); 5] = [
        (
            4,
            vec![
                Some(b"-".to_vec()),
                Some(b"a".to_vec()),
                None,
                Some(b"b".to_vec()),
            ],
            ConcatTerminal::Complete,
            Some(b"a-b"),
        ),
        (
            3,
            vec![Some(b"xx".to_vec()), None, Some(b"a".to_vec())],
            ConcatTerminal::Complete,
            Some(b"a"),
        ),
        (
            2,
            vec![Some(b",".to_vec()), None],
            ConcatTerminal::Complete,
            Some(b""),
        ),
        (3, vec![None], ConcatTerminal::InputNull, None),
        // The original second data slot budgets two separator bytes: 3 > 1.
        (
            4,
            vec![Some(b"xx".to_vec()), None, Some(b"a".to_vec())],
            ConcatTerminal::PacketExceeded { limit: 1 },
            None,
        ),
    ];
    for (index, (total, prefix, terminal, expected)) in cases.into_iter().enumerate() {
        let prepared =
            prepare_concat_args(ConcatKind::ConcatWs, total, prefix, terminal, usize::MAX).unwrap();
        assert_eq!(worker.kernel_invocations(), index as u64);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let ComputedValue::Bytes(value) = worker
            .eval_args(EvaluatedArgs::ConcatReady(prepared))
            .unwrap()
        else {
            panic!("CONCAT_WS ready prefix returned a non-Bytes value");
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
fn local_evaluated_args_elt_uses_sql_offsets_and_full_arity() {
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::EltNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::EltNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, total, expected) in [
        (Some(1), 3, Some(1)),
        (Some(2), 3, Some(2)),
        (Some(0), 3, None),
        (Some(-1), 3, None),
        (None, 3, None),
        (Some(3), 3, None),
        (Some(i64::MAX), usize::MAX, Some(i64::MAX as usize)),
    ] {
        assert_eq!(elt_selected_arg(index, total), expected);
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    for invalid in [
        EvaluatedArgs::EltReady {
            index: Some(1),
            total_sql_arity: 2,
            selected: ReadyBytesArg::Undemanded,
        },
        EvaluatedArgs::EltReady {
            index: Some(1),
            total_sql_arity: 1,
            selected: ReadyBytesArg::Value(Some(b"a".to_vec())),
        },
        EvaluatedArgs::EltReady {
            index: Some(0),
            total_sql_arity: 3,
            selected: ReadyBytesArg::Value(None),
        },
    ] {
        assert!(matches!(
            worker.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let cases: [(Option<i64>, usize, ReadyBytesArg, Option<&[u8]>); 8] = [
        (
            Some(1),
            2,
            ReadyBytesArg::Value(Some(Vec::new())),
            Some(b""),
        ),
        (Some(2), 3, ReadyBytesArg::Value(None), None),
        (
            Some(1),
            2,
            ReadyBytesArg::Value(Some(b"value".to_vec())),
            Some(b"value"),
        ),
        (Some(0), 3, ReadyBytesArg::Undemanded, None),
        (Some(-1), 3, ReadyBytesArg::Undemanded, None),
        (None, 3, ReadyBytesArg::Undemanded, None),
        (Some(3), 3, ReadyBytesArg::Undemanded, None),
        (
            Some(i64::MAX),
            usize::MAX,
            ReadyBytesArg::Value(Some(b"wide".to_vec())),
            Some(b"wide"),
        ),
    ];
    for (invocations, (index, total_sql_arity, selected, expected)) in cases.into_iter().enumerate()
    {
        let args = EvaluatedArgs::EltReady {
            index,
            total_sql_arity,
            selected,
        };
        let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
            panic!("ELT ready selection returned a non-Bytes value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.into_option(), expected.map(|bytes| bytes.to_vec()));
        assert_eq!(worker.kernel_invocations(), invocations as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_field_bytes_preserves_collation_and_prefix_ordinals() {
    use tidb_query_datatype::{
        Collation,
        builder::FieldTypeBuilder,
        codec::data_type::{Bytes, Int},
    };

    let wire = crate::test_util::RpnFnScalarEvaluator::new()
        .return_field_type(
            FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .collation(Collation::Utf8Mb4GeneralCi)
                .build(),
        )
        .push_param(Some(b"A".to_vec()))
        .push_param(None::<Bytes>)
        .push_param(Some(b"a ".to_vec()))
        .evaluate::<Int>(ScalarFuncSig::FieldString)
        .unwrap();
    assert_eq!(wire, Some(2));
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::FieldBytesNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::FieldBytesNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    assert!(field_bytes_equal(b"A", b"a ", NativeCollation::Utf8Mb4GeneralCi).unwrap());
    assert!(!field_bytes_equal(b"a", b"a ", NativeCollation::Binary).unwrap());
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    for (total, prefix, terminal) in [
        (
            3,
            vec![Some(b"a".to_vec()), Some(b"a".to_vec())],
            FieldTerminal::Matched,
        ),
        (2, vec![Some(b"a".to_vec())], FieldTerminal::Exhausted),
    ] {
        assert!(matches!(
            prepare_field_bytes_args(
                total,
                ReadyBytesArg::Value(Some(b"a".to_vec())),
                prefix,
                terminal,
                NativeCollation::Binary,
                usize::MAX,
            ),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    assert!(matches!(
        prepare_field_bytes_args(
            2,
            ReadyBytesArg::Value(Some(b"a".to_vec())),
            vec![Some(b"a".to_vec())],
            FieldTerminal::Matched,
            NativeCollation::Binary,
            0,
        ),
        Err(LocalError::ResourceLimit(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let cases = [
        (
            5,
            ReadyBytesArg::Value(Some(b"A".to_vec())),
            vec![None, Some(b"a ".to_vec())],
            FieldTerminal::Matched,
            Some(2),
        ),
        (
            3,
            ReadyBytesArg::Value(None),
            vec![],
            FieldTerminal::NeedleNull,
            Some(0),
        ),
        (
            3,
            ReadyBytesArg::Undemanded,
            vec![None, None],
            FieldTerminal::Exhausted,
            Some(0),
        ),
        (
            1,
            ReadyBytesArg::Undemanded,
            vec![],
            FieldTerminal::Exhausted,
            Some(0),
        ),
    ];
    for (index, (total, needle, prefix, terminal, expected)) in cases.into_iter().enumerate() {
        let prepared = prepare_field_bytes_args(
            total,
            needle,
            prefix,
            terminal,
            NativeCollation::Utf8Mb4GeneralCi,
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.kernel_invocations(), index as u64);
        let ComputedValue::Int(value) = worker
            .eval_args(EvaluatedArgs::FieldReady(prepared))
            .unwrap()
        else {
            panic!("FIELD bytes returned a non-Int value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_field_numeric_preserves_signedness_and_ieee_equality() {
    let high = 1_u64 << 63;
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::FieldIntNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::FieldIntNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (left, right, expected) in [
        (
            FieldIntValue {
                bits: high,
                unsigned: false,
            },
            FieldIntValue {
                bits: high,
                unsigned: true,
            },
            false,
        ),
        (
            FieldIntValue {
                bits: 7,
                unsigned: false,
            },
            FieldIntValue {
                bits: 7,
                unsigned: true,
            },
            true,
        ),
    ] {
        assert_eq!(field_int_equal(left, right), expected);
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    assert!(matches!(
        prepare_field_int_args(
            2,
            ReadyFieldIntArg::Undemanded,
            vec![Some(FieldIntValue {
                bits: 7,
                unsigned: false
            })],
            FieldTerminal::Exhausted,
            usize::MAX,
        ),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let cases = [
        (
            5,
            ReadyFieldIntArg::Value(Some(FieldIntValue {
                bits: high,
                unsigned: true,
            })),
            vec![
                Some(FieldIntValue {
                    bits: high,
                    unsigned: false,
                }),
                None,
                Some(FieldIntValue {
                    bits: high,
                    unsigned: true,
                }),
            ],
            FieldTerminal::Matched,
            Some(3),
        ),
        (
            3,
            ReadyFieldIntArg::Undemanded,
            vec![None, None],
            FieldTerminal::Exhausted,
            Some(0),
        ),
    ];
    for (index, (total, needle, prefix, terminal, expected)) in cases.into_iter().enumerate() {
        let prepared = prepare_field_int_args(total, needle, prefix, terminal, usize::MAX).unwrap();
        assert_eq!(worker.kernel_invocations(), index as u64);
        let ComputedValue::Int(value) = worker
            .eval_args(EvaluatedArgs::FieldReady(prepared))
            .unwrap()
        else {
            panic!("FIELD int returned a non-Int value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let nan = f64::NAN.to_bits();
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::FieldRealNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::FieldRealNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    assert!(!field_real_equal(nan, nan));
    assert!(field_real_equal((-0.0_f64).to_bits(), 0.0_f64.to_bits()));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    assert!(matches!(
        prepare_field_real_args(
            2,
            ReadyIeee754Arg::Undemanded,
            vec![None],
            FieldTerminal::Exhausted,
            usize::MAX,
        ),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let cases = [
        (
            2,
            ReadyIeee754Arg::Value(Some(nan)),
            vec![Some(nan)],
            FieldTerminal::Exhausted,
            Some(0),
        ),
        (
            4,
            ReadyIeee754Arg::Value(Some((-0.0_f64).to_bits())),
            vec![None, Some(0.0_f64.to_bits())],
            FieldTerminal::Matched,
            Some(2),
        ),
        (
            2,
            ReadyIeee754Arg::Value(Some(1.0_f64.to_bits())),
            vec![None],
            FieldTerminal::Exhausted,
            Some(0),
        ),
        (
            2,
            ReadyIeee754Arg::Value(None),
            vec![],
            FieldTerminal::NeedleNull,
            Some(0),
        ),
    ];
    for (index, (total, needle, prefix, terminal, expected)) in cases.into_iter().enumerate() {
        let prepared =
            prepare_field_real_args(total, needle, prefix, terminal, usize::MAX).unwrap();
        assert_eq!(worker.kernel_invocations(), index as u64);
        let ComputedValue::Int(value) = worker
            .eval_args(EvaluatedArgs::FieldReady(prepared))
            .unwrap()
        else {
            panic!("FIELD real returned a non-Int value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_make_set_preserves_selection_and_shift_profile() {
    use tidb_query_datatype::codec::data_type::Bytes;

    let mut wire = crate::test_util::RpnFnScalarEvaluator::new()
        .push_param(Some(1_i64))
        .push_param(Some(b"first".to_vec()));
    for _ in 1..64 {
        wire = wire.push_param(None::<Bytes>);
    }
    let wire = wire
        .push_param(Some(b"tail".to_vec()))
        .evaluate::<Bytes>(ScalarFuncSig::MakeSet)
        .unwrap();
    assert_eq!(wire, Some(b"first".to_vec()));
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::MakeSetNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::MakeSetNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    assert!(!make_set_selected(0, 63));
    assert!(make_set_selected(1_u64 << 63, 63));
    // Compare only the active profile; neither panic nor wrapping is assumed.
    for mask in [0_u64, 1_u64] {
        let index = std::hint::black_box(64_usize);
        let original = std::panic::catch_unwind(|| mask & (1_u64 << index) != 0);
        let shared = std::panic::catch_unwind(|| make_set_selected(mask, index));
        match (original, shared) {
            (Ok(expected), Ok(actual)) => assert_eq!(actual, expected),
            (Err(_), Err(_)) => {}
            _ => panic!("MAKE_SET selector changed the active shift profile"),
        }
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    for (mask, entries) in [
        (1, vec![ReadyBytesArg::Undemanded]),
        (0, vec![ReadyBytesArg::Value(None)]),
    ] {
        assert!(matches!(
            prepare_make_set_args(Some(mask), 2, entries, usize::MAX),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let mut high_entries: Vec<_> = (0..63).map(|_| ReadyBytesArg::Undemanded).collect();
    high_entries.push(ReadyBytesArg::Value(Some(b"high".to_vec())));
    let cases: [(Option<u64>, usize, Vec<ReadyBytesArg>, Option<&[u8]>); 4] = [
        (Some(0), 1, vec![], Some(b"")),
        (None, 4, vec![], None),
        (
            Some(13),
            5,
            vec![
                ReadyBytesArg::Value(Some(Vec::new())),
                ReadyBytesArg::Undemanded,
                ReadyBytesArg::Value(None),
                ReadyBytesArg::Value(Some(b"b".to_vec())),
            ],
            Some(b",b"),
        ),
        (Some(1_u64 << 63), 65, high_entries, Some(b"high")),
    ];
    for (index, (mask, total, entries, expected)) in cases.into_iter().enumerate() {
        let prepared = prepare_make_set_args(mask, total, entries, usize::MAX).unwrap();
        assert_eq!(worker.kernel_invocations(), index as u64);
        let ComputedValue::Bytes(value) = worker
            .eval_args(EvaluatedArgs::MakeSetReady(prepared))
            .unwrap()
        else {
            panic!("MAKE_SET ready entries returned a non-Bytes value");
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
fn local_evaluated_args_export_set_preserves_defaults_clamps_and_null_witnesses() {
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::ExportSetNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::ExportSetNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    assert!(matches!(
        prepare_export_set_args(
            ReadyIntArg::Undemanded,
            ReadyBytesArg::Value(Some(b"Y".to_vec())),
            ReadyBytesArg::Value(Some(b"N".to_vec())),
            None,
            None,
            usize::MAX,
        ),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    assert!(matches!(
        prepare_export_set_args(
            ReadyIntArg::Value(Some(1)),
            ReadyBytesArg::Value(Some(b"Y".to_vec())),
            ReadyBytesArg::Value(Some(b"N".to_vec())),
            None,
            Some(ReadyIntArg::Value(Some(1))),
            usize::MAX,
        ),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let cases: [(
        ReadyIntArg,
        ReadyBytesArg,
        ReadyBytesArg,
        Option<ReadyBytesArg>,
        Option<ReadyIntArg>,
        Option<&[u8]>,
    ); 7] = [
        (
            ReadyIntArg::Value(Some(0)),
            ReadyBytesArg::Value(Some(Vec::new())),
            ReadyBytesArg::Value(Some(Vec::new())),
            None,
            None,
            Some(&[b','; 63]),
        ),
        (
            ReadyIntArg::Value(Some(-1)),
            ReadyBytesArg::Value(Some(Vec::new())),
            ReadyBytesArg::Value(Some(b"x".to_vec())),
            Some(ReadyBytesArg::Value(Some(Vec::new()))),
            None,
            Some(b"x"),
        ),
        (
            ReadyIntArg::Value(Some(-1)),
            ReadyBytesArg::Value(Some(Vec::new())),
            ReadyBytesArg::Value(Some(b"x".to_vec())),
            Some(ReadyBytesArg::Value(Some(Vec::new()))),
            Some(ReadyIntArg::Value(Some(0))),
            Some(b""),
        ),
        (
            ReadyIntArg::Value(Some(-1)),
            ReadyBytesArg::Value(Some(Vec::new())),
            ReadyBytesArg::Value(Some(b"x".to_vec())),
            Some(ReadyBytesArg::Value(Some(Vec::new()))),
            Some(ReadyIntArg::Value(Some(65))),
            Some(b"x"),
        ),
        (
            ReadyIntArg::Value(Some(-1)),
            ReadyBytesArg::Value(Some(Vec::new())),
            ReadyBytesArg::Value(Some(b"x".to_vec())),
            Some(ReadyBytesArg::Value(Some(Vec::new()))),
            Some(ReadyIntArg::Value(Some(-1))),
            Some(b"x"),
        ),
        (
            ReadyIntArg::Undemanded,
            ReadyBytesArg::Value(Some(b"Y".to_vec())),
            ReadyBytesArg::Value(None),
            None,
            None,
            None,
        ),
        (
            ReadyIntArg::Undemanded,
            ReadyBytesArg::Value(Some(b"Y".to_vec())),
            ReadyBytesArg::Value(Some(b"N".to_vec())),
            Some(ReadyBytesArg::Value(Some(b",".to_vec()))),
            Some(ReadyIntArg::Value(None)),
            None,
        ),
    ];
    for (index, (bits, on, off, separator, count, expected)) in cases.into_iter().enumerate() {
        let prepared =
            prepare_export_set_args(bits, on, off, separator, count, usize::MAX).unwrap();
        assert_eq!(worker.kernel_invocations(), index as u64);
        let ComputedValue::Bytes(value) = worker
            .eval_args(EvaluatedArgs::ExportSetReady(prepared))
            .unwrap()
        else {
            panic!("EXPORT_SET ready arguments returned a non-Bytes value");
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
fn local_evaluated_args_decimal_owns_spilled_results_and_checked_ceil_view() {
    use tidb_query_datatype::codec::mysql::Decimal;

    let wide = Decimal::try_from_native_digits(false, &[b'9'; 90], 0, 0, usize::MAX).unwrap();
    assert!(wide.words().words.len() > 9);
    let retained = {
        let mut worker = prepare_evaluated_bytes(
            EvaluatedBytesOp::AbsDecimalNative,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), EvaluatedBytesOp::AbsDecimalNative);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        let cases = [
            (None, None),
            (Some(Decimal::from(-7_i64)), Some(Decimal::from(7_i64))),
            (Some(wide.clone()), Some(wide.clone())),
        ];
        let mut retained = None;
        for (index, (input, expected)) in cases.into_iter().enumerate() {
            let ComputedValue::Decimal(value) =
                worker.eval_args(EvaluatedArgs::Decimal(input)).unwrap()
            else {
                panic!("ABS Decimal returned an unexpected output type");
            };
            assert_eq!(value.value(), expected.as_ref());
            assert_eq!(value.metadata(), ComputedDecimalMetadata::OwnDecimal);
            retained = value.into_option();
            assert_eq!(retained.as_ref(), expected.as_ref());
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        retained
    };
    assert_eq!(retained.as_ref(), Some(&wide));
    assert!(retained.as_ref().unwrap().words().words.len() > 9);
    let fractional = Decimal::try_from_native_digits(false, b"12", 1, 1, usize::MAX).unwrap();
    let cases = [
        (None, None, None),
        (Some(fractional), Some(Decimal::from(2_i64)), Some(2)),
        (Some(wide.clone()), Some(wide), None),
    ];
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::CeilDecimalNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::CeilDecimalNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, (input, expected, checked)) in cases.into_iter().enumerate() {
        let ComputedValue::Decimal(value) =
            worker.eval_args(EvaluatedArgs::Decimal(input)).unwrap()
        else {
            panic!("CEIL Decimal returned an unexpected output type");
        };
        assert_eq!(value.value(), expected.as_ref());
        assert_eq!(value.metadata(), ComputedDecimalMetadata::OwnDecimal);
        assert_eq!(value.checked_i64_view(), checked);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_int128_null_witness_and_round_policy_keep_carriers() {
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::RoundInt128Legacy,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::RoundInt128Legacy);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for (index, input) in [
        None,
        Some(i128::from(i64::MAX) + 1),
        Some(i128::from(i64::MIN) - 1),
    ]
    .into_iter()
    .enumerate()
    {
        let ComputedValue::Int128(value) = worker.eval_args(EvaluatedArgs::Int128(input)).unwrap()
        else {
            panic!("legacy integer ROUND returned a non-Int128 value");
        };
        assert_eq!(value.value(), input);
        assert_eq!(value.metadata(), ComputedInt128Metadata::OwnInt128);
        assert_eq!(value.into_option(), input);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::MathNullWitnessNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::MathNullWitnessNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for invalid in [
        EvaluatedArgs::NullWitness(Some(0)),
        EvaluatedArgs::Int(None),
    ] {
        assert!(matches!(
            worker.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let ComputedValue::Int(value) = worker.eval_args(EvaluatedArgs::NullWitness(None)).unwrap()
    else {
        panic!("NULL witness returned a non-Int value");
    };
    assert_eq!(value.value(), None);
    assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
    assert_eq!(value.into_option(), None);
    assert_eq!(worker.kernel_invocations(), 1);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    for (operation, args, expected) in [
        (
            EvaluatedBytesOp::RoundRealNative,
            EvaluatedArgs::Ieee754BitsInt {
                value: Some(2.5_f64.to_bits()),
                scale: Some(0),
            },
            Some(2.0_f64.to_bits()),
        ),
        (
            EvaluatedBytesOp::RoundRealLegacy,
            EvaluatedArgs::Ieee754Bits(Some(2.5_f64.to_bits())),
            Some(3.0_f64.to_bits()),
        ),
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
        let ComputedValue::Ieee754Bits(value) = worker.eval_args(args).unwrap() else {
            panic!("real ROUND returned a non-IEEE754 value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(
            value.metadata(),
            ComputedIeee754BitsMetadata::OwnIeee754Bits
        );
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_decimal_ready_budget_and_null_markers_are_distinct() {
    use tidb_query_common::error::{ErrorInner, EvaluateError};
    use tidb_query_datatype::codec::mysql::decimal::{Decimal, NativeDecimalError};

    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::RoundDecimalNative,
        LocalCompileContext::default(),
        ExecutionLimits {
            max_retained_bytes: 16 * 1024,
            ..ExecutionLimits::default()
        },
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::RoundDecimalNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for invalid in [
        EvaluatedArgs::Decimal(None),
        EvaluatedArgs::DecimalIntReady {
            value: ReadyDecimalArg::Undemanded,
            scale: ReadyIntArg::Undemanded,
        },
        EvaluatedArgs::DecimalIntReady {
            value: ReadyDecimalArg::Value(Some(Decimal::from(1_i64))),
            scale: ReadyIntArg::Undemanded,
        },
    ] {
        let failure = worker.eval_args_reported(invalid).unwrap_err();
        assert!(matches!(failure.error(), LocalError::InvalidBatch(_)));
        assert!(failure.sql_failure().is_none());
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    // A resolved i32 ready-boundary scale, not SQL ROUND's frontend scale policy.
    let failure = worker
        .eval_args_reported(EvaluatedArgs::DecimalIntReady {
            value: ReadyDecimalArg::Value(Some(Decimal::from(1_i64))),
            scale: ReadyIntArg::Value(Some(100_000)),
        })
        .unwrap_err();
    assert!(matches!(
        failure.error(),
        LocalError::Evaluation(error)
            if matches!(
                error.0.as_ref(),
                ErrorInner::Evaluate(EvaluateError::Caused(source))
                    if matches!(source.downcast_ref::<NativeDecimalError>(), Some(NativeDecimalError::Resource(_)))
            )
    ));
    assert!(failure.sql_failure().is_none());
    let preserved = failure.into_error();
    assert!(matches!(
        preserved,
        LocalError::Evaluation(error)
            if matches!(
                error.0.as_ref(),
                ErrorInner::Evaluate(EvaluateError::Caused(source))
                    if matches!(source.downcast_ref::<NativeDecimalError>(), Some(NativeDecimalError::Resource(_)))
            )
    ));
    assert_eq!(worker.kernel_invocations(), 1);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let cases = [
        (ReadyDecimalArg::Value(None), ReadyIntArg::Undemanded, None),
        (ReadyDecimalArg::Undemanded, ReadyIntArg::Value(None), None),
        (
            ReadyDecimalArg::Value(Some(Decimal::from(1_i64))),
            ReadyIntArg::Value(Some(0)),
            Some(Decimal::from(1_i64)),
        ),
    ];
    for (index, (value, scale, expected)) in cases.into_iter().enumerate() {
        let ComputedValue::Decimal(value) = worker
            .eval_args(EvaluatedArgs::DecimalIntReady { value, scale })
            .unwrap()
        else {
            panic!("ready decimal ROUND returned an unexpected output type");
        };
        assert_eq!(value.value(), expected.as_ref());
        assert_eq!(value.metadata(), ComputedDecimalMetadata::OwnDecimal);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 2);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_abs_failure_receipt_preserves_primary_and_call_scope() {
    use tidb_query_common::error::{ErrorInner, EvaluateError};

    let wire = crate::test_util::RpnFnScalarEvaluator::new()
        .push_param(Some(i64::MIN))
        .evaluate::<i64>(ScalarFuncSig::AbsInt)
        .unwrap_err();
    let ErrorInner::Evaluate(wire_cause) = wire.0.as_ref() else {
        panic!("wire ABS overflow lost its evaluation cause");
    };
    assert_eq!(wire_cause.code(), 1690);
    assert!(!matches!(
        wire_cause,
        EvaluateError::AbsSignedOverflow { .. }
    ));
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::AbsIntNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::AbsIntNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    let failure = worker
        .eval_args_reported(EvaluatedArgs::Bytes(None))
        .unwrap_err();
    assert!(matches!(failure.error(), LocalError::InvalidBatch(_)));
    assert!(failure.sql_failure().is_none());
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let ComputedValue::Int(value) = worker.eval_args(EvaluatedArgs::Int(Some(-7))).unwrap() else {
        panic!("native ABS returned a non-Int value");
    };
    assert_eq!(value.value(), Some(7));
    assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
    assert_eq!(value.into_option(), Some(7));
    assert_eq!(worker.kernel_invocations(), 1);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let failure = worker
        .eval_args_reported(EvaluatedArgs::Int(Some(i64::MIN)))
        .unwrap_err();
    assert_eq!(failure.operation(), Some(EvaluatedBytesOp::AbsIntNative));
    assert!(matches!(
        failure.sql_failure(),
        Some(EvaluatedSqlFailureKind::AbsSignedOverflow)
    ));
    assert!(matches!(
        failure.error(),
        LocalError::Evaluation(error)
            if matches!(error.0.as_ref(), ErrorInner::Evaluate(cause) if cause.code() == 1690)
    ));
    let primary_text = failure.error().to_string();
    let primary = failure.into_error();
    assert_eq!(primary.to_string(), primary_text);
    assert!(matches!(primary, LocalError::Evaluation(_)));
    // The qualifying failure is this call's single dispatch, not cumulative one.
    assert_eq!(worker.kernel_invocations(), 2);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let ComputedValue::Int(value) = worker.eval_args_reported(EvaluatedArgs::Int(None)).unwrap()
    else {
        panic!("nullable native ABS returned a non-Int value");
    };
    assert_eq!(value.value(), None);
    assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
    assert_eq!(value.into_option(), None);
    assert_eq!(worker.kernel_invocations(), 3);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let plain = worker
        .eval_args(EvaluatedArgs::Int(Some(i64::MIN)))
        .unwrap_err();
    assert_eq!(plain.to_string(), primary_text);
    assert!(matches!(plain, LocalError::Evaluation(_)));
    assert_eq!(worker.kernel_invocations(), 4);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let ComputedValue::Int(value) = worker.eval_args(EvaluatedArgs::Int(Some(-1))).unwrap() else {
        panic!("reused native ABS returned a non-Int value");
    };
    assert_eq!(value.value(), Some(1));
    assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
    assert_eq!(value.into_option(), Some(1));
    assert_eq!(worker.kernel_invocations(), 5);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
}

#[test]
fn local_evaluated_args_char_packs_full_values_and_nullable_entries() {
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::CharNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::CharNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    assert!(matches!(
        worker.eval_args(EvaluatedArgs::Bytes(None)),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let cases: &[(&[Option<i64>], &[u8])] = &[
        (&[], b""),
        (&[None, None], b""),
        (
            &[Some(0), None, Some(0x4142), Some(-1), Some(1_i64 << 32)],
            b"\0AB\xff\xff\xff\xff\0\0\0\0",
        ),
    ];
    for (index, &(input, expected)) in cases.iter().enumerate() {
        let args = prepare_char_args(input).unwrap();
        assert_eq!(worker.kernel_invocations(), index as u64);
        let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
            panic!("native CHAR returned a non-Bytes value");
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
fn local_evaluated_args_conv_legacy_preserves_i128_and_demanded_prefix() {
    use tidb_query_common::error::{ErrorInner, EvaluateError};
    use tidb_query_datatype::codec::data_type::Bytes;

    let wire = crate::test_util::RpnFnScalarEvaluator::new()
        .push_param(Some(b"18446744073709551616".to_vec()))
        .push_param(Some(10_i64))
        .push_param(Some(10_i64))
        .evaluate::<Bytes>(ScalarFuncSig::Conv)
        .unwrap_err();
    assert!(matches!(
        wire.0.as_ref(),
        ErrorInner::Evaluate(EvaluateError::Custom { code: 1690, .. })
    ));
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::ConvLegacy,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::ConvLegacy);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for invalid in [
        EvaluatedArgs::ConvLegacyReady {
            number: ReadyBytesArg::Undemanded,
            from_base: ReadyConvBaseArg::Value(None),
            to_base: ReadyConvBaseArg::Undemanded,
        },
        EvaluatedArgs::ConvLegacyReady {
            number: ReadyBytesArg::Value(None),
            from_base: ReadyConvBaseArg::Value(Some(10)),
            to_base: ReadyConvBaseArg::Undemanded,
        },
        EvaluatedArgs::ConvLegacyReady {
            number: ReadyBytesArg::Value(Some(b"1".to_vec())),
            from_base: ReadyConvBaseArg::Value(Some(1)),
            to_base: ReadyConvBaseArg::Undemanded,
        },
    ] {
        let failure = worker.eval_args_reported(invalid).unwrap_err();
        assert!(matches!(failure.error(), LocalError::InvalidBatch(_)));
        assert!(failure.sql_failure().is_none());
        assert!(failure.conv_overflow_digits().is_none());
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let cases: [(
        Option<&[u8]>,
        ReadyConvBaseArg,
        ReadyConvBaseArg,
        Option<&[u8]>,
    ); 7] = [
        (
            Some(b"FF"),
            ReadyConvBaseArg::Value(Some(16)),
            ReadyConvBaseArg::Value(Some(10)),
            Some(b"255"),
        ),
        (
            None,
            ReadyConvBaseArg::Undemanded,
            ReadyConvBaseArg::Undemanded,
            None,
        ),
        (
            Some(b"1"),
            ReadyConvBaseArg::Value(None),
            ReadyConvBaseArg::Undemanded,
            None,
        ),
        (
            Some(b"1"),
            ReadyConvBaseArg::Value(Some(i128::from(i64::MAX) + 1)),
            ReadyConvBaseArg::Undemanded,
            None,
        ),
        (
            Some(b"1"),
            ReadyConvBaseArg::Value(Some(10)),
            ReadyConvBaseArg::Value(Some(i128::from(i64::MIN) - 1)),
            None,
        ),
        (
            Some(b"1"),
            ReadyConvBaseArg::Value(Some(1)),
            ReadyConvBaseArg::Value(Some(10)),
            None,
        ),
        (
            Some(b"18446744073709551616"),
            ReadyConvBaseArg::Value(Some(10)),
            ReadyConvBaseArg::Value(Some(10)),
            None,
        ),
    ];
    for (index, (number, from_base, to_base, expected)) in cases.into_iter().enumerate() {
        let args = EvaluatedArgs::ConvLegacyReady {
            number: ReadyBytesArg::Value(number.map(|bytes| bytes.to_vec())),
            from_base,
            to_base,
        };
        let ComputedValue::Bytes(value) = worker.eval_args_reported(args).unwrap() else {
            panic!("legacy CONV returned a non-Bytes value");
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
fn local_evaluated_args_conv_native_preserves_markers_and_overflow_receipts() {
    use std::num::ParseIntError;

    use tidb_query_common::error::{ErrorInner, EvaluateError};

    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::ConvNative,
        LocalCompileContext::default(),
        ExecutionLimits {
            max_retained_bytes: 8 * 1024,
            ..ExecutionLimits::default()
        },
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::ConvNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for invalid in [
        EvaluatedArgs::ConvReady {
            number: ReadyBytesArg::Value(None),
            from_base: ReadyIntArg::Undemanded,
            to_base: ReadyIntArg::Value(Some(10)),
        },
        EvaluatedArgs::ConvReady {
            number: ReadyBytesArg::Undemanded,
            from_base: ReadyIntArg::Value(Some(10)),
            to_base: ReadyIntArg::Value(Some(16)),
        },
        EvaluatedArgs::ConvReady {
            number: ReadyBytesArg::Value(Some(b"1".to_vec())),
            from_base: ReadyIntArg::Value(Some(1)),
            to_base: ReadyIntArg::Undemanded,
        },
    ] {
        let failure = worker.eval_args_reported(invalid).unwrap_err();
        assert!(matches!(failure.error(), LocalError::InvalidBatch(_)));
        assert!(failure.sql_failure().is_none());
        assert!(failure.conv_overflow_digits().is_none());
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let mut oversized = Vec::with_capacity(16 * 1024);
    oversized.push(b'1');
    let failure = worker
        .eval_args_reported(EvaluatedArgs::ConvReady {
            number: ReadyBytesArg::Value(Some(oversized)),
            from_base: ReadyIntArg::Value(Some(10)),
            to_base: ReadyIntArg::Value(Some(16)),
        })
        .unwrap_err();
    assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
    assert!(failure.sql_failure().is_none());
    assert!(failure.conv_overflow_digits().is_none());
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let cases: [(ReadyBytesArg, ReadyIntArg, ReadyIntArg, Option<&[u8]>); 6] = [
        (
            ReadyBytesArg::Value(Some(b"-1".to_vec())),
            ReadyIntArg::Value(Some(10)),
            ReadyIntArg::Value(Some(16)),
            Some(b"FFFFFFFFFFFFFFFF"),
        ),
        (
            ReadyBytesArg::Value(Some(b"18446744073709551615".to_vec())),
            ReadyIntArg::Value(Some(10)),
            ReadyIntArg::Value(Some(-10)),
            Some(b"-1"),
        ),
        (
            ReadyBytesArg::Value(None),
            ReadyIntArg::Value(Some(10)),
            ReadyIntArg::Value(Some(16)),
            None,
        ),
        (
            ReadyBytesArg::Undemanded,
            ReadyIntArg::Value(None),
            ReadyIntArg::Undemanded,
            None,
        ),
        (
            ReadyBytesArg::Undemanded,
            ReadyIntArg::Undemanded,
            ReadyIntArg::Value(None),
            None,
        ),
        (
            ReadyBytesArg::Value(Some(b"10".to_vec())),
            ReadyIntArg::Value(Some(1)),
            ReadyIntArg::Value(Some(10)),
            None,
        ),
    ];
    for (index, (number, from_base, to_base, expected)) in cases.into_iter().enumerate() {
        let args = EvaluatedArgs::ConvReady {
            number,
            from_base,
            to_base,
        };
        let ComputedValue::Bytes(value) = worker.eval_args_reported(args).unwrap() else {
            panic!("native CONV returned a non-Bytes value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.into_option(), expected.map(|bytes| bytes.to_vec()));
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let failure = worker
        .eval_args_reported(EvaluatedArgs::ConvReady {
            number: ReadyBytesArg::Value(Some(b"  -18446744073709551616tail ".to_vec())),
            from_base: ReadyIntArg::Value(Some(10)),
            to_base: ReadyIntArg::Value(Some(16)),
        })
        .unwrap_err();
    assert_eq!(failure.operation(), Some(EvaluatedBytesOp::ConvNative));
    assert!(matches!(
        failure.sql_failure(),
        Some(EvaluatedSqlFailureKind::ConvUnsignedOverflow)
    ));
    assert_eq!(failure.conv_overflow_digits(), Some("18446744073709551616"));
    let LocalError::Evaluation(error) = failure.error() else {
        panic!("native CONV overflow lost its primary evaluation error");
    };
    let ErrorInner::Evaluate(cause) = error.0.as_ref() else {
        panic!("native CONV overflow lost its typed evaluation cause");
    };
    assert_eq!(cause.code(), 1690);
    let EvaluateError::ConvUnsignedOverflow { digits, source } = cause else {
        panic!("native CONV overflow lost its typed digits");
    };
    assert_eq!(digits, "18446744073709551616");
    assert_eq!(source.code(), 10000);
    let EvaluateError::Caused(source) = source.as_ref() else {
        panic!("native CONV overflow lost its parse cause");
    };
    assert!(source.downcast_ref::<ParseIntError>().is_some());
    let primary = failure.error().to_string();
    assert_eq!(failure.into_error().to_string(), primary);
    assert_eq!(worker.kernel_invocations(), 7);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let ComputedValue::Bytes(value) = worker
        .eval_args_reported(EvaluatedArgs::ConvReady {
            number: ReadyBytesArg::Value(Some(b"0".to_vec())),
            from_base: ReadyIntArg::Value(Some(10)),
            to_base: ReadyIntArg::Value(Some(16)),
        })
        .unwrap()
    else {
        panic!("reused native CONV returned a non-Bytes value");
    };
    assert_eq!(value.value(), Some(b"0".as_slice()));
    assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
    assert_eq!(value.into_option(), Some(b"0".to_vec()));
    assert_eq!(worker.kernel_invocations(), 8);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
}

#[test]
fn local_evaluated_args_conv_binary_literal_keeps_full_payload_and_first_stage() {
    use std::num::ParseIntError;

    use tidb_query_common::error::{ErrorInner, EvaluateError};

    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::ConvBinaryLiteralNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(
        worker.operation(),
        EvaluatedBytesOp::ConvBinaryLiteralNative
    );
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    let cases: &[(&[u8], i64, i64, &[u8])] = &[
        (b"\0\0\xff", 16, 10, b"255"),
        (b"", 10, 16, b"0"),
        (b"\0\0\0\0\0\0\0\0\x01", 2, 16, b"1"),
    ];
    for (index, &(number, from_base, to_base, expected)) in cases.iter().enumerate() {
        let args = EvaluatedArgs::ConvReady {
            number: ReadyBytesArg::Value(Some(number.to_vec())),
            from_base: ReadyIntArg::Value(Some(from_base)),
            to_base: ReadyIntArg::Value(Some(to_base)),
        };
        let ComputedValue::Bytes(value) = worker.eval_args_reported(args).unwrap() else {
            panic!("binary-literal CONV returned a non-Bytes value");
        };
        assert_eq!(value.value(), Some(expected));
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.into_option(), Some(expected.to_vec()));
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    // Stage one strips leading zero bits, but never narrows the nine-byte input.
    let stage_one_digits = concat!(
        "1", "00000000", "00000000", "00000000", "00000000", "00000000", "00000000", "00000000",
        "00000000",
    );
    let failure = worker
        .eval_args_reported(EvaluatedArgs::ConvReady {
            number: ReadyBytesArg::Value(Some(b"\x01\0\0\0\0\0\0\0\0".to_vec())),
            from_base: ReadyIntArg::Value(Some(10)),
            // Invalid final radix must not bypass the first-stage overflow.
            to_base: ReadyIntArg::Value(Some(1)),
        })
        .unwrap_err();
    assert_eq!(
        failure.operation(),
        Some(EvaluatedBytesOp::ConvBinaryLiteralNative)
    );
    assert!(matches!(
        failure.sql_failure(),
        Some(EvaluatedSqlFailureKind::ConvUnsignedOverflow)
    ));
    assert_eq!(failure.conv_overflow_digits(), Some(stage_one_digits));
    let LocalError::Evaluation(error) = failure.error() else {
        panic!("binary-literal CONV overflow lost its evaluation error");
    };
    let ErrorInner::Evaluate(cause) = error.0.as_ref() else {
        panic!("binary-literal CONV overflow lost its typed cause");
    };
    assert_eq!(cause.code(), 1690);
    let EvaluateError::ConvUnsignedOverflow { digits, source } = cause else {
        panic!("binary-literal CONV overflow lost its first-stage digits");
    };
    assert_eq!(digits, stage_one_digits);
    assert_eq!(source.code(), 10000);
    let EvaluateError::Caused(source) = source.as_ref() else {
        panic!("binary-literal CONV overflow lost its parse cause");
    };
    assert!(source.downcast_ref::<ParseIntError>().is_some());
    assert_eq!(worker.kernel_invocations(), 4);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let ComputedValue::Bytes(value) = worker
        .eval_args_reported(EvaluatedArgs::ConvReady {
            number: ReadyBytesArg::Value(None),
            from_base: ReadyIntArg::Value(Some(10)),
            to_base: ReadyIntArg::Value(Some(16)),
        })
        .unwrap()
    else {
        panic!("nullable binary-literal CONV returned a non-Bytes value");
    };
    assert_eq!(value.value(), None);
    assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
    assert_eq!(value.into_option(), None);
    assert_eq!(worker.kernel_invocations(), 5);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
}

#[test]
fn local_evaluated_args_trig_unary_keeps_nullable_and_nonfinite_bit_carriers() {
    let negative_zero = (-0.0_f64).to_bits();
    let nan = 0x7ff8_0000_0000_0123_u64;
    for (operation, zero_result) in [
        (EvaluatedBytesOp::SinGoNative, negative_zero),
        (EvaluatedBytesOp::CosGoNative, 1.0_f64.to_bits()),
        (EvaluatedBytesOp::TanGoNative, negative_zero),
        (EvaluatedBytesOp::CotGoNative, f64::NEG_INFINITY.to_bits()),
        (EvaluatedBytesOp::AtanGoNative, negative_zero),
        (EvaluatedBytesOp::SinLibmLegacy, negative_zero),
        (EvaluatedBytesOp::CosLibmLegacy, 1.0_f64.to_bits()),
        (EvaluatedBytesOp::CotLibmLegacy, f64::NEG_INFINITY.to_bits()),
        (EvaluatedBytesOp::AtanLibmLegacy, negative_zero),
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
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::Bytes(None)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        for (index, (input, expected)) in [(None, None), (Some(negative_zero), Some(zero_result))]
            .into_iter()
            .enumerate()
        {
            let ComputedValue::Ieee754Bits(value) =
                worker.eval_args(EvaluatedArgs::Ieee754Bits(input)).unwrap()
            else {
                panic!("unary trig returned a non-IEEE754 value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(
                value.metadata(),
                ComputedIeee754BitsMetadata::OwnIeee754Bits
            );
            assert_eq!(value.into_option(), expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let ComputedValue::Ieee754Bits(value) = worker
            .eval_args(EvaluatedArgs::Ieee754Bits(Some(nan)))
            .unwrap()
        else {
            panic!("NaN trig returned a non-IEEE754 value");
        };
        let actual = value.value();
        assert!(actual.is_some_and(|bits| f64::from_bits(bits).is_nan()));
        assert_eq!(
            value.metadata(),
            ComputedIeee754BitsMetadata::OwnIeee754Bits
        );
        assert_eq!(value.into_option(), actual);
        assert_eq!(worker.kernel_invocations(), 3);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        if operation == EvaluatedBytesOp::AtanGoNative {
            let ComputedValue::Ieee754Bits(value) = worker
                .eval_args(EvaluatedArgs::Ieee754Bits(Some(f64::INFINITY.to_bits())))
                .unwrap()
            else {
                panic!("infinite-input ATAN returned a non-IEEE754 value");
            };
            let expected = Some(std::f64::consts::FRAC_PI_2.to_bits());
            assert_eq!(value.value(), expected);
            assert_eq!(
                value.metadata(),
                ComputedIeee754BitsMetadata::OwnIeee754Bits
            );
            assert_eq!(value.into_option(), expected);
            assert_eq!(worker.kernel_invocations(), 4);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[test]
fn local_evaluated_args_atan2_preserves_demand_order_orientation_and_raw_bits() {
    let negative_zero = (-0.0_f64).to_bits();
    let one = 1.0_f64.to_bits();
    let nan = 0x7ff8_0000_0000_0123_u64;
    for (operation, legacy) in [
        (EvaluatedBytesOp::Atan2GoNative, false),
        (EvaluatedBytesOp::Atan2LibmLegacy, true),
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
        for invalid in [
            EvaluatedArgs::Bytes(None),
            EvaluatedArgs::Ieee754Bits2 {
                left: ReadyIeee754Arg::Undemanded,
                right: ReadyIeee754Arg::Value(None),
            },
            EvaluatedArgs::Ieee754Bits2 {
                left: ReadyIeee754Arg::Undemanded,
                right: ReadyIeee754Arg::Undemanded,
            },
        ] {
            assert!(matches!(
                worker.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let skipped_right = EvaluatedArgs::Ieee754Bits2 {
            left: ReadyIeee754Arg::Value(None),
            right: ReadyIeee754Arg::Undemanded,
        };
        let preceding = if legacy {
            let ComputedValue::Ieee754Bits(value) = worker.eval_args(skipped_right).unwrap() else {
                panic!("legacy ATAN2 NULL prefix returned a non-IEEE754 value");
            };
            assert_eq!(value.value(), None);
            assert_eq!(
                value.metadata(),
                ComputedIeee754BitsMetadata::OwnIeee754Bits
            );
            assert_eq!(value.into_option(), None);
            1_u64
        } else {
            assert!(matches!(
                worker.eval_args(skipped_right),
                Err(LocalError::InvalidBatch(_))
            ));
            0
        };
        assert_eq!(worker.kernel_invocations(), preceding);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let cases = [
            (None, Some(one), None),
            (Some(one), None, None),
            (
                Some(one),
                Some(0.0_f64.to_bits()),
                Some(std::f64::consts::FRAC_PI_2.to_bits()),
            ),
            (Some(negative_zero), Some(one), Some(negative_zero)),
        ];
        for (index, (left, right, expected)) in cases.into_iter().enumerate() {
            let ComputedValue::Ieee754Bits(value) = worker
                .eval_args(EvaluatedArgs::Ieee754Bits2 {
                    left: ReadyIeee754Arg::Value(left),
                    right: ReadyIeee754Arg::Value(right),
                })
                .unwrap()
            else {
                panic!("ATAN2 returned a non-IEEE754 value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(
                value.metadata(),
                ComputedIeee754BitsMetadata::OwnIeee754Bits
            );
            assert_eq!(value.into_option(), expected);
            assert_eq!(worker.kernel_invocations(), preceding + index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let ComputedValue::Ieee754Bits(value) = worker
            .eval_args(EvaluatedArgs::Ieee754Bits2 {
                left: ReadyIeee754Arg::Value(Some(nan)),
                right: ReadyIeee754Arg::Value(Some(one)),
            })
            .unwrap()
        else {
            panic!("NaN ATAN2 returned a non-IEEE754 value");
        };
        let actual = value.value();
        assert!(actual.is_some_and(|bits| f64::from_bits(bits).is_nan()));
        assert_eq!(
            value.metadata(),
            ComputedIeee754BitsMetadata::OwnIeee754Bits
        );
        assert_eq!(value.into_option(), actual);
        assert_eq!(worker.kernel_invocations(), preceding + 5);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_exp_log10_go_keep_nullable_and_nonfinite_bits() {
    let negative_zero = (-0.0_f64).to_bits();
    let infinity = f64::INFINITY.to_bits();
    let nan = 0x7ff8_0000_0000_0456_u64;
    for (operation, zero_result) in [
        (EvaluatedBytesOp::ExpGoNative, 1.0_f64.to_bits()),
        (EvaluatedBytesOp::Log10GoNative, f64::NEG_INFINITY.to_bits()),
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
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::Bytes(None)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        for (index, (input, expected)) in [
            (None, None),
            (Some(negative_zero), Some(zero_result)),
            (Some(infinity), Some(infinity)),
        ]
        .into_iter()
        .enumerate()
        {
            let ComputedValue::Ieee754Bits(value) =
                worker.eval_args(EvaluatedArgs::Ieee754Bits(input)).unwrap()
            else {
                panic!("Go EXP/LOG10 returned a non-IEEE754 value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(
                value.metadata(),
                ComputedIeee754BitsMetadata::OwnIeee754Bits
            );
            assert_eq!(value.into_option(), expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let ComputedValue::Ieee754Bits(value) = worker
            .eval_args(EvaluatedArgs::Ieee754Bits(Some(nan)))
            .unwrap()
        else {
            panic!("NaN Go EXP/LOG10 returned a non-IEEE754 value");
        };
        let actual = value.value();
        assert!(actual.is_some_and(|bits| f64::from_bits(bits).is_nan()));
        assert_eq!(
            value.metadata(),
            ComputedIeee754BitsMetadata::OwnIeee754Bits
        );
        assert_eq!(value.into_option(), actual);
        assert_eq!(worker.kernel_invocations(), 4);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_compression_preserves_owned_values_and_uncompress_outcomes() {
    let payload = b"owned\0payload\xff";
    let frame = {
        let mut worker = prepare_evaluated_bytes(
            EvaluatedBytesOp::CompressGoNative,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), EvaluatedBytesOp::CompressGoNative);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::Ieee754Bits(None)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let cases: [(Option<Vec<u8>>, Option<&[u8]>); 2] =
            [(None, None), (Some(Vec::new()), Some(b""))];
        for (index, (input, expected)) in cases.into_iter().enumerate() {
            let ComputedValue::Bytes(value) =
                worker.eval_args(EvaluatedArgs::Bytes(input)).unwrap()
            else {
                panic!("native COMPRESS returned a non-Bytes value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(value.into_option(), expected.map(|bytes| bytes.to_vec()));
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let ComputedValue::Bytes(value) = worker
            .eval_args(EvaluatedArgs::Bytes(Some(payload.to_vec())))
            .unwrap()
        else {
            panic!("nonempty COMPRESS returned a non-Bytes value");
        };
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        let observed = value.value().unwrap().to_vec();
        let frame = value.into_option().unwrap();
        assert_eq!(frame, observed);
        assert!(frame.len() > 4);
        assert_eq!(worker.kernel_invocations(), 3);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        frame
    };
    // Keep the complete encoded stream; only its declared output length changes.
    let mut limited_frame = frame.clone();
    limited_frame[..4].copy_from_slice(&0_u32.to_le_bytes());
    let retained = {
        let mut worker = prepare_evaluated_bytes(
            EvaluatedBytesOp::UncompressNative,
            LocalCompileContext::default(),
            ExecutionLimits {
                max_retained_bytes: 8 * 1024,
                ..ExecutionLimits::default()
            },
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), EvaluatedBytesOp::UncompressNative);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::Ieee754Bits(None)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let mut oversized = Vec::with_capacity(16 * 1024);
        oversized.push(b'x');
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::Bytes(Some(oversized))),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let cases = [
            (None, UncompressOutcome::Null),
            (Some(Vec::new()), UncompressOutcome::Value(Vec::new())),
            (Some(b"x".to_vec()), UncompressOutcome::Corrupt),
            (Some(limited_frame), UncompressOutcome::OutputLimit),
            (Some(frame), UncompressOutcome::Value(payload.to_vec())),
        ];
        let mut retained = UncompressOutcome::Null;
        for (index, (input, expected)) in cases.into_iter().enumerate() {
            let ComputedValue::Uncompress(value) =
                worker.eval_args(EvaluatedArgs::Bytes(input)).unwrap()
            else {
                panic!("UNCOMPRESS returned an unexpected output type");
            };
            assert_eq!(value.metadata(), ComputedUncompressMetadata::OwnUncompress);
            assert_eq!(value.outcome(), &expected);
            retained = value.into_outcome();
            assert_eq!(retained, expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        retained
    };
    let UncompressOutcome::Value(value) = retained else {
        panic!("owned UNCOMPRESS payload was lost after dropping the worker");
    };
    assert_eq!(value.as_slice(), payload);
}

#[test]
fn local_evaluated_args_json_introspection_keeps_carriers_and_transport_errors_distinct() {
    use tidb_query_common::error::{ErrorInner, EvaluateError};

    let valid_cases: [(EvaluatedBytesOp, Vec<(Option<&[u8]>, Option<i64>)>); 2] = [
        (
            EvaluatedBytesOp::JsonValidTextNative,
            vec![
                (None, None),
                (Some(b"\"\xff\""), Some(0)),
                (Some(b"{"), Some(0)),
                (Some(b"[]"), Some(1)),
            ],
        ),
        (
            EvaluatedBytesOp::JsonValidBinaryNative,
            vec![(None, None), (Some(b"\xff\0"), Some(1))],
        ),
    ];
    for (operation, cases) in valid_cases {
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
            worker.eval_args(EvaluatedArgs::Ieee754Bits(None)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        for (index, (input, expected)) in cases.into_iter().enumerate() {
            let ComputedValue::Int(value) = worker
                .eval_args(EvaluatedArgs::Bytes(input.map(|bytes| bytes.to_vec())))
                .unwrap()
            else {
                panic!("JSON_VALID returned a non-Int value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::JsonValidOtherNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::JsonValidOtherNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    assert!(matches!(
        worker.eval_args(EvaluatedArgs::Int(Some(0))),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    for expected_calls in 1..=2 {
        let ComputedValue::Int(value) = worker.eval_args(EvaluatedArgs::NoArgs).unwrap() else {
            panic!("JSON_VALID Others returned a non-Int value");
        };
        assert_eq!(value.value(), Some(0));
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), Some(0));
        assert_eq!(worker.kernel_invocations(), expected_calls);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let report_cases: [(EvaluatedBytesOp, Vec<(Option<&[u8]>, JsonReportOutcome)>); 3] = [
        (
            EvaluatedBytesOp::JsonTypeTextNative,
            vec![
                (None, JsonReportOutcome::Null),
                (Some(b""), JsonReportOutcome::EmptyText),
                (Some(b"{"), JsonReportOutcome::InvalidText),
                (Some(b"[]"), JsonReportOutcome::Bytes(b"ARRAY".to_vec())),
            ],
        ),
        (
            EvaluatedBytesOp::JsonTypeBinaryNative,
            vec![
                (None, JsonReportOutcome::Null),
                (Some(b"\xff\0"), JsonReportOutcome::InvalidText),
            ],
        ),
        (
            EvaluatedBytesOp::JsonDepthNative,
            vec![
                (None, JsonReportOutcome::Null),
                (Some(b"0"), JsonReportOutcome::Int(1)),
            ],
        ),
    ];
    for (operation, cases) in report_cases {
        let retained = {
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits {
                    max_retained_bytes: 8 * 1024,
                    ..ExecutionLimits::default()
                },
                usize::MAX,
            )
            .unwrap();
            assert_eq!(worker.operation(), operation);
            assert_eq!(worker.kernel_invocations(), 0);
            let storage = worker.retained_storage().unwrap();
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::Ieee754Bits(None)),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            if operation == EvaluatedBytesOp::JsonTypeTextNative {
                let mut oversized = Vec::with_capacity(16 * 1024);
                oversized.push(b'{');
                assert!(matches!(
                    worker.eval_args(EvaluatedArgs::Bytes(Some(oversized))),
                    Err(LocalError::ResourceLimit(_))
                ));
                assert_eq!(worker.kernel_invocations(), 0);
                assert!(worker.is_healthy());
                assert_eq!(worker.retained_storage().unwrap(), storage);
            }
            let preceding = if operation == EvaluatedBytesOp::JsonTypeBinaryNative {
                let error = worker
                    .eval_args(EvaluatedArgs::Bytes(Some(Vec::new())))
                    .unwrap_err();
                assert!(matches!(
                    error,
                    LocalError::Evaluation(error)
                        if matches!(error.0.as_ref(), ErrorInner::Evaluate(EvaluateError::Other(_)))
                ));
                1_u64
            } else {
                0
            };
            assert_eq!(worker.kernel_invocations(), preceding);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            let mut retained = JsonReportOutcome::Null;
            for (index, (input, expected)) in cases.into_iter().enumerate() {
                let ComputedValue::JsonReport(value) = worker
                    .eval_args(EvaluatedArgs::Bytes(input.map(|bytes| bytes.to_vec())))
                    .unwrap()
                else {
                    panic!("JSON introspection returned an unexpected output type");
                };
                assert_eq!(value.metadata(), ComputedJsonReportMetadata::OwnJsonReport);
                assert_eq!(value.outcome(), &expected);
                retained = value.into_outcome();
                assert_eq!(retained, expected);
                assert_eq!(worker.kernel_invocations(), preceding + index as u64 + 1);
                assert!(worker.is_healthy());
                assert_eq!(worker.retained_storage().unwrap(), storage);
            }
            retained
        };
        if operation == EvaluatedBytesOp::JsonTypeTextNative {
            let JsonReportOutcome::Bytes(value) = retained else {
                panic!("JSON type name was lost after dropping the worker");
            };
            assert_eq!(value.as_slice(), b"ARRAY");
        }
    }
}

#[test]
fn local_evaluated_args_json_storage_quote_preserves_computed_carriers() {
    for (operation, storage_value) in [
        (EvaluatedBytesOp::JsonStorageFreeNative, 0_i64),
        (EvaluatedBytesOp::JsonStorageSizeNative, 9_i64),
    ] {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits {
                max_retained_bytes: 8 * 1024,
                ..ExecutionLimits::default()
            },
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::Int(Some(0))),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        if operation == EvaluatedBytesOp::JsonStorageSizeNative {
            let mut oversized = Vec::with_capacity(16 * 1024);
            oversized.push(b'{');
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::Bytes(Some(oversized))),
                Err(LocalError::ResourceLimit(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let cases: [(Option<&[u8]>, JsonReportOutcome); 4] = [
            (None, JsonReportOutcome::Null),
            (Some(b""), JsonReportOutcome::EmptyText),
            (Some(b"{"), JsonReportOutcome::InvalidText),
            (Some(b"0"), JsonReportOutcome::Int(storage_value)),
        ];
        for (index, (input, expected)) in cases.into_iter().enumerate() {
            let ComputedValue::JsonReport(value) = worker
                .eval_args(EvaluatedArgs::Bytes(input.map(|bytes| bytes.to_vec())))
                .unwrap()
            else {
                panic!("JSON storage returned an unexpected output type");
            };
            assert_eq!(value.metadata(), ComputedJsonReportMetadata::OwnJsonReport);
            assert_eq!(value.outcome(), &expected);
            assert_eq!(value.into_outcome(), expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
    let expected = b"\"a\\nb\"";
    let quoted = {
        let mut worker = prepare_evaluated_bytes(
            EvaluatedBytesOp::JsonQuoteNative,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), EvaluatedBytesOp::JsonQuoteNative);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::Ieee754Bits(None)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let ComputedValue::Bytes(value) = worker.eval_args(EvaluatedArgs::Bytes(None)).unwrap()
        else {
            panic!("nullable JSON_QUOTE returned a non-Bytes value");
        };
        assert_eq!(value.value(), None);
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.into_option(), None);
        assert_eq!(worker.kernel_invocations(), 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let ComputedValue::Bytes(value) = worker
            .eval_args(EvaluatedArgs::Bytes(Some(b"a\nb".to_vec())))
            .unwrap()
        else {
            panic!("JSON_QUOTE returned a report instead of Bytes");
        };
        assert_eq!(value.value(), Some(expected.as_slice()));
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        let quoted = value.into_option().unwrap();
        assert_eq!(quoted.as_slice(), expected);
        assert_eq!(worker.kernel_invocations(), 2);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        quoted
    };
    assert_eq!(quoted.as_slice(), expected);
}

#[test]
fn local_evaluated_args_calendar_core_fields_keep_raw_role() {
    for (operation, maximum) in [
        (EvaluatedBytesOp::YearCoreNative, 16383_i64),
        (EvaluatedBytesOp::MonthCoreNative, 15_i64),
        (EvaluatedBytesOp::DayOfMonthCoreNative, 31_i64),
        (EvaluatedBytesOp::QuarterCoreNative, 5_i64),
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
        for invalid in [
            EvaluatedArgs::Bytes(Some(0_u64.to_le_bytes().to_vec())),
            EvaluatedArgs::Bytes(None),
            EvaluatedArgs::Ieee754Bits(None),
            EvaluatedArgs::Ieee754Bits(Some(0)),
            EvaluatedArgs::Int(Some(0)),
        ] {
            assert!(matches!(
                worker.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        for (index, (input, expected)) in [
            (None, None),
            (Some(0), Some(0)),
            (Some(u64::MAX), Some(maximum)),
        ]
        .into_iter()
        .enumerate()
        {
            let ComputedValue::Int(value) = worker
                .eval_args(EvaluatedArgs::TimeCoreBits(input))
                .unwrap()
            else {
                panic!("calendar core field returned a non-Int value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
    // Execution charges the materialized LE8 input, independently of the
    // constructor's worker-retained reservation. No caller Vec is supplied.
    let mut bounded = prepare_evaluated_bytes(
        EvaluatedBytesOp::YearCoreNative,
        LocalCompileContext::default(),
        ExecutionLimits {
            max_retained_bytes: 7,
            ..ExecutionLimits::default()
        },
        usize::MAX,
    )
    .unwrap();
    assert_eq!(bounded.operation(), EvaluatedBytesOp::YearCoreNative);
    assert_eq!(bounded.kernel_invocations(), 0);
    let storage = bounded.retained_storage().unwrap();
    assert!(matches!(
        bounded.eval_args(EvaluatedArgs::TimeCoreBits(Some(0))),
        Err(LocalError::ResourceLimit(_))
    ));
    assert_eq!(bounded.kernel_invocations(), 0);
    assert!(bounded.is_healthy());
    assert_eq!(bounded.retained_storage().unwrap(), storage);
    let mut ieee = prepare_evaluated_bytes(
        EvaluatedBytesOp::ExpGoNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(ieee.operation(), EvaluatedBytesOp::ExpGoNative);
    assert_eq!(ieee.kernel_invocations(), 0);
    let storage = ieee.retained_storage().unwrap();
    assert!(matches!(
        ieee.eval_args(EvaluatedArgs::TimeCoreBits(Some(0))),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(ieee.kernel_invocations(), 0);
    assert!(ieee.is_healthy());
    assert_eq!(ieee.retained_storage().unwrap(), storage);
    let ComputedValue::Ieee754Bits(value) = ieee
        .eval_args(EvaluatedArgs::Ieee754Bits(Some(0.0_f64.to_bits())))
        .unwrap()
    else {
        panic!("reused IEEE worker returned a non-IEEE754 value");
    };
    assert_eq!(value.value(), Some(1.0_f64.to_bits()));
    assert_eq!(
        value.metadata(),
        ComputedIeee754BitsMetadata::OwnIeee754Bits
    );
    assert_eq!(value.into_option(), Some(1.0_f64.to_bits()));
    assert_eq!(ieee.kernel_invocations(), 1);
    assert!(ieee.is_healthy());
    assert_eq!(ieee.retained_storage().unwrap(), storage);
}

#[test]
fn local_evaluated_args_time_text_and_nanos_keep_value_roles() {
    for (operation, text, component) in [
        (EvaluatedBytesOp::HourTextNative, true, 838_i64),
        (EvaluatedBytesOp::MinuteTextNative, true, 59_i64),
        (EvaluatedBytesOp::SecondTextNative, true, 59_i64),
        (EvaluatedBytesOp::HourNanosNative, false, 900_i64),
        (EvaluatedBytesOp::MinuteNanosNative, false, 30_i64),
        (EvaluatedBytesOp::SecondNanosNative, false, 15_i64),
    ] {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits {
                max_retained_bytes: 8 * 1024,
                ..ExecutionLimits::default()
            },
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        let wrong = if text {
            EvaluatedArgs::Int(Some(0))
        } else {
            EvaluatedArgs::Bytes(Some(b"00:00:00".to_vec()))
        };
        for invalid in [wrong, EvaluatedArgs::TimeCoreBits(Some(0))] {
            assert!(matches!(
                worker.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        if operation == EvaluatedBytesOp::HourTextNative {
            let mut oversized = Vec::with_capacity(16 * 1024);
            oversized.extend_from_slice(b"bad");
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::Bytes(Some(oversized))),
                Err(LocalError::ResourceLimit(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let cases = if text {
            [
                (EvaluatedArgs::Bytes(None), None),
                (EvaluatedArgs::Bytes(Some(b"00:00:00".to_vec())), Some(0)),
                (
                    EvaluatedArgs::Bytes(Some(b"900:30:15".to_vec())),
                    Some(component),
                ),
            ]
        } else {
            [
                (EvaluatedArgs::Int(None), None),
                (EvaluatedArgs::Int(Some(0)), Some(0)),
                (
                    EvaluatedArgs::Int(Some(3_241_815_000_000_000)),
                    Some(component),
                ),
            ]
        };
        for (index, (args, expected)) in cases.into_iter().enumerate() {
            let ComputedValue::Int(value) = worker.eval_args(args).unwrap() else {
                panic!("time component returned a non-Int value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let extra = match operation {
            EvaluatedBytesOp::HourTextNative => {
                Some((EvaluatedArgs::Bytes(Some(b"bad".to_vec())), None))
            }
            EvaluatedBytesOp::HourNanosNative => {
                Some((EvaluatedArgs::Int(Some(i64::MIN)), Some(2_562_047)))
            }
            EvaluatedBytesOp::SecondNanosNative => {
                Some((EvaluatedArgs::Int(Some(-61_000_000_000)), Some(1)))
            }
            _ => None,
        };
        if let Some((args, expected)) = extra {
            let ComputedValue::Int(value) = worker.eval_args(args).unwrap() else {
                panic!("time component edge returned a non-Int value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), expected);
            assert_eq!(worker.kernel_invocations(), 4);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[test]
fn local_evaluated_args_month_name_and_time_to_sec_keep_nullable_text() {
    let retained = {
        let mut worker = prepare_evaluated_bytes(
            EvaluatedBytesOp::MonthNameTextNative,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), EvaluatedBytesOp::MonthNameTextNative);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::Int(Some(0))),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let cases: [(Option<&[u8]>, Option<&[u8]>); 3] = [
            (None, None),
            (Some(b"2023-02-29"), None),
            (Some(b"2024-02-29"), Some(b"February")),
        ];
        let mut retained = None;
        for (index, (input, expected)) in cases.into_iter().enumerate() {
            let ComputedValue::Bytes(value) = worker
                .eval_args(EvaluatedArgs::Bytes(input.map(|bytes| bytes.to_vec())))
                .unwrap()
            else {
                panic!("MONTHNAME returned a non-Bytes value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            retained = value.into_option();
            assert_eq!(retained.as_deref(), expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        retained
    };
    assert_eq!(retained.as_deref(), Some(b"February".as_slice()));
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::TimeToSecTextNative,
        LocalCompileContext::default(),
        ExecutionLimits {
            max_retained_bytes: 8 * 1024,
            ..ExecutionLimits::default()
        },
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::TimeToSecTextNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    assert!(matches!(
        worker.eval_args(EvaluatedArgs::Int(Some(0))),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let mut oversized = Vec::with_capacity(16 * 1024);
    oversized.extend_from_slice(b"junk");
    assert!(matches!(
        worker.eval_args(EvaluatedArgs::Bytes(Some(oversized))),
        Err(LocalError::ResourceLimit(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let cases: [(Option<&[u8]>, Option<i64>); 4] = [
        (None, None),
        (Some(b"-12:34:56.9999999"), Some(-45_296)),
        (Some(b"900:00:00"), None),
        (Some(b"junk"), Some(0)),
    ];
    for (index, (input, expected)) in cases.into_iter().enumerate() {
        let ComputedValue::Int(value) = worker
            .eval_args(EvaluatedArgs::Bytes(input.map(|bytes| bytes.to_vec())))
            .unwrap()
        else {
            panic!("TIME_TO_SEC returned a non-Int value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_period_receipts_and_get_format_keep_nullable_boundaries() {
    use tidb_query_common::error::{ErrorInner, EvaluateError};

    for (operation, kind, left, right, expected, display) in [
        (
            EvaluatedBytesOp::PeriodAddNative,
            EvaluatedSqlFailureKind::PeriodAddIncorrectArguments,
            202312,
            1,
            202401,
            "Incorrect arguments to period_add",
        ),
        (
            EvaluatedBytesOp::PeriodDiffNative,
            EvaluatedSqlFailureKind::PeriodDiffIncorrectArguments,
            202402,
            202401,
            1,
            "Incorrect arguments to period_diff",
        ),
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
        let failure = worker
            .eval_args_reported(EvaluatedArgs::Int(Some(0)))
            .unwrap_err();
        assert!(matches!(failure.error(), LocalError::InvalidBatch(_)));
        assert_eq!(failure.operation(), None);
        assert_eq!(failure.sql_failure(), None);
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let ComputedValue::Int(value) = worker
            .eval_args_reported(EvaluatedArgs::Int2(Some(0), None))
            .unwrap()
        else {
            panic!("nullable period operation returned a non-Int value");
        };
        assert_eq!(value.value(), None);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), None);
        assert_eq!(worker.kernel_invocations(), 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        for (index, reported) in [true, false].into_iter().enumerate() {
            let error = if reported {
                let failure = worker
                    .eval_args_reported(EvaluatedArgs::Int2(Some(0), Some(0)))
                    .unwrap_err();
                assert_eq!(failure.operation(), Some(operation));
                assert_eq!(failure.sql_failure(), Some(kind));
                let primary = failure.error().to_string();
                let error = failure.into_error();
                assert_eq!(error.to_string(), primary);
                error
            } else {
                worker
                    .eval_args(EvaluatedArgs::Int2(Some(0), Some(0)))
                    .unwrap_err()
            };
            let LocalError::Evaluation(error) = error else {
                panic!("invalid period did not preserve its evaluation error");
            };
            let ErrorInner::Evaluate(cause) = error.0.as_ref() else {
                panic!("invalid period did not preserve its typed cause");
            };
            assert!(matches!(
                (operation, cause),
                (
                    EvaluatedBytesOp::PeriodAddNative,
                    EvaluateError::PeriodAddIncorrectArguments
                ) | (
                    EvaluatedBytesOp::PeriodDiffNative,
                    EvaluateError::PeriodDiffIncorrectArguments
                )
            ));
            assert_eq!(cause.code(), 1210);
            assert_eq!(cause.to_string(), display);
            assert_eq!(worker.kernel_invocations(), index as u64 + 2);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let ComputedValue::Int(value) = worker
            .eval_args_reported(EvaluatedArgs::Int2(Some(left), Some(right)))
            .unwrap()
        else {
            panic!("reused period worker returned a non-Int value");
        };
        assert_eq!(value.value(), Some(expected));
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), Some(expected));
        assert_eq!(worker.kernel_invocations(), 4);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let mut bounded = prepare_evaluated_bytes(
        EvaluatedBytesOp::PeriodAddNative,
        LocalCompileContext::default(),
        ExecutionLimits {
            max_steps: 0,
            ..ExecutionLimits::default()
        },
        usize::MAX,
    )
    .unwrap();
    assert_eq!(bounded.kernel_invocations(), 0);
    let storage = bounded.retained_storage().unwrap();
    let failure = bounded
        .eval_args_reported(EvaluatedArgs::Int2(Some(0), Some(0)))
        .unwrap_err();
    assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
    assert_eq!(failure.operation(), None);
    assert_eq!(failure.sql_failure(), None);
    assert_eq!(bounded.kernel_invocations(), 0);
    assert!(bounded.is_healthy());
    assert_eq!(bounded.retained_storage().unwrap(), storage);
    let retained = {
        let mut worker = prepare_evaluated_bytes(
            EvaluatedBytesOp::GetFormatNative,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), EvaluatedBytesOp::GetFormatNative);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::Int(Some(0))),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let cases: [(Option<&[u8]>, Option<&[u8]>); 3] = [
            (None, None),
            (Some(b"\xff"), Some(b"")),
            (Some(b"usa"), Some(b"%m.%d.%Y")),
        ];
        let mut retained = None;
        for (index, (format, expected)) in cases.into_iter().enumerate() {
            let ComputedValue::Bytes(value) = worker
                .eval_args(EvaluatedArgs::Bytes2(
                    Some(b"DATE".to_vec()),
                    format.map(|bytes| bytes.to_vec()),
                ))
                .unwrap()
            else {
                panic!("GET_FORMAT returned a non-Bytes value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            retained = value.into_option();
            assert_eq!(retained.as_deref(), expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        retained
    };
    assert_eq!(retained.as_deref(), Some(b"%m.%d.%Y".as_slice()));
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::GetFormatNullNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::GetFormatNullNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    assert!(matches!(
        worker.eval_args(EvaluatedArgs::Int(Some(0))),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let ComputedValue::Bytes(value) = worker
        .eval_args_reported(EvaluatedArgs::Bytes(None))
        .unwrap()
    else {
        panic!("GET_FORMAT NULL path returned a non-Bytes value");
    };
    assert_eq!(value.value(), None);
    assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
    assert_eq!(value.into_option(), None);
    assert_eq!(worker.kernel_invocations(), 1);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let failure = worker
        .eval_args_reported(EvaluatedArgs::Bytes(Some(b"DATE".to_vec())))
        .unwrap_err();
    assert_eq!(failure.operation(), None);
    assert_eq!(failure.sql_failure(), None);
    assert!(matches!(failure.into_error(), LocalError::Evaluation(_)));
    assert_eq!(worker.kernel_invocations(), 2);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
}

#[test]
fn local_evaluated_args_day_text_fields_keep_nullable_and_year_zero_values() {
    for (operation, december, year_zero) in [
        (EvaluatedBytesOp::DayOfWeekTextNative, 6_i64, 7_i64),
        (EvaluatedBytesOp::WeekdayTextNative, 4_i64, 5_i64),
        (EvaluatedBytesOp::DayOfYearTextNative, 335_i64, 1_i64),
    ] {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits {
                max_retained_bytes: 8 * 1024,
                ..ExecutionLimits::default()
            },
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::Int(Some(0))),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let preceding = if operation == EvaluatedBytesOp::DayOfWeekTextNative {
            let mut oversized = Vec::with_capacity(16 * 1024);
            oversized.extend_from_slice(b"2017-02-30");
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::Bytes(Some(oversized))),
                Err(LocalError::ResourceLimit(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::Bytes(Some(vec![0xff]))),
                Err(LocalError::Evaluation(_))
            ));
            1_u64
        } else {
            0
        };
        assert_eq!(worker.kernel_invocations(), preceding);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let cases: [(Option<&[u8]>, Option<i64>); 4] = [
            (None, None),
            (Some(b"2017-02-30"), None),
            (Some(b"2017-12-01"), Some(december)),
            (Some(b"0000-01-01"), Some(year_zero)),
        ];
        for (index, (input, expected)) in cases.into_iter().enumerate() {
            let ComputedValue::Int(value) = worker
                .eval_args(EvaluatedArgs::Bytes(input.map(|bytes| bytes.to_vec())))
                .unwrap()
            else {
                panic!("day text field returned a non-Int value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), expected);
            assert_eq!(worker.kernel_invocations(), preceding + index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
    let retained = {
        let mut worker = prepare_evaluated_bytes(
            EvaluatedBytesOp::DayNameTextNative,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), EvaluatedBytesOp::DayNameTextNative);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::Int(Some(0))),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let cases: [(Option<&[u8]>, Option<&[u8]>); 4] = [
            (None, None),
            (Some(b"2017-02-30"), None),
            (Some(b"2017-12-01"), Some(b"Friday")),
            (Some(b"0000-01-01"), Some(b"Saturday")),
        ];
        let mut retained = None;
        for (index, (input, expected)) in cases.into_iter().enumerate() {
            let ComputedValue::Bytes(value) = worker
                .eval_args(EvaluatedArgs::Bytes(input.map(|bytes| bytes.to_vec())))
                .unwrap()
            else {
                panic!("DAYNAME returned a non-Bytes value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            retained = value.into_option();
            assert_eq!(retained.as_deref(), expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        retained
    };
    assert_eq!(retained.as_deref(), Some(b"Saturday".as_slice()));
}

#[test]
fn local_evaluated_args_date_diff_core_pair_keeps_exclusive_raw_role() {
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::DateDiffCoreNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::DateDiffCoreNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for invalid in [
        EvaluatedArgs::Bytes2(None, None),
        EvaluatedArgs::Int2(None, None),
        EvaluatedArgs::Ieee754Bits2 {
            left: ReadyIeee754Arg::Value(None),
            right: ReadyIeee754Arg::Value(None),
        },
        EvaluatedArgs::TimeCoreBits(None),
    ] {
        assert!(matches!(
            worker.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let ComputedValue::Int(value) = worker
        .eval_args(EvaluatedArgs::TimeCoreBits2(None, None))
        .unwrap()
    else {
        panic!("nullable DATE_DIFF core returned a non-Int value");
    };
    assert_eq!(value.value(), None);
    assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
    assert_eq!(value.into_option(), None);
    assert_eq!(worker.kernel_invocations(), 1);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    // These year-zero core fixtures intentionally differ from the text policy.
    let mut retained = None;
    for (index, (left, right)) in [
        (0x0000_c200_0000_0000, 0x0000_ba00_0000_0000),
        (0x0000_c3ff_ffff_ffff, 0x0000_ba00_0000_0000),
    ]
    .into_iter()
    .enumerate()
    {
        let ComputedValue::Int(value) = worker
            .eval_args(EvaluatedArgs::TimeCoreBits2(Some(left), Some(right)))
            .unwrap()
        else {
            panic!("DATE_DIFF core returned a non-Int value");
        };
        assert_eq!(value.value(), Some(0));
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        retained = value.into_option();
        assert_eq!(retained, Some(0));
        assert_eq!(worker.kernel_invocations(), index as u64 + 2);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    drop(worker);
    assert_eq!(retained, Some(0));
    let mut bounded = prepare_evaluated_bytes(
        EvaluatedBytesOp::DateDiffCoreNative,
        LocalCompileContext::default(),
        ExecutionLimits {
            max_retained_bytes: 15,
            ..ExecutionLimits::default()
        },
        usize::MAX,
    )
    .unwrap();
    assert_eq!(bounded.kernel_invocations(), 0);
    let storage = bounded.retained_storage().unwrap();
    assert!(matches!(
        bounded.eval_args(EvaluatedArgs::TimeCoreBits2(Some(0), Some(0))),
        Err(LocalError::ResourceLimit(_))
    ));
    assert_eq!(bounded.kernel_invocations(), 0);
    assert!(bounded.is_healthy());
    assert_eq!(bounded.retained_storage().unwrap(), storage);
}

#[test]
fn local_evaluated_args_date_text_and_tso_preserve_sql_values_and_null_witness() {
    for (operation, nullable, normal, expected) in [
        (
            EvaluatedBytesOp::DateDiffTextNative,
            EvaluatedArgs::Bytes2(Some(b"0000-03-01".to_vec()), None),
            EvaluatedArgs::Bytes2(Some(b"0000-03-01".to_vec()), Some(b"0000-02-29".to_vec())),
            1_i64,
        ),
        (
            EvaluatedBytesOp::ToDaysTextNative,
            EvaluatedArgs::Bytes(None),
            EvaluatedArgs::Bytes(Some(b"0000-01-01".to_vec())),
            1_i64,
        ),
        (
            EvaluatedBytesOp::ToSecondsTextNative,
            EvaluatedArgs::Bytes(None),
            EvaluatedArgs::Bytes(Some(b"0000-01-01".to_vec())),
            86_400_i64,
        ),
        (
            EvaluatedBytesOp::TsoLogicalNative,
            EvaluatedArgs::Int(None),
            EvaluatedArgs::Int(Some(262_145)),
            1_i64,
        ),
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
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::TimeCoreBits2(None, None)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        for (index, (args, expected)) in [(nullable, None), (normal, Some(expected))]
            .into_iter()
            .enumerate()
        {
            let ComputedValue::Int(value) = worker.eval_args(args).unwrap() else {
                panic!("SQL date/TSO operation returned a non-Int value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        if operation == EvaluatedBytesOp::TsoLogicalNative {
            for (index, input) in [0, -1].into_iter().enumerate() {
                let ComputedValue::Int(value) =
                    worker.eval_args(EvaluatedArgs::Int(Some(input))).unwrap()
                else {
                    panic!("nonpositive TSO returned a non-Int value");
                };
                assert_eq!(value.value(), None);
                assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
                assert_eq!(value.into_option(), None);
                assert_eq!(worker.kernel_invocations(), index as u64 + 3);
                assert!(worker.is_healthy());
                assert_eq!(worker.retained_storage().unwrap(), storage);
            }
        }
    }
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::DateDiffNullNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::DateDiffNullNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for invalid in [
        EvaluatedArgs::NullWitness(Some(0)),
        EvaluatedArgs::Int(None),
    ] {
        assert!(matches!(
            worker.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let ComputedValue::Int(value) = worker.eval_args(EvaluatedArgs::NullWitness(None)).unwrap()
    else {
        panic!("DATE_DIFF NULL witness returned a non-Int value");
    };
    assert_eq!(value.value(), None);
    assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
    assert_eq!(value.into_option(), None);
    assert_eq!(worker.kernel_invocations(), 1);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
}

#[test]
fn local_evaluated_args_week_core_and_null_witness_keep_roles() {
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::WeekCoreNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::WeekCoreNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for invalid in [
        EvaluatedArgs::Bytes(None),
        EvaluatedArgs::Ieee754Bits(None),
        EvaluatedArgs::Int(Some(0)),
    ] {
        assert!(matches!(
            worker.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    for (index, (input, expected)) in [(None, None), (Some(0), Some(0))].into_iter().enumerate() {
        let ComputedValue::Int(value) = worker
            .eval_args(EvaluatedArgs::TimeCoreBits(input))
            .unwrap()
        else {
            panic!("WEEK core returned a non-Int value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::WeekNullNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::WeekNullNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for invalid in [
        EvaluatedArgs::NullWitness(Some(0)),
        EvaluatedArgs::Int(None),
    ] {
        assert!(matches!(
            worker.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let ComputedValue::Int(value) = worker.eval_args(EvaluatedArgs::NullWitness(None)).unwrap()
    else {
        panic!("WEEK NULL witness returned a non-Int value");
    };
    assert_eq!(value.value(), None);
    assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
    assert_eq!(value.into_option(), None);
    assert_eq!(worker.kernel_invocations(), 1);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
}

#[test]
fn local_evaluated_args_week_probe_owns_text_before_nullable_mode_calls() {
    for (operation, input, expected) in [
        (
            EvaluatedBytesOp::WeekTextNative,
            b"2008-02-20".as_slice(),
            7_i64,
        ),
        (
            EvaluatedBytesOp::YearWeekTextNative,
            b"2000-01-01".as_slice(),
            199_952_i64,
        ),
    ] {
        let text = {
            let mut probe = prepare_evaluated_bytes(
                EvaluatedBytesOp::WeekDateTextNative,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            assert_eq!(probe.operation(), EvaluatedBytesOp::WeekDateTextNative);
            assert_eq!(probe.kernel_invocations(), 0);
            let storage = probe.retained_storage().unwrap();
            assert!(matches!(
                probe.eval_args(EvaluatedArgs::Int(Some(0))),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(probe.kernel_invocations(), 0);
            assert!(probe.is_healthy());
            assert_eq!(probe.retained_storage().unwrap(), storage);
            for (index, input) in [None, Some(b"0000-00-00".to_vec())].into_iter().enumerate() {
                let ComputedValue::Bytes(value) =
                    probe.eval_args(EvaluatedArgs::Bytes(input)).unwrap()
                else {
                    panic!("WEEK date probe returned a non-Bytes value");
                };
                assert_eq!(value.value(), None);
                assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
                assert_eq!(value.into_option(), None);
                assert_eq!(probe.kernel_invocations(), index as u64 + 1);
                assert!(probe.is_healthy());
                assert_eq!(probe.retained_storage().unwrap(), storage);
            }
            let ComputedValue::Bytes(value) = probe
                .eval_args(EvaluatedArgs::Bytes(Some(input.to_vec())))
                .unwrap()
            else {
                panic!("successful WEEK date probe returned a non-Bytes value");
            };
            assert_eq!(value.value(), Some(input));
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            let text = value.into_option().unwrap();
            assert_eq!(text.as_slice(), input);
            assert_eq!(probe.kernel_invocations(), 3);
            assert!(probe.is_healthy());
            assert_eq!(probe.retained_storage().unwrap(), storage);
            text
        };
        assert_eq!(text.as_slice(), input);
        // Only the owned original text crosses into the independent mode stage.
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
            worker.eval_args(EvaluatedArgs::Bytes(None)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let ComputedValue::Int(value) = worker
            .eval_args(EvaluatedArgs::BytesInt(None, None))
            .unwrap()
        else {
            panic!("nullable WEEK mode stage returned a non-Int value");
        };
        assert_eq!(value.value(), None);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), None);
        assert_eq!(worker.kernel_invocations(), 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        for (index, mode) in [None, Some(0)].into_iter().enumerate() {
            let ComputedValue::Int(value) = worker
                .eval_args(EvaluatedArgs::BytesInt(Some(text.clone()), mode))
                .unwrap()
            else {
                panic!("WEEK mode stage returned a non-Int value");
            };
            assert_eq!(value.value(), Some(expected));
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), Some(expected));
            assert_eq!(worker.kernel_invocations(), index as u64 + 2);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::WeekOfYearTextNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::WeekOfYearTextNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    assert!(matches!(
        worker.eval_args(EvaluatedArgs::BytesInt(None, None)),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let cases: [(Option<&[u8]>, Option<i64>); 2] = [(None, None), (Some(b"2024-03-15"), Some(11))];
    for (index, (input, expected)) in cases.into_iter().enumerate() {
        let ComputedValue::Int(value) = worker
            .eval_args(EvaluatedArgs::Bytes(input.map(|bytes| bytes.to_vec())))
            .unwrap()
        else {
            panic!("WEEKOFYEAR returned a non-Int value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
}

#[test]
fn local_evaluated_args_password_and_sm3_own_original_digest_bytes() {
    for (operation, digest) in [
        (
            EvaluatedBytesOp::PasswordNative,
            b"*0D3CED9BEC10A777AEC23CCC353A8C08A633045E".as_slice(),
        ),
        (
            EvaluatedBytesOp::Sm3Native,
            b"66c7f0f462eeedd9d1f2d46bdc10e4e24167c4875cf2f7a2297da02b8f4ba8e0".as_slice(),
        ),
    ] {
        let retained = {
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
                worker.eval_args(EvaluatedArgs::Int(Some(0))),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            let mut cases: Vec<(Option<&[u8]>, Option<&[u8]>)> = vec![(None, None)];
            if operation == EvaluatedBytesOp::PasswordNative {
                cases.push((Some(b""), Some(b"")));
            }
            cases.push((Some(b"abc"), Some(digest)));
            let mut retained = None;
            for (index, (input, expected)) in cases.into_iter().enumerate() {
                let ComputedValue::Bytes(value) = worker
                    .eval_args(EvaluatedArgs::Bytes(input.map(|bytes| bytes.to_vec())))
                    .unwrap()
                else {
                    panic!("native crypto returned a non-Bytes value");
                };
                assert_eq!(value.value(), expected);
                assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
                retained = value.into_option();
                assert_eq!(retained.as_deref(), expected);
                assert_eq!(worker.kernel_invocations(), index as u64 + 1);
                assert!(worker.is_healthy());
                assert_eq!(worker.retained_storage().unwrap(), storage);
            }
            retained
        };
        assert_eq!(retained.as_deref(), Some(digest));
    }
}

#[test]
fn local_evaluated_args_make_time_parts_own_seconds_before_fsp_stage() {
    let seconds = {
        let mut worker = prepare_evaluated_bytes(
            EvaluatedBytesOp::MakeTimePartsNative,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(worker.operation(), EvaluatedBytesOp::MakeTimePartsNative);
        assert_eq!(worker.kernel_invocations(), 0);
        let storage = worker.retained_storage().unwrap();
        for invalid in [
            EvaluatedArgs::Bytes3([None, None, None]),
            EvaluatedArgs::Ieee754BitsInt {
                value: None,
                scale: None,
            },
        ] {
            assert!(matches!(
                worker.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let mut seconds = None;
        for (index, (hour, minute, second, expected)) in [
            (None, Some(0), Some(0.0_f64.to_bits()), None),
            (Some((0, false)), None, Some(0.0_f64.to_bits()), None),
            (Some((0, false)), Some(0), None, None),
            (
                Some((-1, true)),
                Some(0),
                Some(0.0_f64.to_bits()),
                Some(3_020_399.0_f64.to_bits()),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let ComputedValue::Ieee754Bits(value) = worker
                .eval_args(EvaluatedArgs::MakeTimeParts {
                    hour,
                    minute,
                    second,
                })
                .unwrap()
            else {
                panic!("MAKETIME parts returned a non-IEEE754 value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(
                value.metadata(),
                ComputedIeee754BitsMetadata::OwnIeee754Bits
            );
            seconds = value.into_option();
            assert_eq!(seconds, expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        seconds.unwrap()
    };
    assert_eq!(seconds, 3_020_399.0_f64.to_bits());
    // FSP is supplied only after the first worker produced owned Some seconds.
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::SecToTimeNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::SecToTimeNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    let ComputedValue::Bytes(value) = worker
        .eval_args(EvaluatedArgs::Ieee754BitsInt {
            value: Some(seconds),
            scale: Some(0),
        })
        .unwrap()
    else {
        panic!("MAKETIME formatting stage returned a non-Bytes value");
    };
    assert_eq!(value.value(), Some(b"838:59:59".as_slice()));
    assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
    assert_eq!(value.into_option(), Some(b"838:59:59".to_vec()));
    assert_eq!(worker.kernel_invocations(), 1);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
}

#[test]
fn local_evaluated_args_sec_to_time_keeps_demanded_fsp_coupling_and_seven_digits() {
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::SecToTimeNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::SecToTimeNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for invalid in [
        EvaluatedArgs::BytesInt(None, None),
        EvaluatedArgs::Ieee754BitsInt {
            value: None,
            scale: Some(0),
        },
        EvaluatedArgs::Ieee754BitsInt {
            value: Some(0.0_f64.to_bits()),
            scale: None,
        },
        EvaluatedArgs::Ieee754BitsInt {
            value: Some(0.0_f64.to_bits()),
            scale: Some(-1),
        },
    ] {
        assert!(matches!(
            worker.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let cases: [(Option<u64>, Option<i64>, Option<&[u8]>); 3] = [
        // With no seconds value, FSP is undemanded rather than SQL NULL.
        (None, None, None),
        (Some(2_378.0_f64.to_bits()), Some(0), Some(b"00:39:38")),
        (Some(0.0_f64.to_bits()), Some(7), Some(b"00:00:00.0000000")),
    ];
    let mut retained = None;
    for (index, (value, scale, expected)) in cases.into_iter().enumerate() {
        let ComputedValue::Bytes(value) = worker
            .eval_args(EvaluatedArgs::Ieee754BitsInt { value, scale })
            .unwrap()
        else {
            panic!("SEC_TO_TIME returned a non-Bytes value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        retained = value.into_option();
        assert_eq!(retained.as_deref(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    drop(worker);
    assert_eq!(retained.as_deref(), Some(b"00:00:00.0000000".as_slice()));
}

#[test]
fn local_evaluated_args_date_constructors_own_normal_zero_and_null_results() {
    let cases: [(EvaluatedBytesOp, Vec<(EvaluatedArgs, Option<&[u8]>)>); 2] = [
        (
            EvaluatedBytesOp::MakeDateNative,
            vec![
                (EvaluatedArgs::Int2(None, Some(1)), None),
                (
                    EvaluatedArgs::Int2(Some(2012), Some(1)),
                    Some(b"2012-01-01"),
                ),
            ],
        ),
        (
            EvaluatedBytesOp::FromDaysNative,
            vec![
                (EvaluatedArgs::Int(None), None),
                (EvaluatedArgs::Int(Some(-140)), Some(b"0000-00-00")),
                (EvaluatedArgs::Int(Some(3_652_425)), None),
                (EvaluatedArgs::Int(Some(735_000)), Some(b"2012-05-12")),
            ],
        ),
    ];
    for (operation, cases) in cases {
        let final_expected = cases.last().unwrap().1;
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
            worker.eval_args(EvaluatedArgs::Bytes(None)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let mut retained = None;
        for (index, (args, expected)) in cases.into_iter().enumerate() {
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!("date constructor returned a non-Bytes value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            retained = value.into_option();
            assert_eq!(retained.as_deref(), expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        drop(worker);
        assert_eq!(retained.as_deref(), final_expected);
    }
}

#[test]
fn local_evaluated_args_duration_probe_owns_text_before_format_stage() {
    let text = {
        let mut probe = prepare_evaluated_bytes(
            EvaluatedBytesOp::DurationTextProbeNative,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(probe.operation(), EvaluatedBytesOp::DurationTextProbeNative);
        assert_eq!(probe.kernel_invocations(), 0);
        let storage = probe.retained_storage().unwrap();
        assert!(matches!(
            probe.eval_args(EvaluatedArgs::Int(None)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(probe.kernel_invocations(), 0);
        assert!(probe.is_healthy());
        assert_eq!(probe.retained_storage().unwrap(), storage);
        let ComputedValue::Bytes(value) = probe.eval_args(EvaluatedArgs::Bytes(None)).unwrap()
        else {
            panic!("nullable duration probe returned a non-Bytes value");
        };
        assert_eq!(value.value(), None);
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.into_option(), None);
        assert_eq!(probe.kernel_invocations(), 1);
        assert!(probe.is_healthy());
        assert_eq!(probe.retained_storage().unwrap(), storage);
        let ComputedValue::Bytes(value) = probe
            .eval_args(EvaluatedArgs::Bytes(Some(b"1990-05-07 19:30:10".to_vec())))
            .unwrap()
        else {
            panic!("duration probe returned a non-Bytes value");
        };
        assert_eq!(value.value(), Some(b"1990-05-07 19:30:10".as_slice()));
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        let text = value.into_option().unwrap();
        assert_eq!(text.as_slice(), b"1990-05-07 19:30:10");
        assert_eq!(probe.kernel_invocations(), 2);
        assert!(probe.is_healthy());
        assert_eq!(probe.retained_storage().unwrap(), storage);
        text
    };
    assert_eq!(text.as_slice(), b"1990-05-07 19:30:10");
    // Only after the probe exits do we prepare the caller's format argument.
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::TimeFormatTextNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::TimeFormatTextNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    assert!(matches!(
        worker.eval_args(EvaluatedArgs::Bytes(None)),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let cases: [(EvaluatedArgs, Option<&[u8]>); 2] = [
        (EvaluatedArgs::Bytes2(Some(text.clone()), None), None),
        (
            EvaluatedArgs::Bytes2(Some(text), Some(b"%H %i %s".to_vec())),
            Some(b"19 30 10"),
        ),
    ];
    let mut retained = None;
    for (index, (args, expected)) in cases.into_iter().enumerate() {
        let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
            panic!("TIME_FORMAT returned a non-Bytes value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        retained = value.into_option();
        assert_eq!(retained.as_deref(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    drop(worker);
    assert_eq!(retained.as_deref(), Some(b"19 30 10".as_slice()));
}

#[test]
fn local_evaluated_args_date_format_core_keeps_roles_and_missing_distinct() {
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::DateFormatCoreNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::DateFormatCoreNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for invalid in [
        EvaluatedArgs::Bytes2(None, None),
        EvaluatedArgs::TimeCoreBits2(None, None),
        EvaluatedArgs::Ieee754Bits2 {
            left: ReadyIeee754Arg::Value(None),
            right: ReadyIeee754Arg::Value(None),
        },
    ] {
        assert!(matches!(
            worker.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    // Raw zero is not globally rejected: only the demanded month is invalid.
    let cases: [(Option<&[u8]>, Option<&[u8]>); 3] = [
        (None, None),
        (Some(b"%M"), None),
        (Some(b"literal%"), Some(b"literal")),
    ];
    let mut retained = None;
    for (index, (layout, expected)) in cases.into_iter().enumerate() {
        let ComputedValue::Bytes(value) = worker
            .eval_args(EvaluatedArgs::TimeCoreBitsBytes {
                core: 0,
                bytes: layout.map(|bytes| bytes.to_vec()),
            })
            .unwrap()
        else {
            panic!("DATE_FORMAT core returned a non-Bytes value");
        };
        assert_eq!(value.value(), expected);
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        retained = value.into_option();
        assert_eq!(retained.as_deref(), expected);
        assert_eq!(worker.kernel_invocations(), index as u64 + 1);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    // Defensive transport error, not a new SQL input fixture or InvalidDate.
    assert!(matches!(
        worker.eval_args(EvaluatedArgs::TimeCoreBitsBytes {
            core: 0,
            bytes: Some(vec![0xff])
        }),
        Err(LocalError::Evaluation(_))
    ));
    assert_eq!(worker.kernel_invocations(), 4);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    drop(worker);
    assert_eq!(retained.as_deref(), Some(b"literal".as_slice()));
    let mut bounded = prepare_evaluated_bytes(
        EvaluatedBytesOp::DateFormatCoreNative,
        LocalCompileContext::default(),
        ExecutionLimits {
            max_retained_bytes: 0,
            ..ExecutionLimits::default()
        },
        usize::MAX,
    )
    .unwrap();
    assert_eq!(bounded.kernel_invocations(), 0);
    let storage = bounded.retained_storage().unwrap();
    assert!(matches!(
        bounded.eval_args(EvaluatedArgs::TimeCoreBitsBytes {
            core: 0,
            bytes: None
        }),
        Err(LocalError::ResourceLimit(_))
    ));
    assert_eq!(bounded.kernel_invocations(), 0);
    assert!(bounded.is_healthy());
    assert_eq!(bounded.retained_storage().unwrap(), storage);
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::DateFormatNullNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(worker.operation(), EvaluatedBytesOp::DateFormatNullNative);
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    for invalid in [
        EvaluatedArgs::NullWitness(Some(0)),
        EvaluatedArgs::Int(None),
    ] {
        assert!(matches!(
            worker.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }
    let ComputedValue::Bytes(value) = worker.eval_args(EvaluatedArgs::NullWitness(None)).unwrap()
    else {
        panic!("DATE_FORMAT NULL witness returned a non-Bytes value");
    };
    assert_eq!(value.value(), None);
    assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
    assert_eq!(value.into_option(), None);
    assert_eq!(worker.kernel_invocations(), 1);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::DateFormatMissingNative,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        usize::MAX,
    )
    .unwrap();
    assert_eq!(
        worker.operation(),
        EvaluatedBytesOp::DateFormatMissingNative
    );
    assert_eq!(worker.kernel_invocations(), 0);
    let storage = worker.retained_storage().unwrap();
    assert!(matches!(
        worker.eval_args(EvaluatedArgs::NullWitness(None)),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(worker.kernel_invocations(), 0);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
    let ComputedValue::Int(value) = worker.eval_args(EvaluatedArgs::NoArgs).unwrap() else {
        panic!("DATE_FORMAT missing arguments returned a non-Int value");
    };
    assert_eq!(value.value(), Some(0));
    assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
    assert_eq!(value.into_option(), Some(0));
    assert_eq!(worker.kernel_invocations(), 1);
    assert!(worker.is_healthy());
    assert_eq!(worker.retained_storage().unwrap(), storage);
}

#[test]
fn uuid_translate_dispatch_owned_values_and_exact_admission() {
    let prepare = |operation| {
        prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap()
    };
    // Fixed UUID rows from native builtin_ext/misc.rs, not a new UUID oracle.
    let canonical = b"6ccd780c-baba-1026-9564-5b8c656024db";
    let normal = vec![
        0x6c, 0xcd, 0x78, 0x0c, 0xba, 0xba, 0x10, 0x26, 0x95, 0x64, 0x5b, 0x8c, 0x65, 0x60, 0x24,
        0xdb,
    ];
    let swapped = vec![
        0x10, 0x26, 0xba, 0xba, 0x6c, 0xcd, 0x78, 0x0c, 0x95, 0x64, 0x5b, 0x8c, 0x65, 0x60, 0x24,
        0xdb,
    ];
    for operation in [
        EvaluatedBytesOp::IsUuidNative,
        EvaluatedBytesOp::UuidVersionNative,
    ] {
        let mut worker = prepare(operation);
        let storage = worker.retained_storage().unwrap();
        assert_eq!(worker.operation(), operation);
        assert_eq!(worker.kernel_invocations(), 0);
        for (index, (input, expected)) in [(None, None), (Some(canonical.as_slice()), Some(1))]
            .into_iter()
            .enumerate()
        {
            let ComputedValue::Int(value) = worker
                .eval_args(EvaluatedArgs::Bytes(input.map(<[u8]>::to_vec)))
                .unwrap()
            else {
                panic!("UUID predicate/version must own its integer");
            };
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.value(), expected);
            assert_eq!(value.into_option(), expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
    let mut timestamp = prepare(EvaluatedBytesOp::UuidTimestampNative);
    let storage = timestamp.retained_storage().unwrap();
    assert!(matches!(
        timestamp.eval_args(EvaluatedArgs::Decimal(None)),
        Err(LocalError::InvalidBatch(_))
    ));
    assert_eq!(timestamp.kernel_invocations(), 0);
    for (index, input) in [
        None,
        Some(b"a3e3b4a1-ea6d-471e-9860-8303a8b261f6".as_slice()),
    ]
    .into_iter()
    .enumerate()
    {
        let ComputedValue::Decimal(value) = timestamp
            .eval_args(EvaluatedArgs::Bytes(input.map(<[u8]>::to_vec)))
            .unwrap()
        else {
            panic!("UUID_TIMESTAMP NULL must retain the Decimal result domain");
        };
        assert_eq!(value.metadata(), ComputedDecimalMetadata::OwnDecimal);
        assert_eq!(value.value(), None);
        assert_eq!(value.checked_i64_view(), None);
        assert_eq!(value.into_option(), None);
        assert_eq!(timestamp.kernel_invocations(), index as u64 + 1);
    }
    let ComputedValue::Decimal(value) = timestamp
        .eval_args(EvaluatedArgs::Bytes(Some(
            b"019b1440-87b7-7380-ab00-ce413e795004".to_vec(),
        )))
        .unwrap()
    else {
        panic!("UUID_TIMESTAMP must own its exact Decimal");
    };
    assert_eq!(value.metadata(), ComputedDecimalMetadata::OwnDecimal);
    assert_eq!(value.checked_i64_view(), None);
    let owned = value.into_option().unwrap();
    assert_eq!(owned.result_scale(), 6);
    assert_eq!(owned.to_string(), "1765571332.023000");
    assert_eq!(timestamp.kernel_invocations(), 3);
    assert!(timestamp.is_healthy());
    assert_eq!(timestamp.retained_storage().unwrap(), storage);
    drop(timestamp);
    assert_eq!(owned.to_string(), "1765571332.023000");

    let mut parser = prepare(EvaluatedBytesOp::UuidToBinParseNative);
    let ComputedValue::Bytes(value) = parser.eval_args(EvaluatedArgs::Bytes(None)).unwrap() else {
        panic!("UUID parse NULL must own Bytes");
    };
    assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
    assert_eq!(value.into_option(), None);
    let ComputedValue::Bytes(value) = parser
        .eval_args(EvaluatedArgs::Bytes(Some(canonical.to_vec())))
        .unwrap()
    else {
        panic!("UUID parse must own Bytes");
    };
    assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
    let parsed = value.into_option().unwrap();
    assert_eq!(parsed, normal);
    assert_eq!(parser.kernel_invocations(), 2);
    assert!(parser.is_healthy());
    drop(parser);
    assert_eq!(parsed, normal);

    let mut swap = prepare(EvaluatedBytesOp::UuidToBinSwapNative);
    let storage = swap.retained_storage().unwrap();
    for invalid in [
        EvaluatedArgs::BytesInt(None, Some(0)),
        EvaluatedArgs::BytesInt(Some(vec![0; 15]), Some(0)),
        EvaluatedArgs::BytesInt(Some(vec![0; 17]), Some(1)),
        EvaluatedArgs::BytesInt(Some(parsed.clone()), None),
    ] {
        assert!(matches!(
            swap.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(swap.kernel_invocations(), 0);
        assert!(swap.is_healthy());
        assert_eq!(swap.retained_storage().unwrap(), storage);
    }
    for (index, (flag, expected)) in [(0, &normal), (1, &swapped)].into_iter().enumerate() {
        let ComputedValue::Bytes(value) = swap
            .eval_args(EvaluatedArgs::BytesInt(Some(parsed.clone()), Some(flag)))
            .unwrap()
        else {
            panic!("UUID swap must own Bytes");
        };
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.value(), Some(expected.as_slice()));
        assert_eq!(value.into_option().as_ref(), Some(expected));
        assert_eq!(swap.kernel_invocations(), index as u64 + 1);
        assert!(swap.is_healthy());
        assert_eq!(swap.retained_storage().unwrap(), storage);
    }
    let mut binary = prepare(EvaluatedBytesOp::BinToUuidNative);
    for (index, (input, flag, expected)) in [
        (None, 0, None),
        (
            Some(normal),
            1,
            Some(b"baba1026-780c-6ccd-9564-5b8c656024db".as_slice()),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let ComputedValue::Bytes(value) = binary
            .eval_args(EvaluatedArgs::BytesInt(input, Some(flag)))
            .unwrap()
        else {
            panic!("BIN_TO_UUID must own Bytes");
        };
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.value(), expected);
        let owned = value.into_option();
        assert_eq!(owned.as_deref(), expected);
        assert_eq!(binary.kernel_invocations(), index as u64 + 1);
        assert!(binary.is_healthy());
    }

    for (operation, src, from, to, expected) in [
        // Original string2.rs rune fixture.
        (
            EvaluatedBytesOp::TranslateUtf8Native,
            "中文测试".as_bytes(),
            "中试".as_bytes(),
            b"XY".as_slice(),
            "X文测Y".as_bytes(),
        ),
        // New hand-derived byte-policy literal: ff is replaced by 00, not decoded.
        (
            EvaluatedBytesOp::TranslateBinaryNative,
            [0xff, b'a', 0xff].as_slice(),
            [0xff].as_slice(),
            [0].as_slice(),
            [0, b'a', 0].as_slice(),
        ),
    ] {
        let mut worker = prepare(operation);
        let storage = worker.retained_storage().unwrap();
        let ComputedValue::Bytes(value) = worker
            .eval_args(EvaluatedArgs::Bytes3([
                Some(src.to_vec()),
                None,
                Some(to.to_vec()),
            ]))
            .unwrap()
        else {
            panic!("nullable TRANSLATE must own Bytes");
        };
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(value.into_option(), None);
        let ComputedValue::Bytes(value) = worker
            .eval_args(EvaluatedArgs::Bytes3([
                Some(src.to_vec()),
                Some(from.to_vec()),
                Some(to.to_vec()),
            ]))
            .unwrap()
        else {
            panic!("TRANSLATE must own Bytes");
        };
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        let owned = value.into_option().unwrap();
        assert_eq!(owned.as_slice(), expected);
        assert_eq!(worker.kernel_invocations(), 2);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        drop(worker);
        assert_eq!(owned.as_slice(), expected);
    }
    let mut null = prepare(EvaluatedBytesOp::TranslateNullNative);
    for invalid in [
        EvaluatedArgs::NullWitness(Some(0)),
        EvaluatedArgs::Int(None),
        EvaluatedArgs::Bytes3([None, None, None]),
    ] {
        assert!(matches!(
            null.eval_args(invalid),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(null.kernel_invocations(), 0);
        assert!(null.is_healthy());
    }
    let ComputedValue::Bytes(value) = null.eval_args(EvaluatedArgs::NullWitness(None)).unwrap()
    else {
        panic!("observed TRANSLATE NULL must own Bytes");
    };
    assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
    assert_eq!(value.into_option(), None);
    assert_eq!(null.kernel_invocations(), 1);
    assert!(null.is_healthy());
}

#[test]
fn local_evaluated_args_date_format_and_last_day_keep_distinct_clock_policies() {
    let cases: [(EvaluatedBytesOp, Vec<(EvaluatedArgs, Option<&[u8]>)>); 2] = [
        (
            EvaluatedBytesOp::DateFormatTextNative,
            vec![
                (
                    EvaluatedArgs::Bytes2(Some(b"0000-01-01".to_vec()), None),
                    None,
                ),
                // Untyped native clock fallback, not a typed SQL-cast fixture.
                (
                    EvaluatedArgs::Bytes2(
                        Some(b"2007-10-07 23:59:61".to_vec()),
                        Some(b"%T".to_vec()),
                    ),
                    Some(b"00:00:00"),
                ),
                (
                    EvaluatedArgs::Bytes2(Some(b"0000-01-01".to_vec()), Some(b"%X %x".to_vec())),
                    Some(b"0000 4294967295"),
                ),
            ],
        ),
        (
            EvaluatedBytesOp::LastDayTextNative,
            vec![
                (EvaluatedArgs::Bytes(None), None),
                (
                    EvaluatedArgs::Bytes(Some(b"2007-10-07 23:59:61".to_vec())),
                    None,
                ),
                (
                    EvaluatedArgs::Bytes(Some(b"2004-02-05".to_vec())),
                    Some(b"2004-02-29"),
                ),
            ],
        ),
    ];
    for (operation, cases) in cases {
        let final_expected = cases.last().unwrap().1;
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
            worker.eval_args(EvaluatedArgs::Int(None)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        let mut retained = None;
        for (index, (args, expected)) in cases.into_iter().enumerate() {
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!("native date formatting returned a non-Bytes value");
            };
            assert_eq!(value.value(), expected);
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            retained = value.into_option();
            assert_eq!(retained.as_deref(), expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        drop(worker);
        assert_eq!(retained.as_deref(), final_expected);
    }
}
