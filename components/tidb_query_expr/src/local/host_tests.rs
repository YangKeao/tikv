// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
    thread,
};

use tidb_query_datatype::{
    FieldTypeTp,
    codec::data_type::{ScalarValue, VectorValue},
    expr::{Error, EvalContext},
};
use tikv_util::sys::thread::StdThreadBuildWrapper;
use tipb::{FieldType, ScalarFuncSig};

use super::*;

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
fn plus(lhs: LocalExpr, rhs: LocalExpr) -> LocalExpr {
    LocalExpr::Call {
        function: FunctionRef::TiPb(ScalarFuncSig::PlusIntSignedSigned),
        args: vec![lhs, rhs].into_boxed_slice(),
        return_type: ft(),
        metadata: CallMetadata::None,
    }
}
fn host(catalog: &HostCatalog, index: usize, args: Vec<LocalExpr>) -> LocalExpr {
    LocalExpr::HostCall {
        slot: catalog.slot(index).unwrap(),
        args: args.into_boxed_slice(),
        return_type: ft(),
    }
}
fn request(index: usize, mode: ArgMode) -> HostArgRequest {
    HostArgRequest { index, mode }
}
fn warn(ctx: &mut EvalContext, message: String) {
    ctx.warnings.append_warning(Error::Eval(message, 1234));
}

// These are protocol fixtures, not implementations of the named SQL functions.
// Benchmark requests count once, then body Fresh N times; negative/NULL returns
// NULL. A sequence can model an AES-like host that never requests trailing
// args.
#[derive(Clone)]
enum Code {
    Ready(Option<i64>),
    Counter,
    Relay,
    Benchmark,
    Sequence(Vec<HostArgRequest>),
    StartError,
    ResumeError,
}
#[derive(Clone, Copy)]
enum ReadFault {
    Binding,
    Resource,
    Evaluation,
    Panic,
}
#[derive(Clone, Copy)]
enum TokenFault {
    ZeroGeneration,
    DuplicateLive,
}
#[derive(Clone, Copy)]
enum ResultFault {
    Type,
    Length,
}
#[derive(Debug, PartialEq, Eq)]
enum Event {
    Start(usize, InputRow),
    Read(usize, InputRow),
    Resume(HostTaskId, usize, Option<i64>),
    Cancel(HostTaskId),
}
struct Task {
    code: Code,
    replies: usize,
    remaining: usize,
}
struct Services {
    catalog: HostCatalog,
    definitions: Vec<(Code, usize)>,
    schema: Vec<FieldType>,
    inputs: Vec<Vec<Option<i64>>>,
    live: HashMap<HostTaskId, Task>,
    events: Vec<Event>,
    next_task: usize,
    effects: i64,
    fail_counter_at: Option<i64>,
    peak_tasks: usize,
    read_fault: Option<(usize, ReadFault)>,
    token_fault: Option<TokenFault>,
    result_fault: Option<ResultFault>,
    same_task_slot: bool,
    hide_after_start: bool,
    catalog_after_start: Option<HostCatalog>,
    host_enabled: bool,
    warnings: bool,
    partial_start: usize,
    self_cleaned_start: bool,
}
impl Services {
    fn new(definitions: Vec<(Code, usize)>, inputs: Vec<Vec<Option<i64>>>) -> Self {
        let catalog = HostCatalog::new(
            definitions
                .iter()
                .map(|(_, arity)| HostSignature {
                    arg_types: vec![ft(); *arity].into_boxed_slice(),
                    return_type: ft(),
                })
                .collect(),
        )
        .unwrap();
        Self {
            catalog,
            definitions,
            schema: vec![ft(); inputs.len()],
            inputs,
            live: HashMap::new(),
            events: Vec::new(),
            next_task: 0,
            effects: 0,
            fail_counter_at: None,
            peak_tasks: 0,
            read_fault: None,
            token_fault: None,
            result_fault: None,
            same_task_slot: false,
            hide_after_start: false,
            catalog_after_start: None,
            host_enabled: true,
            warnings: false,
            partial_start: 0,
            self_cleaned_start: false,
        }
    }
    fn compile(&self, expr: &LocalExpr) -> LocalProgram {
        compile_local_with_hosts(
            expr,
            &self.schema,
            LocalCompileContext::default(),
            &self.catalog,
        )
        .unwrap()
    }
    fn result(&self, result: Option<i64>) -> VectorValue {
        match self.result_fault {
            Some(ResultFault::Type) => {
                VectorValue::from_scalar(&ScalarValue::Bytes(Some(vec![0xff])), 1)
            }
            Some(ResultFault::Length) => VectorValue::from_scalar(&ScalarValue::Int(result), 2),
            None => value(result),
        }
    }
    fn starts(&self) -> Vec<usize> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::Start(slot, _) => Some(*slot),
                _ => None,
            })
            .collect()
    }
    fn reads(&self) -> Vec<(usize, InputRow)> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::Read(slot, row) => Some((*slot, *row)),
                _ => None,
            })
            .collect()
    }
    fn replies(&self) -> Vec<Option<i64>> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::Resume(_, _, value) => Some(*value),
                _ => None,
            })
            .collect()
    }
    fn canceled(&self) -> Vec<HostTaskId> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::Cancel(task) => Some(*task),
                _ => None,
            })
            .collect()
    }
}
impl LocalRuntimeServices for Services {
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
        self.events.push(Event::Read(slot, row));
        if self.warnings {
            warn(ctx, format!("read:{}", slot));
        }
        if let Some((fault_slot, fault)) = self.read_fault {
            if fault_slot == slot {
                return match fault {
                    ReadFault::Binding => {
                        Err(LocalError::BindingContract("read binding primary".into()))
                    }
                    ReadFault::Resource => {
                        Err(LocalError::ResourceLimit("read resource primary".into()))
                    }
                    ReadFault::Evaluation => Err(LocalError::Evaluation(other_err!(
                        "read evaluation primary"
                    ))),
                    ReadFault::Panic => panic!("read panic primary"),
                };
            }
        }
        let result = self
            .inputs
            .get(slot)
            .and_then(|column| column.get(row.input_row))
            .ok_or_else(|| LocalError::BindingContract("missing fixture input".into()))?;
        Ok(value(*result))
    }
    fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
        if self.host_enabled { Some(self) } else { None }
    }
}
impl LocalHostServices for Services {
    fn catalog_key(&self) -> &HostCatalogKey {
        self.catalog.key()
    }
    fn start(
        &mut self,
        ctx: &mut EvalContext,
        invocation: HostInvocation<'_>,
    ) -> LocalResult<HostStart> {
        let index = invocation.slot.index();
        let (code, arity) = self.definitions[index].clone();
        assert_eq!(invocation.arg_types, vec![ft(); arity].as_slice());
        assert_eq!(invocation.return_type, &ft());
        self.events.push(Event::Start(index, invocation.row));
        if self.warnings {
            warn(ctx, format!("start:{}", index));
        }
        match code {
            Code::Ready(result) => return Ok(HostStart::Ready(self.result(result))),
            Code::Counter => {
                self.effects += 1;
                if self.fail_counter_at == Some(self.effects) {
                    return Err(LocalError::Evaluation(other_err!("fresh counter primary")));
                }
                return Ok(HostStart::Ready(self.result(Some(self.effects))));
            }
            Code::StartError => {
                // No task ID reaches the driver: start owns cleanup of partial
                // state before returning an error.
                self.partial_start += 1;
                self.partial_start -= 1;
                self.self_cleaned_start = true;
                return Err(LocalError::Evaluation(other_err!("start primary")));
            }
            _ => {}
        }
        if matches!(self.token_fault, Some(TokenFault::DuplicateLive)) {
            if let Some(&task) = self.live.keys().next() {
                return Ok(HostStart::Pending {
                    task,
                    request: request(0, ArgMode::Fresh),
                });
            }
        }
        self.next_task += 1;
        let task = HostTaskId {
            slot: if self.same_task_slot {
                1
            } else {
                self.next_task
            },
            generation: if matches!(self.token_fault, Some(TokenFault::ZeroGeneration)) {
                0
            } else if self.same_task_slot {
                self.next_task as u64
            } else {
                1
            },
        };
        let first = match &code {
            Code::Sequence(requests) => requests[0],
            Code::Benchmark => request(0, ArgMode::Reuse),
            _ => request(0, ArgMode::Fresh),
        };
        self.live.insert(
            task,
            Task {
                code,
                replies: 0,
                remaining: 0,
            },
        );
        self.peak_tasks = self.peak_tasks.max(self.live.len());
        if self.hide_after_start {
            self.host_enabled = false;
        }
        if let Some(catalog) = self.catalog_after_start.take() {
            self.catalog = catalog;
        }
        Ok(HostStart::Pending {
            task,
            request: first,
        })
    }
    fn resume(
        &mut self,
        ctx: &mut EvalContext,
        task: &HostTaskId,
        reply: HostArgReply<'_>,
    ) -> LocalResult<HostStep> {
        assert_eq!(reply.field_type, &ft());
        let values = reply.values.to_int_vec();
        assert_eq!(values.len(), 1);
        let result = values[0];
        self.events.push(Event::Resume(*task, reply.index, result));
        if self.warnings {
            warn(ctx, "resume".into());
        }
        let state = self
            .live
            .get_mut(task)
            .expect("resume must identify a live task");
        state.replies += 1;
        let ready = match &state.code {
            Code::Relay => result,
            Code::ResumeError => return Err(LocalError::Evaluation(other_err!("resume primary"))),
            Code::Sequence(requests) => {
                if state.replies < requests.len() {
                    return Ok(HostStep::NeedArg(requests[state.replies]));
                }
                result
            }
            Code::Benchmark => {
                if state.replies == 1 {
                    if let Some(count) = result.filter(|&count| count >= 0) {
                        state.remaining = count as usize;
                        if state.remaining > 0 {
                            return Ok(HostStep::NeedArg(request(1, ArgMode::Fresh)));
                        }
                        Some(0)
                    } else {
                        None
                    }
                } else {
                    state.remaining -= 1;
                    if state.remaining > 0 {
                        return Ok(HostStep::NeedArg(request(1, ArgMode::Fresh)));
                    }
                    Some(0)
                }
            }
            _ => unreachable!("immediate fixture cannot suspend"),
        };
        // Ready is terminal from the provider's perspective. If validation of
        // this value fails, a subsequent cancel must remain harmless.
        self.live.remove(task);
        Ok(HostStep::Ready(self.result(ready)))
    }
    fn cancel(&mut self, task: &HostTaskId) {
        self.events.push(Event::Cancel(*task));
        self.live.remove(task);
    }
}
fn run(
    program: &mut LocalProgram,
    services: &mut Services,
    physical_rows: usize,
    selection: &[usize],
) -> LocalResult<VectorValue> {
    program.eval_with_bindings(
        &mut LocalEvalState::default(),
        &mut EvalContext::default(),
        physical_rows,
        selection,
        services,
    )
}
fn run_limited(
    program: &mut LocalProgram,
    services: &mut Services,
    limits: ExecutionLimits,
) -> LocalResult<VectorValue> {
    program.eval_with_bindings(
        &mut LocalEvalState::with_limits(limits),
        &mut EvalContext::default(),
        1,
        &[0],
        services,
    )
}

#[test]
fn host_compilation_requires_registration_and_checks_undemanded_children() {
    let services = Services::new(vec![(Code::Ready(Some(7)), 1)], vec![]);
    let expr = host(&services.catalog, 0, vec![constant(Some(9))]);
    assert!(matches!(
        compile_local(&expr, &[], LocalCompileContext::default()),
        Err(LocalError::InvalidSpec(_))
    ));
    let other = HostCatalog::new(vec![HostSignature {
        arg_types: vec![ft()].into_boxed_slice(),
        return_type: ft(),
    }])
    .unwrap();
    assert!(matches!(
        compile_local_with_hosts(&expr, &[], LocalCompileContext::default(), &other),
        Err(LocalError::InvalidSpec(_))
    ));
    assert!(matches!(
        compile_local_with_hosts(
            &expr,
            &[],
            LocalCompileContext {
                limits: CompileLimits {
                    max_nodes: 1,
                    max_depth: 1
                }
            },
            &services.catalog
        ),
        Err(LocalError::ResourceLimit(_))
    ));
    let invalid_child = LocalExpr::Constant {
        value: ScalarValue::Bytes(Some(vec![0xff])),
        field_type: ft(),
        literal_kind: LiteralKind::Typed,
    };
    let expr = host(&services.catalog, 0, vec![invalid_child]);
    assert!(matches!(
        compile_local_with_hosts(
            &expr,
            &[],
            LocalCompileContext::default(),
            &services.catalog
        ),
        Err(LocalError::InvalidSpec(_))
    ));
    assert!(services.events.is_empty());
}

#[test]
fn benchmark_count_controls_fresh_body_demand() {
    for (count, expected, demands) in [
        (Some(0), Some(0), 0),
        (None, None, 0),
        (Some(-1), None, 0),
        (Some(3), Some(0), 3),
    ] {
        let mut services = Services::new(vec![(Code::Benchmark, 2), (Code::Counter, 0)], vec![]);
        let expr = host(
            &services.catalog,
            0,
            vec![constant(count), host(&services.catalog, 1, vec![])],
        );
        let mut program = services.compile(&expr);
        assert_eq!(
            run(&mut program, &mut services, 1, &[0])
                .unwrap()
                .to_int_vec(),
            vec![expected]
        );
        assert_eq!(services.effects, demands);
        assert_eq!(
            services.starts().iter().filter(|&&slot| slot == 1).count(),
            demands as usize
        );
        assert!(services.live.is_empty());
    }
    // Fresh also repeats demanded binding effects, not only host invocations.
    let mut services = Services::new(vec![(Code::Benchmark, 2)], vec![vec![Some(9)]]);
    let mut program = services.compile(&host(
        &services.catalog,
        0,
        vec![constant(Some(3)), input(0)],
    ));
    services.warnings = true;
    let mut ctx = EvalContext::default();
    program
        .eval_with_bindings(
            &mut LocalEvalState::default(),
            &mut ctx,
            1,
            &[0],
            &mut services,
        )
        .unwrap();
    assert_eq!(services.reads().len(), 3);
    assert_eq!(
        ctx.warnings
            .warnings
            .iter()
            .filter(|warning| warning.get_msg().ends_with("read:0"))
            .count(),
        3
    );
}

#[test]
fn fresh_and_reuse_cache_only_this_invocation() {
    let requests = vec![
        request(0, ArgMode::Reuse),
        request(0, ArgMode::Reuse),
        request(0, ArgMode::Fresh),
        request(0, ArgMode::Reuse),
    ];
    let mut services = Services::new(
        vec![
            (Code::Sequence(requests), 1),
            (Code::Relay, 1),
            (Code::Counter, 0),
        ],
        vec![],
    );
    let expr = host(
        &services.catalog,
        0,
        vec![host(
            &services.catalog,
            1,
            vec![host(&services.catalog, 2, vec![])],
        )],
    );
    let mut program = services.compile(&expr);
    let mut state = LocalEvalState::default();
    let mut ctx = EvalContext::default();
    for (expected, replies) in [
        (2, vec![Some(1), Some(1), Some(2), Some(2)]),
        (4, vec![Some(3), Some(3), Some(4), Some(4)]),
    ] {
        services.events.clear();
        let outer = HostTaskId {
            slot: services.next_task + 1,
            generation: 1,
        };
        let output = program
            .eval_with_bindings(&mut state, &mut ctx, 1, &[0], &mut services)
            .unwrap();
        assert_eq!(output.to_int_vec(), vec![Some(expected)]);
        let outer_replies: Vec<_> = services
            .events
            .iter()
            .filter_map(|event| match event {
                Event::Resume(task, _, result) if *task == outer => Some(*result),
                _ => None,
            })
            .collect();
        assert_eq!(outer_replies, replies);
        assert_eq!(services.starts(), vec![0, 1, 2, 1, 2]);
        assert_eq!(services.peak_tasks, 2);
        assert!(services.live.is_empty());
    }
    assert_eq!(services.effects, 4);
}

#[test]
fn reuse_caches_successful_null_without_reimporting() {
    let code = Code::Sequence(vec![request(0, ArgMode::Reuse), request(0, ArgMode::Reuse)]);
    let mut services = Services::new(vec![(code, 1)], vec![vec![None]]);
    let mut program = services.compile(&host(&services.catalog, 0, vec![input(0)]));
    assert_eq!(
        run(&mut program, &mut services, 1, &[0])
            .unwrap()
            .to_int_vec(),
        vec![None]
    );
    assert_eq!(services.reads().len(), 1);
    assert_eq!(services.replies(), vec![None, None]);
    assert!(services.live.is_empty());
}

#[test]
fn failed_fresh_discards_cached_success_and_cancels_new_descendants() {
    let requests = vec![
        request(0, ArgMode::Reuse),
        request(0, ArgMode::Fresh),
        request(0, ArgMode::Reuse),
    ];
    let mut services = Services::new(
        vec![
            (Code::Sequence(requests), 1),
            (Code::Relay, 1),
            (Code::Counter, 0),
        ],
        vec![],
    );
    let expr = host(
        &services.catalog,
        0,
        vec![host(
            &services.catalog,
            1,
            vec![host(&services.catalog, 2, vec![])],
        )],
    );
    let mut program = services.compile(&expr);
    services.fail_counter_at = Some(2);
    let error = run(&mut program, &mut services, 1, &[0]).unwrap_err();
    assert!(matches!(error, LocalError::Evaluation(_)));
    assert!(error.to_string().contains("fresh counter primary"));
    assert_eq!(services.effects, 2);
    assert_eq!(services.starts(), vec![0, 1, 2, 1, 2]);
    assert_eq!(
        services.canceled(),
        vec![
            HostTaskId {
                slot: 3,
                generation: 1
            },
            HostTaskId {
                slot: 1,
                generation: 1
            }
        ]
    );
    assert!(services.live.is_empty());
}

#[test]
fn unused_trailing_arguments_and_host_ordinary_host_composition() {
    let mut services = Services::new(
        vec![(
            Code::Sequence(vec![request(0, ArgMode::Fresh), request(1, ArgMode::Fresh)]),
            4,
        )],
        vec![vec![Some(7)], vec![Some(9)], vec![], vec![]],
    );
    let expr = host(
        &services.catalog,
        0,
        vec![input(0), input(1), input(2), input(3)],
    );
    let mut program = services.compile(&expr);
    services.read_fault = Some((2, ReadFault::Panic));
    assert_eq!(
        run(&mut program, &mut services, 1, &[0])
            .unwrap()
            .to_int_vec(),
        vec![Some(9)]
    );
    assert_eq!(
        services
            .reads()
            .iter()
            .map(|&(slot, _)| slot)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );

    let mut services = Services::new(vec![(Code::Relay, 1), (Code::Counter, 0)], vec![]);
    let expr = host(
        &services.catalog,
        0,
        vec![plus(constant(Some(40)), host(&services.catalog, 1, vec![]))],
    );
    let mut program = services.compile(&expr);
    assert_eq!(
        run(&mut program, &mut services, 1, &[0])
            .unwrap()
            .to_int_vec(),
        vec![Some(41)]
    );
    assert_eq!(services.starts(), vec![0, 1]);
    assert!(services.live.is_empty());
}

#[test]
fn host_rows_preserve_empty_large_and_repeated_selections() {
    for (physical_rows, selection) in [
        (0, vec![]),
        (1025, (0..1025).rev().collect()),
        (3, vec![2, 0, 2]),
    ] {
        let values: Vec<_> = (0..physical_rows).map(|row| Some(row as i64)).collect();
        let mut services = Services::new(vec![(Code::Relay, 1)], vec![values]);
        let mut program = services.compile(&host(&services.catalog, 0, vec![input(0)]));
        let output = run(&mut program, &mut services, physical_rows, &selection).unwrap();
        assert_eq!(
            output.to_int_vec(),
            selection
                .iter()
                .map(|&row| Some(row as i64))
                .collect::<Vec<_>>()
        );
        let rows: Vec<_> = selection
            .iter()
            .enumerate()
            .map(|(occurrence, &input_row)| InputRow {
                occurrence,
                input_row,
            })
            .collect();
        assert_eq!(
            services.reads(),
            rows.iter().map(|&row| (0, row)).collect::<Vec<_>>()
        );
        let starts: Vec<_> = services
            .events
            .iter()
            .filter_map(|event| match event {
                Event::Start(_, row) => Some(*row),
                _ => None,
            })
            .collect();
        assert_eq!(starts, rows);
        assert!(services.live.is_empty());
        if selection.is_empty() {
            assert!(services.events.is_empty());
        }
    }
}

#[test]
fn host_capability_and_schema_fail_before_effects() {
    let mut services = Services::new(vec![(Code::Ready(Some(1)), 0)], vec![vec![Some(4)]]);
    let expr = host(&services.catalog, 0, vec![]);
    let mut program = services.compile(&expr);
    services.host_enabled = false;
    assert!(matches!(
        run(&mut program, &mut services, 1, &[0]),
        Err(LocalError::HostContract(_))
    ));
    services.host_enabled = true;
    let original = services.catalog.clone();
    services.catalog = HostCatalog::new(vec![HostSignature {
        arg_types: Box::new([]),
        return_type: ft(),
    }])
    .unwrap();
    for selection in [&[][..], &[0][..]] {
        assert!(matches!(
            run(&mut program, &mut services, 1, selection),
            Err(LocalError::HostContract(_))
        ));
    }
    services.catalog = original;
    services.schema[0].set_flen(7);
    assert!(matches!(
        run(&mut program, &mut services, 1, &[0]),
        Err(LocalError::InvalidBatch(_))
    ));
    services.schema[0] = ft();
    assert!(matches!(
        run(&mut program, &mut services, 0, &[0]),
        Err(LocalError::InvalidBatch(_))
    ));
    assert!(services.events.is_empty());
}

// Existing D1 adapters need only these two methods, without a host hook.
struct D1 {
    schema: Vec<FieldType>,
    reads: usize,
}
impl LocalRuntimeServices for D1 {
    fn binding_schema(&self) -> &[FieldType] {
        &self.schema
    }
    fn read_input(
        &mut self,
        _ctx: &mut EvalContext,
        _slot: usize,
        _row: InputRow,
        _expected: &FieldType,
    ) -> LocalResult<VectorValue> {
        self.reads += 1;
        Ok(value(Some(7)))
    }
}
struct NoHostHook(D1);
impl LocalRuntimeServices for NoHostHook {
    fn binding_schema(&self) -> &[FieldType] {
        self.0.binding_schema()
    }
    fn read_input(
        &mut self,
        ctx: &mut EvalContext,
        slot: usize,
        row: InputRow,
        expected: &FieldType,
    ) -> LocalResult<VectorValue> {
        self.0.read_input(ctx, slot, row, expected)
    }
    fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
        panic!("host-free program consulted host hook")
    }
}
#[test]
fn d1_compatibility_and_host_free_optional_hook_is_unused() {
    let mut services = D1 {
        schema: vec![ft()],
        reads: 0,
    };
    let mut program = compile_local(&input(0), &[ft()], LocalCompileContext::default()).unwrap();
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
        vec![Some(7)]
    );
    assert_eq!(services.reads, 1);
    let catalog = HostCatalog::new(vec![]).unwrap();
    let mut program =
        compile_local_with_hosts(&input(0), &[ft()], LocalCompileContext::default(), &catalog)
            .unwrap();
    let mut services = NoHostHook(services);
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
        vec![Some(7)]
    );
    assert_eq!(services.0.reads, 2);
}

#[test]
fn bad_request_and_zero_generation_cancel_the_known_task() {
    for zero_generation in [false, true] {
        let code = if zero_generation {
            Code::Relay
        } else {
            Code::Sequence(vec![request(1, ArgMode::Fresh)])
        };
        let mut services = Services::new(vec![(code, 1)], vec![vec![Some(8)]]);
        let mut program = services.compile(&host(&services.catalog, 0, vec![input(0)]));
        services.read_fault = Some((0, ReadFault::Panic));
        if zero_generation {
            services.token_fault = Some(TokenFault::ZeroGeneration);
        }
        assert!(matches!(
            run(&mut program, &mut services, 1, &[0]),
            Err(LocalError::HostContract(_))
        ));
        assert!(services.reads().is_empty());
        assert_eq!(
            services.canceled(),
            vec![HostTaskId {
                slot: 1,
                generation: if zero_generation { 0 } else { 1 }
            }]
        );
        assert!(services.live.is_empty());
    }
}

#[test]
fn same_task_slot_with_different_generations_remains_distinct() {
    for fail in [false, true] {
        let mut services = Services::new(vec![(Code::Relay, 1)], vec![vec![Some(9)]]);
        services.same_task_slot = true;
        if fail {
            services.read_fault = Some((0, ReadFault::Binding));
        }
        let expr = host(
            &services.catalog,
            0,
            vec![host(&services.catalog, 0, vec![input(0)])],
        );
        let mut program = services.compile(&expr);
        let result = run(&mut program, &mut services, 1, &[0]);
        if fail {
            assert!(matches!(result, Err(LocalError::BindingContract(_))));
            assert_eq!(
                services.canceled(),
                vec![
                    HostTaskId {
                        slot: 1,
                        generation: 2
                    },
                    HostTaskId {
                        slot: 1,
                        generation: 1
                    }
                ]
            );
        } else {
            assert_eq!(result.unwrap().to_int_vec(), vec![Some(9)]);
            assert!(services.canceled().is_empty());
        }
        assert_eq!(services.peak_tasks, 2);
        assert!(services.live.is_empty());
    }
}

#[test]
fn contract_breaking_provider_cannot_claim_unreachable_task_cleanup() {
    for hide in [false, true] {
        let mut services = Services::new(vec![(Code::Relay, 1)], vec![]);
        let original = services.catalog.clone();
        let expr = host(&services.catalog, 0, vec![constant(Some(9))]);
        let mut program = services.compile(&expr);
        if hide {
            services.hide_after_start = true;
        } else {
            services.catalog_after_start = Some(
                HostCatalog::new(vec![HostSignature {
                    arg_types: vec![ft()].into_boxed_slice(),
                    return_type: ft(),
                }])
                .unwrap(),
            );
        }
        let error = run(&mut program, &mut services, 1, &[0]).unwrap_err();
        assert!(matches!(error, LocalError::HostContract(_)));
        assert!(services.replies().is_empty());
        assert!(services.canceled().is_empty());
        // Deliberate provider violation: the driver cannot reach the original
        // namespace and must not send its token to an unrelated catalog.
        assert_eq!(services.live.len(), 1);
        services.catalog = original;
        services.host_enabled = true;
        let task = *services.live.keys().next().unwrap();
        services.cancel(&task); // Fixture owner performs the unreachable cleanup.
        assert!(services.live.is_empty());
    }
}

#[test]
fn duplicate_live_token_is_canceled_only_once() {
    let mut services = Services::new(vec![(Code::Relay, 1)], vec![]);
    let expr = host(
        &services.catalog,
        0,
        vec![host(&services.catalog, 0, vec![constant(Some(9))])],
    );
    let mut program = services.compile(&expr);
    services.token_fault = Some(TokenFault::DuplicateLive);
    assert!(matches!(
        run(&mut program, &mut services, 1, &[0]),
        Err(LocalError::HostContract(_))
    ));
    assert_eq!(services.starts(), vec![0, 0]);
    assert_eq!(
        services.canceled(),
        vec![HostTaskId {
            slot: 1,
            generation: 1
        }]
    );
    assert!(services.live.is_empty());
}

#[test]
fn malformed_ready_has_only_the_cleanup_ownership_it_established() {
    for result_fault in [ResultFault::Type, ResultFault::Length] {
        for resumed in [false, true] {
            let (code, args) = if resumed {
                (Code::Relay, vec![constant(Some(9))])
            } else {
                (Code::Ready(Some(9)), vec![])
            };
            let mut services = Services::new(vec![(code, args.len())], vec![]);
            let mut program = services.compile(&host(&services.catalog, 0, args));
            services.result_fault = Some(result_fault);
            assert!(matches!(
                run(&mut program, &mut services, 1, &[0]),
                Err(LocalError::HostContract(_))
            ));
            assert_eq!(services.canceled().len(), usize::from(resumed));
            assert!(services.live.is_empty());
            // In the resumed case Ready already removed the fixture task;
            // cancellation is therefore explicitly exercised as idempotent.
            if resumed {
                assert_eq!(services.replies(), vec![Some(9)]);
            }
        }
    }
}

#[test]
fn child_errors_keep_primary_variant_warning_prefix_and_inner_first_cleanup() {
    for fault in [
        ReadFault::Binding,
        ReadFault::Resource,
        ReadFault::Evaluation,
    ] {
        let mut services = Services::new(vec![(Code::Relay, 1)], vec![vec![Some(8)]]);
        let expr = host(
            &services.catalog,
            0,
            vec![host(&services.catalog, 0, vec![input(0)])],
        );
        let mut program = services.compile(&expr);
        services.read_fault = Some((0, fault));
        services.warnings = true;
        let mut ctx = EvalContext::default();
        warn(&mut ctx, "prior".into());
        let error = program
            .eval_with_bindings(
                &mut LocalEvalState::default(),
                &mut ctx,
                1,
                &[0],
                &mut services,
            )
            .unwrap_err();
        match fault {
            ReadFault::Binding => assert!(matches!(error, LocalError::BindingContract(_))),
            ReadFault::Resource => assert!(matches!(error, LocalError::ResourceLimit(_))),
            ReadFault::Evaluation => assert!(matches!(error, LocalError::Evaluation(_))),
            ReadFault::Panic => unreachable!(),
        }
        assert!(error.to_string().contains("primary"));
        assert_eq!(ctx.warnings.warning_cnt, 4);
        for (warning, suffix) in ctx
            .warnings
            .warnings
            .iter()
            .zip(["prior", "start:0", "start:0", "read:0"])
        {
            assert!(warning.get_msg().ends_with(suffix));
        }
        assert_eq!(
            services.canceled(),
            vec![
                HostTaskId {
                    slot: 2,
                    generation: 1
                },
                HostTaskId {
                    slot: 1,
                    generation: 1
                }
            ]
        );
        assert!(services.live.is_empty());
    }
}

#[test]
fn kernel_error_precedes_later_rows_and_resume_error_keeps_its_primary() {
    let mut services = Services::new(vec![(Code::Relay, 1)], vec![vec![Some(i64::MAX), Some(1)]]);
    let expr = host(
        &services.catalog,
        0,
        vec![plus(input(0), constant(Some(1)))],
    );
    let mut program = services.compile(&expr);
    services.warnings = true;
    let mut ctx = EvalContext::default();
    warn(&mut ctx, "prior".into());
    let error = program
        .eval_with_bindings(
            &mut LocalEvalState::default(),
            &mut ctx,
            2,
            &[0, 1],
            &mut services,
        )
        .unwrap_err();
    assert!(matches!(error, LocalError::Evaluation(_)));
    assert_eq!(
        services.reads(),
        vec![(
            0,
            InputRow {
                occurrence: 0,
                input_row: 0
            }
        )]
    );
    assert_eq!(services.starts(), vec![0]);
    assert_eq!(services.canceled().len(), 1);
    assert_eq!(ctx.warnings.warning_cnt, 3);
    for (warning, suffix) in ctx
        .warnings
        .warnings
        .iter()
        .zip(["prior", "start:0", "read:0"])
    {
        assert!(warning.get_msg().ends_with(suffix));
    }
    assert!(services.live.is_empty());

    let mut services = Services::new(vec![(Code::ResumeError, 1)], vec![]);
    let mut program = services.compile(&host(&services.catalog, 0, vec![constant(Some(4))]));
    let error = run(&mut program, &mut services, 1, &[0]).unwrap_err();
    assert!(matches!(error, LocalError::Evaluation(_)));
    assert!(error.to_string().contains("resume primary"));
    assert_eq!(services.canceled().len(), 1);
    assert!(services.live.is_empty());
}

#[test]
fn failed_start_cleans_its_own_unpublished_state() {
    let mut services = Services::new(vec![(Code::StartError, 0)], vec![]);
    let mut program = services.compile(&host(&services.catalog, 0, vec![]));
    let error = run(&mut program, &mut services, 1, &[0]).unwrap_err();
    assert!(matches!(error, LocalError::Evaluation(_)));
    assert!(error.to_string().contains("start primary"));
    assert!(services.self_cleaned_start);
    assert_eq!(services.partial_start, 0);
    assert!(services.canceled().is_empty());
    assert!(services.live.is_empty());
}

#[test]
fn healthy_provider_cancels_known_tasks_during_input_unwind() {
    let mut services = Services::new(vec![(Code::Relay, 1)], vec![vec![Some(9)]]);
    let expr = host(
        &services.catalog,
        0,
        vec![host(&services.catalog, 0, vec![input(0)])],
    );
    let mut program = services.compile(&expr);
    services.read_fault = Some((0, ReadFault::Panic));
    let result = catch_unwind(AssertUnwindSafe(|| {
        run(&mut program, &mut services, 1, &[0])
    }));
    let panic = result.unwrap_err();
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
    assert_eq!(message, Some("read panic primary"));
    assert_eq!(
        services.canceled(),
        vec![
            HostTaskId {
                slot: 2,
                generation: 1
            },
            HostTaskId {
                slot: 1,
                generation: 1
            }
        ]
    );
    assert!(services.live.is_empty());
    services.read_fault = None;
    assert_eq!(
        run(&mut program, &mut services, 1, &[0])
            .unwrap()
            .to_int_vec(),
        vec![Some(9)]
    );
    assert!(services.live.is_empty());
}

#[test]
fn task_frame_and_initial_work_limits_precede_the_next_effect() {
    let mut services = Services::new(vec![(Code::Ready(Some(7)), 0)], vec![]);
    let mut program = services.compile(&host(&services.catalog, 0, vec![]));
    let limits = ExecutionLimits {
        max_active_tasks: 0,
        ..ExecutionLimits::default()
    };
    assert!(matches!(
        run_limited(&mut program, &mut services, limits),
        Err(LocalError::ResourceLimit(_))
    ));
    assert!(
        services.events.is_empty(),
        "even immediate Ready must reserve a potential task before start"
    );

    for limits in [
        ExecutionLimits {
            max_frame_depth: 2,
            ..ExecutionLimits::default()
        },
        ExecutionLimits {
            max_steps: 2,
            ..ExecutionLimits::default()
        },
    ] {
        let mut services = Services::new(vec![(Code::Relay, 1)], vec![vec![Some(9)]]);
        let mut program = services.compile(&host(&services.catalog, 0, vec![input(0)]));
        services.read_fault = Some((0, ReadFault::Panic));
        assert!(matches!(
            run_limited(&mut program, &mut services, limits),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(services.starts(), vec![0]);
        assert!(services.reads().is_empty());
        assert!(services.replies().is_empty());
        assert_eq!(
            services.canceled(),
            vec![HostTaskId {
                slot: 1,
                generation: 1
            }]
        );
        assert!(services.live.is_empty());
    }
}

#[test]
fn work_budget_meters_resume_and_cached_reuse_requests() {
    // Root node + start + Fresh request + constant + accept + resume + Reuse
    // request + resume = 8 steps. Reuse bypasses neither protocol charge.
    for (max_steps, replies, success) in [(5, 0, false), (6, 1, false), (7, 1, false), (8, 2, true)]
    {
        let code = Code::Sequence(vec![request(0, ArgMode::Fresh), request(0, ArgMode::Reuse)]);
        let mut services = Services::new(vec![(code, 1)], vec![]);
        let mut program = services.compile(&host(&services.catalog, 0, vec![constant(Some(9))]));
        let limits = ExecutionLimits {
            max_steps,
            ..ExecutionLimits::default()
        };
        let result = run_limited(&mut program, &mut services, limits);
        if success {
            assert_eq!(result.unwrap().to_int_vec(), vec![Some(9)]);
        } else {
            assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
        }
        assert_eq!(services.replies().len(), replies);
        assert!(services.live.is_empty());
        if !success {
            assert_eq!(services.canceled().len(), 1);
        }
    }
}

#[test]
fn deep_host_relays_use_heap_frames_and_release_all_tasks() {
    thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn_wrapper(|| {
            for depth in [33, 64, 256] {
                let mut services = Services::new(vec![(Code::Relay, 1)], vec![]);
                let mut expr = constant(Some(11));
                for _ in 0..depth {
                    expr = host(&services.catalog, 0, vec![expr]);
                }
                let cx = LocalCompileContext {
                    limits: CompileLimits {
                        max_nodes: depth + 1,
                        max_depth: depth + 1,
                    },
                };
                let mut program =
                    compile_local_with_hosts(&expr, &[], cx, &services.catalog).unwrap();
                assert_eq!(
                    run(&mut program, &mut services, 1, &[0])
                        .unwrap()
                        .to_int_vec(),
                    vec![Some(11)]
                );
                assert_eq!(services.peak_tasks, depth);
                assert_eq!(services.starts().len(), depth);
                assert!(services.live.is_empty());
                drop(program);
                drop(expr);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
