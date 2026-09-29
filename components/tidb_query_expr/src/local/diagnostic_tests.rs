// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Staged tests for the additive reported entrypoint. These are local failure
//! identity/prefix tests, not proof of PB ingestion, native SQL rendering,
//! per-warning attribution, or a native numeric-batch execution profile.

use std::{
    error::Error as StdError,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use tidb_query_common::{
    Error as CommonError,
    error::{ErrorInner, EvaluateError, StorageError},
};
use tidb_query_datatype::{
    FieldTypeTp,
    codec::data_type::{ScalarValue, VectorValue},
    expr::{Error as EvalError, EvalConfig, EvalContext},
};
use tikv_util::sys::thread::StdThreadBuildWrapper;
use tipb::{FieldType, ScalarFuncSig as Sig};

use super::{
    diagnostic::{FailureRecorder, LocalFailureSite, LocalFailureStage, ReportedLocalFailure},
    *,
};

fn ft() -> FieldType {
    FieldTypeTp::LongLong.into()
}
fn value(value: Option<i64>) -> VectorValue {
    VectorValue::from_scalar(&ScalarValue::Int(value), 1)
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
fn plus(left: LocalExpr, right: LocalExpr) -> LocalExpr {
    call(Sig::PlusInt, vec![left, right])
}
fn row(occurrence: usize, input_row: usize) -> InputRow {
    InputRow {
        occurrence,
        input_row,
    }
}

// Deliberately equal source IDs on distinct call ordinals, including high bits.
const SOURCE: OrdinarySourceId =
    OrdinarySourceId::new(0xfedc_ba98_7654_3210, 0x8000_0000_0000_0001);
fn call_record(ordinal: usize) -> OrdinaryCallSite {
    if ordinal == 0 {
        OrdinaryCallSite::typed_row(ordinal, SOURCE)
    } else {
        OrdinaryCallSite::pb_row(ordinal, SOURCE, 203)
    }
}
fn compile_with_limits(expr: &LocalExpr, slots: usize, limits: CompileLimits) -> LocalProgram {
    let schema = vec![ft(); slots];
    let mut pending = vec![expr];
    let mut ordinal = 0;
    let mut sites = Vec::new();
    while let Some(node) = pending.pop() {
        if let LocalExpr::Call { args, .. } = node {
            sites.push(call_record(ordinal));
            pending.extend(args.iter().rev());
        }
        ordinal += 1;
    }
    let facts =
        OrdinaryProfileSpec::new(expr, &schema, OrdinaryProfile::TypedRow, sites, limits).unwrap();
    compile_local_profiled(expr, &schema, LocalCompileContext { limits }, &facts).unwrap()
}
fn compile(expr: &LocalExpr, slots: usize) -> LocalProgram {
    compile_with_limits(expr, slots, CompileLimits::default())
}
fn evaluation(code: i32, message: &str) -> LocalError {
    LocalError::Evaluation(
        EvaluateError::Custom {
            code,
            msg: message.into(),
        }
        .into(),
    )
}
fn common_ptr(error: &LocalError) -> *const ErrorInner {
    let LocalError::Evaluation(error) = error else {
        panic!("test requires a common error");
    };
    error.0.as_ref() as *const ErrorInner
}
fn assert_kernel(report: &ReportedLocalFailure, ordinal: usize, expected_row: InputRow) {
    assert_eq!(report.stage(), LocalFailureStage::Kernel);
    assert_eq!(
        report.site(),
        Some(&LocalFailureSite::Kernel {
            call: call_record(ordinal),
            row: expected_row
        })
    );
    assert_eq!(report.sql_error_code(), Some(1690));
    assert!(matches!(report.error(), LocalError::Evaluation(_)));
}
fn assert_input(report: &ReportedLocalFailure, slot: usize, expected_row: InputRow) {
    assert_eq!(report.stage(), LocalFailureStage::Input);
    assert_eq!(
        report.site(),
        Some(&LocalFailureSite::InputSlot {
            slot,
            row: expected_row
        })
    );
}
fn assert_unsited(report: &ReportedLocalFailure, stage: LocalFailureStage) {
    assert_eq!(report.stage(), stage);
    assert!(report.site().is_none());
}

#[test]
fn diagnostic_owned_error_display_source_and_box_identity_are_preserved() {
    let error = evaluation(1690, "original payload, not source text");
    let pointer = common_ptr(&error);
    let original_display = error.to_string();
    let mut recorder = FailureRecorder::default();
    let error = recorder.capture_kernel(&call_record(2), row(3, 7), error);
    assert_eq!(common_ptr(&error), pointer);
    let report = recorder.into_failure(error);
    assert_kernel(&report, 2, row(3, 7));
    assert_eq!(report.to_string(), original_display);
    let source = StdError::source(&report)
        .unwrap()
        .downcast_ref::<LocalError>()
        .unwrap();
    assert!(std::ptr::eq(source, report.error()));
    assert_eq!(common_ptr(report.error()), pointer);
    let error = report.into_error();
    assert_eq!(common_ptr(&error), pointer);
    let LocalError::Evaluation(CommonError(inner)) = error else {
        unreachable!()
    };
    assert!(
        matches!(inner.as_ref(), ErrorInner::Evaluate(EvaluateError::Custom { code: 1690, msg }) if msg == "original payload, not source text")
    );
}

#[test]
fn diagnostic_code_getter_uses_typed_codes_and_unsited_stage_classes() {
    use LocalFailureStage::{Resource, Unattributed, Validation};
    let cases = vec![
        (
            evaluation(1690, "not an overflow classifier"),
            Unattributed,
            Some(1690),
        ),
        (evaluation(-7, "1690"), Unattributed, Some(-7)),
        (evaluation(0, "1690"), Unattributed, Some(0)),
        (
            LocalError::Evaluation(EvaluateError::Other("1690 BIGINT overflow".into()).into()),
            Unattributed,
            Some(10000),
        ),
        (
            LocalError::Evaluation(EvaluateError::DeadlineExceeded.into()),
            Unattributed,
            Some(9007),
        ),
        (
            LocalError::Evaluation(
                EvaluateError::InvalidCharacterString {
                    charset: "utf8".into(),
                }
                .into(),
            ),
            Unattributed,
            Some(1300),
        ),
        (
            LocalError::Evaluation(
                StorageError(std::io::Error::other("1690 storage").into()).into(),
            ),
            Unattributed,
            None,
        ),
        (LocalError::InvalidSpec("1690".into()), Validation, None),
        (LocalError::InvalidBatch("1690".into()), Validation, None),
        (LocalError::BindingContract("1690".into()), Validation, None),
        (LocalError::HostContract("1690".into()), Validation, None),
        (LocalError::ResourceLimit("1690".into()), Resource, None),
    ];
    for (error, stage, code) in cases {
        let report = FailureRecorder::default().into_failure(error);
        assert_unsited(&report, stage);
        assert_eq!(report.sql_error_code(), code);
    }
}

#[derive(Debug)]
struct DisplayBomb(Arc<AtomicUsize>);
impl fmt::Display for DisplayBomb {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("storage Display must not classify a diagnostic");
    }
}
impl StdError for DisplayBomb {}

#[test]
fn diagnostic_capture_and_getters_never_render_a_storage_error() {
    let displays = Arc::new(AtomicUsize::new(0));
    let error =
        LocalError::Evaluation(StorageError(DisplayBomb(Arc::clone(&displays)).into()).into());
    let pointer = common_ptr(&error);
    let mut recorder = FailureRecorder::default();
    let error = recorder.capture_input(9, row(2, 4), error);
    let report = recorder.into_failure(error);
    assert_input(&report, 9, row(2, 4));
    assert_eq!(report.sql_error_code(), None);
    assert_eq!(common_ptr(report.error()), pointer);
    assert!(StdError::source(&report).unwrap().is::<LocalError>());
    assert_eq!(displays.load(Ordering::SeqCst), 0);
    // Prove the fixture would catch string-based classification. Display itself
    // intentionally delegates to the original error; it is not panic shielding.
    assert!(catch_unwind(AssertUnwindSafe(|| report.to_string())).is_err());
    assert_eq!(displays.load(Ordering::SeqCst), 1);
    assert_eq!(common_ptr(&report.into_error()), pointer);
}

#[test]
fn diagnostic_first_capture_wins_without_changing_the_error() {
    for input_first in [false, true] {
        let error = evaluation(1690, "same primary");
        let pointer = common_ptr(&error);
        let mut recorder = FailureRecorder::default();
        let error = if input_first {
            recorder.capture_input(11, row(2, 9), error)
        } else {
            recorder.capture_kernel(&call_record(2), row(2, 9), error)
        };
        let error = recorder.capture_kernel(&call_record(0), row(7, 8), error);
        let error = recorder.capture_input(88, row(8, 7), error);
        let report = recorder.into_failure(error);
        if input_first {
            assert_input(&report, 11, row(2, 9));
        } else {
            assert_kernel(&report, 2, row(2, 9));
        }
        assert_eq!(common_ptr(report.error()), pointer);
    }
    let mut recorder = FailureRecorder::default();
    let error = recorder.capture_input(
        0,
        row(0, 0),
        LocalError::ResourceLimit("input-owned limit".into()),
    );
    let report = recorder.into_failure(error);
    assert_input(&report, 0, row(0, 0));
    assert!(matches!(report.error(), LocalError::ResourceLimit(_)));
    assert_eq!(report.sql_error_code(), None);
}

// Minimal recording imports; no native evaluator or SQL-source renderer.
enum Reply {
    Error(LocalError),
    Value(Option<i64>),
    WrongType,
    WrongLength,
    Panic,
}
struct Bindings {
    schema: Vec<FieldType>,
    values: Vec<Vec<Option<i64>>>,
    reads: Vec<(usize, InputRow)>,
    reply_at: Option<(usize, Reply)>,
    warnings: bool,
}
impl Bindings {
    fn new(values: Vec<Vec<Option<i64>>>) -> Self {
        Self {
            schema: vec![ft(); values.len()],
            values,
            reads: vec![],
            reply_at: None,
            warnings: false,
        }
    }
    fn reset(&mut self) {
        self.reads.clear();
        self.reply_at = None;
    }
}
fn warn(ctx: &mut EvalContext, text: String) {
    ctx.warnings.append_warning(EvalError::Eval(text, 1234));
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
        self.reads.push((slot, row));
        if self.warnings {
            warn(
                ctx,
                format!("read:{slot}:{}:{}", row.occurrence, row.input_row),
            );
        }
        if self
            .reply_at
            .as_ref()
            .is_some_and(|(at, _)| *at == self.reads.len())
        {
            return match self.reply_at.take().unwrap().1 {
                Reply::Error(error) => Err(error),
                Reply::Value(v) => Ok(value(v)),
                Reply::WrongType => Ok(VectorValue::from_scalar(&ScalarValue::Bytes(None), 1)),
                Reply::WrongLength => Ok(VectorValue::from_scalar(&ScalarValue::Int(None), 2)),
                Reply::Panic => panic!("primary diagnostic input panic"),
            };
        }
        Ok(value(self.values[slot][row.input_row]))
    }
    fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
        panic!("host-free reported evaluation consulted the optional hook")
    }
}
fn run(
    program: &mut LocalProgram,
    services: &mut Bindings,
    physical_rows: usize,
    selection: &[usize],
) -> Result<VectorValue, ReportedLocalFailure> {
    program.eval_with_bindings_reported(
        &mut LocalEvalState::default(),
        &mut EvalContext::default(),
        physical_rows,
        selection,
        services,
    )
}

#[test]
fn reported_nested_and_root_kernel_errors_keep_exact_owned_call_records() {
    for (values, ordinal) in [
        (vec![Some(0), Some(i64::MAX), Some(1)], 2),
        (vec![Some(i64::MAX), Some(0), Some(1)], 0),
    ] {
        let report = {
            let expr = plus(input(0), plus(input(1), input(2)));
            let mut program = compile(&expr, 3);
            let mut services =
                Bindings::new(values.into_iter().map(|v| vec![v, Some(9)]).collect());
            let report = run(&mut program, &mut services, 2, &[0, 1]).unwrap_err();
            assert_eq!(
                services.reads,
                vec![(0, row(0, 0)), (1, row(0, 0)), (2, row(0, 0))]
            );
            report
        }; // Program, source tree and service have all been dropped.
        assert_kernel(&report, ordinal, row(0, 0));
        let Some(LocalFailureSite::Kernel { call, .. }) = report.site() else {
            unreachable!()
        };
        assert_eq!(call.source().unit(), 0xfedc_ba98_7654_3210);
        assert_eq!(call.source().node(), 0x8000_0000_0000_0001);
        assert_eq!(
            call.original_pb_signature(),
            if ordinal == 0 { None } else { Some(203) }
        );
    }
}

#[test]
fn reported_root_input_fast_path_preserves_every_callback_error_variant() {
    let mut program = compile(&input(0), 1);
    for error in [
        LocalError::InvalidSpec("callback spec".into()),
        LocalError::InvalidBatch("callback batch".into()),
        LocalError::BindingContract("callback binding".into()),
        LocalError::HostContract("callback contract".into()),
        LocalError::ResourceLimit("callback resource".into()),
        evaluation(1690, "input overflow is NOT plus overflow"),
    ] {
        let kind = std::mem::discriminant(&error);
        let pointer = if matches!(&error, LocalError::Evaluation(_)) {
            Some(common_ptr(&error))
        } else {
            None
        };
        let mut services = Bindings::new(vec![vec![Some(1)]]);
        services.reply_at = Some((1, Reply::Error(error)));
        let report = run(&mut program, &mut services, 1, &[0]).unwrap_err();
        assert_input(&report, 0, row(0, 0));
        assert_eq!(std::mem::discriminant(report.error()), kind);
        assert_eq!(report.sql_error_code(), pointer.map(|_| 1690));
        if let Some(pointer) = pointer {
            assert_eq!(common_ptr(report.error()), pointer);
        }
        assert_eq!(services.reads, vec![(0, row(0, 0))]);
    }
}

#[test]
fn reported_shared_binding_is_not_a_leaf_ordinal_or_nearest_call() {
    let expr = plus(input(0), plus(input(1), input(1)));
    let mut program = compile(&expr, 2);
    let mut services = Bindings::new(vec![vec![Some(0)], vec![Some(1)]]);
    services.reply_at = Some((3, Reply::Error(evaluation(1690, "third read"))));
    let report = run(&mut program, &mut services, 1, &[0]).unwrap_err();
    // The failing source leaf has preorder ordinal4 and its parent call is2,
    // but the actual callback was for binding slot1, reused by two leaves.
    assert_input(&report, 1, row(0, 0));
    assert_eq!(report.sql_error_code(), Some(1690));
    assert_eq!(
        services.reads,
        vec![(0, row(0, 0)), (1, row(0, 0)), (1, row(0, 0))]
    );
}

#[test]
fn reported_duplicate_physical_rows_keep_distinct_occurrence_sites() {
    let mut program = compile(&plus(input(0), constant(Some(1))), 1);
    for kernel in [false, true] {
        let mut services = Bindings::new(vec![vec![Some(0); 3]]);
        services.reply_at = Some((
            3,
            if kernel {
                Reply::Value(Some(i64::MAX))
            } else {
                Reply::Error(evaluation(1690, "third occurrence"))
            },
        ));
        let report = run(&mut program, &mut services, 3, &[2, 0, 2]).unwrap_err();
        if kernel {
            assert_kernel(&report, 0, row(2, 2));
        } else {
            assert_input(&report, 0, row(2, 2));
        }
        assert_eq!(
            services.reads,
            vec![(0, row(0, 2)), (0, row(1, 0)), (0, row(2, 2))]
        );
    }
}

#[test]
fn reported_successful_bad_input_reply_is_unsited_validation() {
    let mut program = compile(&plus(input(0), constant(Some(1))), 1);
    for reply in [Reply::WrongType, Reply::WrongLength] {
        let mut services = Bindings::new(vec![vec![Some(1), Some(2)]]);
        services.reply_at = Some((1, reply));
        let report = run(&mut program, &mut services, 2, &[0, 1]).unwrap_err();
        assert_unsited(&report, LocalFailureStage::Validation);
        assert!(matches!(report.error(), LocalError::BindingContract(_)));
        assert_eq!(report.sql_error_code(), None);
        assert_eq!(services.reads, vec![(0, row(0, 0))]);
    }
}

#[test]
fn reported_preflight_and_budget_errors_never_invent_operation_sites() {
    let mut program = compile(&plus(input(0), input(1)), 2);
    let mut services = Bindings::new(vec![vec![Some(1)], vec![Some(2)]]);
    services.reply_at = Some((1, Reply::Panic));
    services.schema[1].set_flen(7);
    for selection in [&[][..], &[0][..]] {
        let report = run(&mut program, &mut services, 1, selection).unwrap_err();
        assert_unsited(&report, LocalFailureStage::Validation);
        assert!(matches!(report.error(), LocalError::InvalidBatch(_)));
    }
    services.schema[1] = ft();
    let report = run(&mut program, &mut services, 1, &[1]).unwrap_err();
    assert_unsited(&report, LocalFailureStage::Validation);
    assert!(matches!(report.error(), LocalError::InvalidBatch(_)));
    assert!(services.reads.is_empty());
    services.reset();

    // Tick4 is a successful left read; acceptance at5 can fail without turning
    // that success into an Input site. Tick9 has accepted both successful reads.
    for (max_steps, reads) in [(0, 0), (3, 0), (4, 1), (9, 2)] {
        services.reset();
        let report = program
            .eval_with_bindings_reported(
                &mut LocalEvalState::with_limits(ExecutionLimits {
                    max_steps,
                    ..ExecutionLimits::default()
                }),
                &mut EvalContext::default(),
                1,
                &[0],
                &mut services,
            )
            .unwrap_err();
        assert_unsited(&report, LocalFailureStage::Resource);
        assert!(matches!(report.error(), LocalError::ResourceLimit(_)));
        assert_eq!(report.sql_error_code(), None);
        assert_eq!(services.reads.len(), reads);
    }
    for limits in [
        ExecutionLimits {
            max_frame_depth: 1,
            ..ExecutionLimits::default()
        },
        ExecutionLimits {
            max_retained_bytes: 0,
            ..ExecutionLimits::default()
        },
    ] {
        services.reset();
        let report = program
            .eval_with_bindings_reported(
                &mut LocalEvalState::with_limits(limits),
                &mut EvalContext::default(),
                1,
                &[0],
                &mut services,
            )
            .unwrap_err();
        assert_unsited(&report, LocalFailureStage::Resource);
        assert!(services.reads.is_empty());
    }
}

#[test]
fn reported_kernel_site_requires_an_actual_err_not_entry_or_prior_success() {
    let mut program = compile(&plus(constant(Some(i64::MAX)), constant(Some(1))), 0);
    for max_steps in [7, 8] {
        let report = program
            .eval_with_bindings_reported(
                &mut LocalEvalState::with_limits(ExecutionLimits {
                    max_steps,
                    ..ExecutionLimits::default()
                }),
                &mut EvalContext::default(),
                1,
                &[0],
                &mut Bindings::new(vec![]),
            )
            .unwrap_err();
        if max_steps == 7 {
            assert_unsited(&report, LocalFailureStage::Resource);
        } else {
            assert_kernel(&report, 0, row(0, 0));
        }
    }
    // Outer root/request are ticks1/2; the inner constant PLUS reaches its
    // kernel at10. With a successful inner kernel, outer acceptance fails at11.
    // Pairing it with overflow at the SAME budget proves this is after a kernel,
    // not merely a denial before any kernel could have run.
    for left in [1, i64::MAX] {
        let expr = plus(plus(constant(Some(left)), constant(Some(1))), input(0));
        let mut program = compile(&expr, 1);
        let mut services = Bindings::new(vec![vec![Some(3)]]);
        let report = program
            .eval_with_bindings_reported(
                &mut LocalEvalState::with_limits(ExecutionLimits {
                    max_steps: 10,
                    ..ExecutionLimits::default()
                }),
                &mut EvalContext::default(),
                1,
                &[0],
                &mut services,
            )
            .unwrap_err();
        if left == 1 {
            assert_unsited(&report, LocalFailureStage::Resource);
            assert_eq!(report.sql_error_code(), None);
        } else {
            assert_kernel(&report, 1, row(0, 0));
        }
        assert!(services.reads.is_empty());
    }
}

#[test]
fn reported_retry_empty_and_panic_do_not_leave_stale_sites() {
    let mut program = compile(&plus(input(0), constant(Some(1))), 1);
    let mut services = Bindings::new(vec![vec![Some(i64::MAX)]]);
    let mut state = LocalEvalState::default();
    let mut ctx = EvalContext::default();
    let first = program
        .eval_with_bindings_reported(&mut state, &mut ctx, 1, &[0], &mut services)
        .unwrap_err();
    assert_kernel(&first, 0, row(0, 0));
    services.values[0][0] = Some(0);
    services.reset();
    assert_eq!(
        program
            .eval_with_bindings_reported(&mut state, &mut ctx, 1, &[0], &mut services)
            .unwrap()
            .to_int_vec(),
        vec![Some(1)]
    );
    services.reset();
    assert!(
        program
            .eval_with_bindings_reported(&mut state, &mut ctx, 1, &[], &mut services)
            .unwrap()
            .is_empty()
    );
    assert!(services.reads.is_empty());
    services.schema[0].set_flen(17);
    let report = program
        .eval_with_bindings_reported(&mut state, &mut ctx, 1, &[0], &mut services)
        .unwrap_err();
    assert_unsited(&report, LocalFailureStage::Validation);
    services.schema[0] = ft();
    services.reply_at = Some((1, Reply::Error(evaluation(1690, "new input failure"))));
    let report = program
        .eval_with_bindings_reported(&mut state, &mut ctx, 1, &[0], &mut services)
        .unwrap_err();
    assert_input(&report, 0, row(0, 0));
    services.reset();
    services.reply_at = Some((1, Reply::Panic));
    let panic = catch_unwind(AssertUnwindSafe(|| {
        program.eval_with_bindings_reported(&mut state, &mut ctx, 1, &[0], &mut services)
    }))
    .unwrap_err();
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
    assert_eq!(message, Some("primary diagnostic input panic"));
    // No warning-delta assertion across a panic: there is no returned receipt.
    services.reset();
    assert_eq!(
        program
            .eval_with_bindings_reported(&mut state, &mut ctx, 1, &[0], &mut services)
            .unwrap()
            .to_int_vec(),
        vec![Some(1)]
    );
    let report = program
        .eval_with_bindings_reported(&mut state, &mut ctx, 1, &[1], &mut services)
        .unwrap_err();
    assert_unsited(&report, LocalFailureStage::Validation);
    assert_kernel(&first, 0, row(0, 0)); // The earlier owned receipt is unchanged.
}

#[test]
fn reported_null_rhs_and_unselected_rows_have_no_effects_or_reports() {
    let expr = plus(input(0), plus(input(1), constant(Some(1))));
    let mut program = compile(&expr, 2);
    let mut services = Bindings::new(vec![vec![None, Some(1), None], vec![Some(i64::MAX); 3]]);
    services.warnings = true;
    let mut ctx = EvalContext::default();
    warn(&mut ctx, "prior".into());
    assert_eq!(
        program
            .eval_with_bindings_reported(
                &mut LocalEvalState::default(),
                &mut ctx,
                3,
                &[2, 0, 2],
                &mut services
            )
            .unwrap()
            .to_int_vec(),
        vec![None; 3]
    );
    assert_eq!(
        services.reads,
        vec![(0, row(0, 2)), (0, row(1, 0)), (0, row(2, 2))]
    );
    assert_eq!(ctx.warnings.warning_cnt, 4);
    services.reset();
    let report = program
        .eval_with_bindings_reported(
            &mut LocalEvalState::default(),
            &mut ctx,
            3,
            &[1],
            &mut services,
        )
        .unwrap_err();
    assert_kernel(&report, 2, row(0, 1));
    assert_eq!(services.reads, vec![(0, row(0, 1)), (1, row(0, 1))]);
    assert_eq!(ctx.warnings.warning_cnt, 6);
}

#[derive(Debug, PartialEq, Eq)]
struct WarningEndpoint {
    count: usize,
    stored_len: usize,
    details: Vec<(i32, String)>,
}
fn endpoint(ctx: &EvalContext) -> WarningEndpoint {
    WarningEndpoint {
        count: ctx.warnings.warning_cnt,
        stored_len: ctx.warnings.warnings.len(),
        details: ctx
            .warnings
            .warnings
            .iter()
            .map(|w| (w.get_code(), w.get_msg().to_owned()))
            .collect(),
    }
}

#[test]
fn reported_warning_endpoints_preserve_live_count_and_actual_receiver_cap() {
    let default_cap = EvalConfig::default().max_warning_cnt;
    // Configured0, configured1, default, full1, full-default, and a replaced
    // Default warning receiver (cap0) whose configured cap is nevertheless7.
    for (cap, prior, replace_receiver) in [
        (Some(0), 1, false),
        (Some(1), 0, false),
        (None, 1, false),
        (Some(1), 1, false),
        (None, default_cap, false),
        (Some(7), 1, true),
    ] {
        for outcome in 0..3 {
            // success, actual input Err, actual kernel Err
            let mut config = EvalConfig::default();
            if let Some(cap) = cap {
                config.set_max_warning_cnt(cap);
            }
            let receiver_cap = if replace_receiver {
                0
            } else {
                config.max_warning_cnt
            };
            let mut ctx = EvalContext::new(Arc::new(config));
            if replace_receiver {
                ctx.warnings = Default::default();
                assert_eq!(ctx.cfg.max_warning_cnt, 7);
            }
            for n in 0..prior {
                warn(&mut ctx, format!("prior:{n}"));
            }
            let before = endpoint(&ctx);
            let mut program = compile(&plus(input(0), input(1)), 2);
            let mut services = Bindings::new(vec![
                vec![Some(if outcome == 2 { i64::MAX } else { 1 })],
                vec![Some(1)],
            ]);
            services.warnings = true;
            if outcome == 1 {
                services.reply_at =
                    Some((2, Reply::Error(evaluation(1690, "input warning prefix"))));
            }
            let result = program.eval_with_bindings_reported(
                &mut LocalEvalState::default(),
                &mut ctx,
                1,
                &[0],
                &mut services,
            );
            let after = endpoint(&ctx); // Caller-owned snapshots on success AND failure.
            match outcome {
                0 => assert_eq!(result.unwrap().to_int_vec(), vec![Some(2)]),
                1 => assert_input(&result.unwrap_err(), 1, row(0, 0)),
                _ => assert_kernel(&result.unwrap_err(), 0, row(0, 0)),
            }
            assert_eq!(after.count, before.count + 2);
            let mut expected = before.details.clone();
            for text in ["read:0:0:0", "read:1:0:0"] {
                if expected.len() < receiver_cap {
                    expected.push((1234, EvalError::Eval(text.into(), 1234).to_string()));
                }
            }
            assert_eq!(after.stored_len, expected.len());
            assert_eq!(after.details, expected);
            assert_eq!(endpoint(&ctx), after); // Inspection/drop did not drain or append.
        }
    }
}

#[test]
fn reported_legacy_eager_error_stays_unattributed_and_controls_still_work() {
    let expr = call(
        Sig::PlusIntSignedSigned,
        vec![constant(Some(i64::MAX)), constant(Some(1))],
    );
    let mut program = compile_local(&expr, &[], LocalCompileContext::default()).unwrap();
    let mut services = Bindings::new(vec![]);
    let original = program
        .eval_with_bindings(
            &mut LocalEvalState::default(),
            &mut EvalContext::default(),
            1,
            &[0],
            &mut services,
        )
        .unwrap_err();
    let report = run(&mut program, &mut services, 1, &[0]).unwrap_err();
    assert_unsited(&report, LocalFailureStage::Unattributed);
    assert_eq!(report.sql_error_code(), Some(1690));
    assert_eq!(report.error().to_string(), original.to_string());
    let expr = call(Sig::IfInt, vec![constant(Some(1)), input(0), input(1)]);
    let mut program = compile_local(&expr, &[ft(), ft()], LocalCompileContext::default()).unwrap();
    let mut services = Bindings::new(vec![vec![Some(5)], vec![Some(9)]]);
    services.reply_at = Some((2, Reply::Panic));
    assert_eq!(
        program
            .eval_with_bindings(
                &mut LocalEvalState::default(),
                &mut EvalContext::default(),
                1,
                &[0],
                &mut services
            )
            .unwrap()
            .to_int_vec(),
        vec![Some(5)]
    );
    services.reset();
    services.reply_at = Some((2, Reply::Panic));
    assert_eq!(
        run(&mut program, &mut services, 1, &[0])
            .unwrap()
            .to_int_vec(),
        vec![Some(5)]
    );
    assert_eq!(services.reads, vec![(0, row(0, 0))]);
}

struct ReadyHost {
    catalog: HostCatalog,
    schema: Vec<FieldType>,
    hooks: usize,
    starts: usize,
    resumes: usize,
    cancels: usize,
}
impl LocalRuntimeServices for ReadyHost {
    fn binding_schema(&self) -> &[FieldType] {
        &self.schema
    }
    fn read_input(
        &mut self,
        _: &mut EvalContext,
        _: usize,
        _: InputRow,
        _: &FieldType,
    ) -> LocalResult<VectorValue> {
        panic!("zero-argument host must not import input")
    }
    fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
        self.hooks += 1;
        Some(self)
    }
}
impl LocalHostServices for ReadyHost {
    fn catalog_key(&self) -> &HostCatalogKey {
        self.catalog.key()
    }
    fn start(
        &mut self,
        _: &mut EvalContext,
        invocation: HostInvocation<'_>,
    ) -> LocalResult<HostStart> {
        assert_eq!(invocation.slot.index(), 0);
        self.starts += 1;
        Ok(HostStart::Ready(value(Some(42))))
    }
    fn resume(
        &mut self,
        _: &mut EvalContext,
        _: &HostTaskId,
        _: HostArgReply<'_>,
    ) -> LocalResult<HostStep> {
        self.resumes += 1;
        Err(LocalError::HostContract("Ready host cannot resume".into()))
    }
    fn cancel(&mut self, _: &HostTaskId) {
        self.cancels += 1;
    }
}

#[test]
fn reported_host_refusal_follows_schema_selection_and_precedes_the_hook() {
    let catalog = HostCatalog::new(vec![HostSignature {
        arg_types: vec![].into_boxed_slice(),
        return_type: ft(),
    }])
    .unwrap();
    let expr = LocalExpr::HostCall {
        slot: catalog.slot(0).unwrap(),
        args: vec![].into_boxed_slice(),
        return_type: ft(),
    };
    let mut program =
        compile_local_with_hosts(&expr, &[], LocalCompileContext::default(), &catalog).unwrap();
    let mut services = ReadyHost {
        catalog,
        schema: vec![],
        hooks: 0,
        starts: 0,
        resumes: 0,
        cancels: 0,
    };
    services.schema.push(ft());
    let report = program
        .eval_with_bindings_reported(
            &mut LocalEvalState::default(),
            &mut EvalContext::default(),
            1,
            &[0],
            &mut services,
        )
        .unwrap_err();
    assert_unsited(&report, LocalFailureStage::Validation);
    assert!(matches!(report.error(), LocalError::InvalidBatch(_)));
    services.schema.clear();
    let report = program
        .eval_with_bindings_reported(
            &mut LocalEvalState::default(),
            &mut EvalContext::default(),
            0,
            &[0],
            &mut services,
        )
        .unwrap_err();
    assert_unsited(&report, LocalFailureStage::Validation);
    assert!(matches!(report.error(), LocalError::InvalidBatch(_)));
    for selection in [&[][..], &[0][..]] {
        let report = program
            .eval_with_bindings_reported(
                &mut LocalEvalState::default(),
                &mut EvalContext::default(),
                1,
                selection,
                &mut services,
            )
            .unwrap_err();
        assert_unsited(&report, LocalFailureStage::Validation);
        assert!(matches!(report.error(), LocalError::HostContract(_)));
        assert_eq!(report.sql_error_code(), None);
    }
    assert_eq!(
        (
            services.hooks,
            services.starts,
            services.resumes,
            services.cancels
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(
        program
            .eval_with_bindings(
                &mut LocalEvalState::default(),
                &mut EvalContext::default(),
                1,
                &[0],
                &mut services
            )
            .unwrap()
            .to_int_vec(),
        vec![Some(42)]
    );
    assert!(services.hooks > 0);
    assert_eq!(
        (services.starts, services.resumes, services.cancels),
        (1, 0, 0)
    );
}

#[test]
fn reported_deep_failure_and_owned_receipt_drop_remain_iterative() {
    thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn_wrapper(|| {
            for depth in [33, 64, 256] {
                let report = {
                    let mut expr = plus(input(0), constant(Some(1)));
                    for _ in 1..depth {
                        expr = plus(expr, constant(Some(0)));
                    }
                    let mut program = compile_with_limits(
                        &expr,
                        1,
                        CompileLimits {
                            max_nodes: 2 * depth + 1,
                            max_depth: depth + 1,
                        },
                    );
                    let mut services = Bindings::new(vec![vec![Some(i64::MAX)]]);
                    let report = run(&mut program, &mut services, 1, &[0]).unwrap_err();
                    assert_eq!(services.reads, vec![(0, row(0, 0))]);
                    report
                };
                assert_kernel(&report, depth - 1, row(0, 0));
                drop(report);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
