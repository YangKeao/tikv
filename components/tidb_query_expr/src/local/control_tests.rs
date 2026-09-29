// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use std::thread;

use tidb_query_datatype::{
    EvalType, FieldTypeAccessor, FieldTypeFlag, FieldTypeTp,
    codec::{
        batch::LazyBatchColumnVec,
        data_type::{ScalarValue, VectorValue},
    },
    expr::{Error, EvalContext},
};
use tikv_util::sys::thread::StdThreadBuildWrapper;
use tipb::{FieldType, ScalarFuncSig as Sig};

use super::*;

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
fn call(sig: Sig, args: Vec<LocalExpr>) -> LocalExpr {
    LocalExpr::Call {
        function: FunctionRef::TiPb(sig),
        args: args.into_boxed_slice(),
        return_type: ft(),
        metadata: CallMetadata::None,
    }
}
fn compile(expr: &LocalExpr, slots: usize) -> LocalProgram {
    compile_local(expr, &vec![ft(); slots], LocalCompileContext::default()).unwrap()
}

#[derive(Clone, Copy)]
enum Reply {
    Normal,
    WrongType,
    WrongLength,
    ResourceError,
    EvaluationError,
}
struct Bindings {
    schema: Vec<FieldType>,
    values: Vec<Vec<Option<i64>>>,
    trace: Vec<(usize, usize, usize)>,
    poison: Option<usize>,
    reply: Reply,
    warn: bool,
}
impl Bindings {
    fn new(values: Vec<Vec<Option<i64>>>) -> Self {
        Self {
            schema: vec![ft(); values.len()],
            values,
            trace: Vec::new(),
            poison: None,
            reply: Reply::Normal,
            warn: false,
        }
    }
}
impl LocalRuntimeServices for Bindings {
    fn binding_schema(&self) -> &[FieldType] {
        &self.schema
    }
    fn read_input(
        &mut self,
        ctx: &mut EvalContext,
        slot: usize,
        row: InputRow,
        expected: &FieldType,
    ) -> LocalResult<VectorValue> {
        assert_eq!(expected, &self.schema[slot]);
        self.trace.push((row.occurrence, row.input_row, slot));
        if self.warn {
            ctx.warnings.append_warning(Error::Eval(
                format!("read:{}:{}:{}", row.occurrence, row.input_row, slot),
                1234,
            ));
        }
        if self.poison == Some(slot) {
            return Err(LocalError::BindingContract("poison binding".into()));
        }
        match self.reply {
            Reply::WrongType => {
                return Ok(VectorValue::from_scalar(
                    &ScalarValue::Bytes(Some(vec![0xff])),
                    1,
                ));
            }
            Reply::WrongLength => {
                return Ok(VectorValue::from_scalar(&ScalarValue::Int(Some(9)), 2));
            }
            Reply::ResourceError => {
                return Err(LocalError::ResourceLimit(
                    "adapter resource sentinel".into(),
                ));
            }
            Reply::EvaluationError => {
                return Err(LocalError::Evaluation(other_err!(
                    "adapter evaluation sentinel"
                )));
            }
            Reply::Normal => {}
        }
        let value = self
            .values
            .get(slot)
            .and_then(|values| values.get(row.input_row))
            .ok_or_else(|| LocalError::BindingContract("missing binding value".into()))?;
        Ok(VectorValue::from_scalar(&ScalarValue::Int(*value), 1))
    }
}
fn run(
    program: &mut LocalProgram,
    bindings: &mut Bindings,
    physical_rows: usize,
    selection: &[usize],
) -> LocalResult<VectorValue> {
    program.eval_with_bindings(
        &mut LocalEvalState::default(),
        &mut EvalContext::default(),
        physical_rows,
        selection,
        bindings,
    )
}

#[test]
fn local_logical_controls_cover_three_valued_truth_and_demand() {
    for sig in [Sig::LogicalAnd, Sig::LogicalOr] {
        let mut program = compile(&call(sig, vec![input(0), input(1)]), 2);
        for lhs in [None, Some(0), Some(2), Some(-3)] {
            for rhs in [None, Some(0), Some(7), Some(-1)] {
                let absorbing = |value: Option<i64>| match sig {
                    Sig::LogicalAnd => value == Some(0),
                    _ => value.is_some_and(|value| value != 0),
                };
                let expected = if absorbing(lhs) || absorbing(rhs) {
                    Some((sig == Sig::LogicalOr) as i64)
                } else if lhs.is_none() || rhs.is_none() {
                    None
                } else {
                    Some((sig == Sig::LogicalAnd) as i64)
                };
                let mut bindings = Bindings::new(vec![vec![lhs], vec![rhs]]);
                if absorbing(lhs) {
                    bindings.poison = Some(1);
                }
                assert_eq!(
                    run(&mut program, &mut bindings, 1, &[0])
                        .unwrap()
                        .to_int_vec(),
                    vec![expected]
                );
                assert_eq!(bindings.trace.len(), if absorbing(lhs) { 1 } else { 2 });
                assert_eq!(bindings.trace[0], (0, 0, 0));
            }
        }
    }
}

#[test]
fn local_branch_controls_leave_dead_bindings_unread() {
    let cases = [
        (
            Sig::IfInt,
            vec![constant(Some(-1)), input(0), input(1)],
            Some(7),
        ),
        (
            Sig::IfInt,
            vec![constant(None), input(1), input(0)],
            Some(7),
        ),
        (Sig::IfNullInt, vec![input(0), input(1)], Some(7)),
        (Sig::IfNullInt, vec![constant(None), input(0)], Some(7)),
        (
            Sig::CaseWhenInt,
            vec![
                constant(None),
                input(1),
                constant(Some(1)),
                input(0),
                input(1),
            ],
            Some(7),
        ),
        (Sig::CaseWhenInt, vec![constant(Some(0)), input(1)], None),
        (Sig::CaseWhenInt, vec![input(0)], Some(7)),
        (Sig::CaseWhenInt, vec![], None),
        (
            Sig::CoalesceInt,
            vec![constant(None), input(0), input(1)],
            Some(7),
        ),
        (Sig::CoalesceInt, vec![constant(None), constant(None)], None),
        (Sig::CoalesceInt, vec![], None),
    ];
    for (sig, args, expected) in cases {
        let mut program = compile(&call(sig, args), 2);
        let mut bindings = Bindings::new(vec![vec![Some(7)], vec![]]);
        bindings.poison = Some(1);
        assert_eq!(
            run(&mut program, &mut bindings, 1, &[0])
                .unwrap()
                .to_int_vec(),
            vec![expected]
        );
        assert!(bindings.trace.iter().all(|&(_, _, slot)| slot == 0));
    }
}

#[test]
fn local_demand_rows_keep_occurrences_and_cross_the_batch_boundary() {
    let mut program = compile(
        &call(Sig::IfInt, vec![constant(Some(1)), input(0), input(1)]),
        2,
    );
    for count in [0, 1, 1024, 1025] {
        let values: Vec<_> = (0..count).map(|row| Some(row as i64)).collect();
        let selection: Vec<_> = (0..count).rev().collect();
        let mut bindings = Bindings::new(vec![values.clone(), vec![]]);
        bindings.poison = Some(1);
        let output = run(&mut program, &mut bindings, count, &selection).unwrap();
        assert_eq!(output.eval_type(), EvalType::Int);
        assert_eq!(
            output.to_int_vec(),
            values.into_iter().rev().collect::<Vec<_>>()
        );
        assert_eq!(
            bindings.trace,
            selection
                .iter()
                .enumerate()
                .map(|(occurrence, &row)| (occurrence, row, 0))
                .collect::<Vec<_>>()
        );
    }
    let mut bindings = Bindings::new(vec![vec![Some(3), Some(5), Some(9)], vec![]]);
    bindings.poison = Some(1);
    assert_eq!(
        run(&mut program, &mut bindings, 3, &[2, 0, 2])
            .unwrap()
            .to_int_vec(),
        vec![Some(9), Some(3), Some(9)]
    );
    assert_eq!(bindings.trace, vec![(0, 2, 0), (1, 0, 0), (2, 2, 0)]);
    bindings.values[0][2] = Some(22);
    assert_eq!(
        run(&mut program, &mut bindings, 3, &[2])
            .unwrap()
            .to_int_vec(),
        vec![Some(22)]
    );
}

#[test]
fn local_binding_preflight_and_result_validation_precede_loaders() {
    let mut program = compile(&call(Sig::AbsInt, vec![input(0)]), 1);
    let mut bindings = Bindings::new(vec![vec![Some(8)]]);
    assert!(matches!(
        run(&mut program, &mut bindings, 1, &[1]),
        Err(LocalError::InvalidBatch(_))
    ));
    bindings.schema[0]
        .as_mut_accessor()
        .set_flag(FieldTypeFlag::UNSIGNED);
    assert!(matches!(
        run(&mut program, &mut bindings, 1, &[0]),
        Err(LocalError::InvalidBatch(_))
    ));
    assert!(bindings.trace.is_empty());
    bindings.schema[0] = ft();
    for reply in [Reply::WrongType, Reply::WrongLength] {
        bindings.reply = reply;
        assert!(matches!(
            run(&mut program, &mut bindings, 1, &[0]),
            Err(LocalError::BindingContract(_))
        ));
    }
    bindings.reply = Reply::Normal;
    bindings.poison = Some(0);
    assert!(
        matches!(run(&mut program, &mut bindings, 1, &[0]), Err(LocalError::BindingContract(message)) if message == "poison binding")
    );
}

#[test]
fn local_binding_errors_retain_their_variants() {
    let mut program = compile(&input(0), 1);
    let mut bindings = Bindings::new(vec![vec![Some(8)]]);
    bindings.reply = Reply::ResourceError;
    assert!(
        matches!(run(&mut program, &mut bindings, 1, &[0]), Err(LocalError::ResourceLimit(message)) if message == "adapter resource sentinel")
    );
    bindings.reply = Reply::EvaluationError;
    assert!(matches!(
        run(&mut program, &mut bindings, 1, &[0]),
        Err(LocalError::Evaluation(_))
    ));
}

#[test]
fn local_ordinary_calls_remain_eager_on_null_inputs() {
    // Deliberate C2a boundary: scalar-TiDB NULL-stop needs an origin profile,
    // not a blanket change to the shared eager vector/batch call semantics.
    let mut program = compile(
        &call(Sig::PlusIntSignedSigned, vec![constant(None), input(0)]),
        1,
    );
    let mut bindings = Bindings::new(vec![vec![]]);
    bindings.poison = Some(0);
    assert!(matches!(
        run(&mut program, &mut bindings, 1, &[0]),
        Err(LocalError::BindingContract(_))
    ));
    assert_eq!(bindings.trace, vec![(0, 0, 0)]);
}

#[test]
fn local_parent_error_precedes_later_rows_and_preserves_warning_prefix() {
    let mut program = compile(
        &call(
            Sig::PlusIntSignedSigned,
            vec![input(0), constant(Some(i64::MAX))],
        ),
        1,
    );
    let mut bindings = Bindings::new(vec![vec![Some(0), Some(1), Some(2)]]);
    bindings.warn = true;
    let mut ctx = EvalContext::default();
    ctx.warnings
        .append_warning(Error::Eval("prior".into(), 1234));
    assert!(matches!(
        program.eval_with_bindings(
            &mut LocalEvalState::default(),
            &mut ctx,
            3,
            &[0, 1, 2],
            &mut bindings
        ),
        Err(LocalError::Evaluation(_))
    ));
    assert_eq!(bindings.trace, vec![(0, 0, 0), (1, 1, 0)]);
    assert_eq!(ctx.warnings.warning_cnt, 3);
    assert!(ctx.warnings.warnings[0].get_msg().ends_with("prior"));
    assert!(ctx.warnings.warnings[1].get_msg().ends_with("read:0:0:0"));
    assert!(ctx.warnings.warnings[2].get_msg().ends_with("read:1:1:0"));
}

#[test]
fn local_malformed_binding_keeps_already_emitted_diagnostics() {
    let mut program = compile(&input(0), 1);
    let mut bindings = Bindings::new(vec![vec![Some(8)]]);
    bindings.warn = true;
    bindings.reply = Reply::WrongLength;
    let mut ctx = EvalContext::default();
    assert!(matches!(
        program.eval_with_bindings(
            &mut LocalEvalState::default(),
            &mut ctx,
            1,
            &[0],
            &mut bindings
        ),
        Err(LocalError::BindingContract(_))
    ));
    assert_eq!(ctx.warnings.warning_cnt, 1);
    assert!(ctx.warnings.warnings[0].get_msg().ends_with("read:0:0:0"));
}

#[test]
fn local_controls_validate_arity_before_execution() {
    for (sig, args) in [
        (Sig::IfInt, vec![constant(Some(1)), constant(Some(0))]),
        (Sig::IfNullInt, vec![constant(None)]),
        (Sig::LogicalAnd, vec![constant(Some(1))]),
        (Sig::LogicalOr, vec![constant(Some(0))]),
    ] {
        assert!(matches!(
            compile_local(&call(sig, args), &[], LocalCompileContext::default()),
            Err(LocalError::InvalidSpec(_))
        ));
    }
}

#[test]
fn local_execution_defaults_and_small_refusals_are_explicit() {
    let defaults = ExecutionLimits::default();
    assert_eq!(defaults.max_steps, u64::MAX);
    assert_eq!(defaults.max_frame_depth, 1024);
    assert_eq!(defaults.max_active_tasks, 256);
    assert_eq!(defaults.max_retained_bytes, 64 * 1024 * 1024);
    let mut program = compile(
        &call(Sig::IfInt, vec![constant(Some(1)), input(0), input(1)]),
        2,
    );
    for limits in [
        ExecutionLimits {
            max_steps: 0,
            ..defaults
        },
        ExecutionLimits {
            max_frame_depth: 1,
            ..defaults
        },
        ExecutionLimits {
            max_retained_bytes: 0,
            ..defaults
        },
    ] {
        let mut bindings = Bindings::new(vec![vec![Some(8)], vec![]]);
        let mut state = LocalEvalState::with_limits(limits);
        assert!(matches!(
            program.eval_with_bindings(
                &mut state,
                &mut EvalContext::default(),
                1,
                &[0],
                &mut bindings
            ),
            Err(LocalError::ResourceLimit(_))
        ));
        assert!(bindings.trace.is_empty());
        assert!(
            program
                .eval_with_bindings(
                    &mut state,
                    &mut EvalContext::default(),
                    1,
                    &[],
                    &mut bindings
                )
                .unwrap()
                .is_empty()
        );
    }
    // Tasks are not implemented in C2a; zero task capacity does not invent a
    // HostCall restriction or a cancellation guarantee for ordinary evaluation.
    let mut bindings = Bindings::new(vec![vec![Some(8)], vec![]]);
    assert_eq!(
        program
            .eval_with_bindings(
                &mut LocalEvalState::with_limits(ExecutionLimits {
                    max_active_tasks: 0,
                    ..defaults
                }),
                &mut EvalContext::default(),
                1,
                &[0],
                &mut bindings
            )
            .unwrap()
            .to_int_vec(),
        vec![Some(8)]
    );
}

#[test]
fn local_work_limit_is_shared_across_occurrences_and_state_reusable() {
    let mut program = compile(&input(0), 1);
    let mut bindings = Bindings::new(vec![vec![Some(8)]]);
    let mut state = LocalEvalState::new(2); // one node plus one demanded read
    assert!(matches!(
        program.eval_with_bindings(
            &mut state,
            &mut EvalContext::default(),
            1,
            &[0, 0],
            &mut bindings
        ),
        Err(LocalError::ResourceLimit(_))
    ));
    assert_eq!(bindings.trace, vec![(0, 0, 0)]);
    assert_eq!(
        program
            .eval_with_bindings(
                &mut state,
                &mut EvalContext::default(),
                1,
                &[0],
                &mut bindings
            )
            .unwrap()
            .to_int_vec(),
        vec![Some(8)]
    );
}

#[test]
fn local_deep_controls_never_fall_back_to_eager_evaluation() {
    thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn_wrapper(|| {
            for depth in [33, 64, 256] {
                let mut expr = input(0);
                for _ in 0..depth {
                    expr = call(Sig::IfInt, vec![constant(Some(1)), expr, input(1)]);
                }
                let mut program = compile_local(
                    &expr,
                    &[ft(), ft()],
                    LocalCompileContext {
                        limits: CompileLimits {
                            max_depth: depth + 1,
                            max_nodes: depth * 3 + 1,
                        },
                    },
                )
                .unwrap();
                let mut bindings = Bindings::new(vec![vec![Some(9)], vec![]]);
                bindings.poison = Some(1);
                assert_eq!(
                    run(&mut program, &mut bindings, 1, &[0])
                        .unwrap()
                        .to_int_vec(),
                    vec![Some(9)]
                );
                assert_eq!(bindings.trace, vec![(0, 0, 0)]);
                // Same official metadata traversal and iterative destruction.
                assert_eq!(program.expression.work_count(), depth * 3 + 1);
                assert_eq!(program.expression.column_ref_count(), depth + 1);
                drop(program);
                drop(expr);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn local_decoded_controls_use_the_same_official_driver() {
    let mut program = compile(
        &call(Sig::IfNullInt, vec![constant(None), constant(Some(5))]),
        0,
    );
    let output = program
        .eval(
            &mut LocalEvalState::default(),
            &mut EvalContext::default(),
            LocalBatch {
                columns: &LazyBatchColumnVec::empty(),
                physical_rows: 3,
                selection: &[2, 0, 2],
            },
        )
        .unwrap();
    assert_eq!(output.to_int_vec(), vec![Some(5); 3]);
}
