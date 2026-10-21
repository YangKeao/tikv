// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Protocol fixtures only; these byte relays are not SQL arithmetic kernels.
use std::collections::HashMap;

use tidb_query_datatype::{
    FieldTypeTp,
    codec::data_type::{ScalarValue, ScalarValueRef, VectorValue},
    expr::EvalContext,
};
use tipb::FieldType;

use super::*;

fn bytes_ft() -> FieldType {
    let mut field: FieldType = FieldTypeTp::VarString.into();
    field.set_charset("utf8mb4".into());
    field.set_collate(46);
    field
}
fn bytes(values: impl IntoIterator<Item = Option<Vec<u8>>>) -> VectorValue {
    let mut output = VectorValue::with_capacity(0, tidb_query_datatype::EvalType::Bytes);
    for value in values {
        output.push_bytes(value);
    }
    output
}
fn input(slot: usize) -> LocalExpr {
    LocalExpr::InputSlot {
        slot,
        field_type: bytes_ft(),
    }
}
fn host(catalog: &HostCatalog, slot: usize, args: Vec<LocalExpr>) -> LocalExpr {
    LocalExpr::HostCall {
        slot: catalog.slot(slot).unwrap(),
        args: args.into_boxed_slice(),
        return_type: bytes_ft(),
    }
}
fn request(index: usize, mode: ArgMode) -> HostArgRequest {
    HostArgRequest { index, mode }
}
fn sites(node: &LocalExpr, ordinal: &mut usize, output: &mut Vec<OrdinaryCallSite>) {
    let current = *ordinal;
    *ordinal += 1;
    if let LocalExpr::HostCall { args, .. } | LocalExpr::Call { args, .. } = node {
        output.push(OrdinaryCallSite::sql_native_batch(
            current,
            OrdinarySourceId::new(61, current as u64),
        ));
        for child in args {
            sites(child, ordinal, output);
        }
    }
}
fn compile(node: &LocalExpr, service: &Service) -> LocalBatchProgram {
    let mut source = Vec::new();
    sites(node, &mut 0, &mut source);
    let facts = BatchFacts::sql_native_batch_with_hosts(
        node,
        &service.schema,
        source,
        CompileLimits::default(),
        &service.catalog,
    )
    .unwrap();
    compile_batch(
        node,
        &service.schema,
        LocalCompileContext::default(),
        &facts,
    )
    .unwrap()
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Start(usize, Vec<usize>),
    RowStart(InputRow),
    Read(usize, InputRow),
    Resume(usize, usize, usize),
    Cancel(HostTaskId),
}
struct Task {
    slot: usize,
    occurrence_start: usize,
    rows: Vec<usize>,
    phase: usize,
}
struct Service {
    catalog: HostCatalog,
    schema: Vec<FieldType>,
    events: Vec<Event>,
    live: HashMap<HostTaskId, Task>,
    next: usize,
    requests: Vec<HostArgRequest>,
    read_failure: Option<(usize, usize)>,
    second_read_failure: Option<(usize, usize)>,
    primary_error: Option<usize>,
    fail_after_cached: bool,
    bad_request: bool,
    bad_token: bool,
    bad_result: bool,
    loop_reuse: bool,
    row_enabled: bool,
}
impl Service {
    fn new() -> Self {
        let signature = || HostSignature {
            arg_types: vec![bytes_ft(); 2].into_boxed_slice(),
            return_type: bytes_ft(),
        };
        Self {
            catalog: HostCatalog::for_sql_batch(vec![
                (SqlBatchHostOperation::AddWindowNative, signature()),
                (SqlBatchHostOperation::SubtractWindowNative, signature()),
                (SqlBatchHostOperation::MultiplyWindowNative, signature()),
            ])
            .unwrap(),
            schema: vec![bytes_ft(); 3],
            events: Vec::new(),
            live: HashMap::new(),
            next: 0,
            requests: vec![request(0, ArgMode::Fresh), request(1, ArgMode::Fresh)],
            read_failure: None,
            second_read_failure: None,
            primary_error: None,
            fail_after_cached: false,
            bad_request: false,
            bad_token: false,
            bad_result: false,
            loop_reuse: false,
            row_enabled: false,
        }
    }
    fn reads(&self) -> Vec<(usize, InputRow)> {
        self.events
            .iter()
            .filter_map(|event| {
                if let Event::Read(slot, row) = event {
                    Some((*slot, *row))
                } else {
                    None
                }
            })
            .collect()
    }
}
impl LocalRuntimeServices for Service {
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
        assert_eq!(expected, &bytes_ft());
        self.events.push(Event::Read(slot, row));
        ctx.warnings
            .append_warning(tidb_query_datatype::expr::Error::Eval(
                format!("read:{slot}:{}:{}", row.occurrence, row.input_row),
                1234,
            ));
        if self.read_failure == Some((slot, row.occurrence))
            || self.second_read_failure == Some((slot, row.occurrence))
        {
            let error = other_err!("window read primary");
            self.primary_error = Some(error.0.as_ref() as *const _ as usize);
            return Err(LocalError::Evaluation(error));
        }
        Ok(bytes([Some(
            format!("{slot}/{}:{}", row.occurrence, row.input_row).into_bytes(),
        )]))
    }
    fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
        Some(self)
    }
}
impl LocalHostServices for Service {
    fn catalog_key(&self) -> &HostCatalogKey {
        self.catalog.key()
    }
    fn start(
        &mut self,
        ctx: &mut EvalContext,
        invocation: HostInvocation<'_>,
    ) -> LocalResult<HostStart> {
        assert!(self.row_enabled, "window dispatch reached row start");
        self.events.push(Event::RowStart(invocation.row));
        self.start_window(
            ctx,
            HostWindowInvocation {
                slot: invocation.slot,
                occurrence_start: invocation.row.occurrence,
                physical_rows: &[invocation.row.input_row],
                arg_types: invocation.arg_types,
                return_type: invocation.return_type,
            },
        )
    }
    fn resume(
        &mut self,
        ctx: &mut EvalContext,
        id: &HostTaskId,
        reply: HostArgReply<'_>,
    ) -> LocalResult<HostStep> {
        assert!(self.row_enabled, "window dispatch reached row resume");
        let task = self.live.get(id).unwrap();
        let rows = task.rows.clone();
        let occurrence_start = task.occurrence_start;
        self.resume_window(
            ctx,
            id,
            HostWindowArgReply {
                index: reply.index,
                occurrence_start,
                field_type: reply.field_type,
                physical_rows: &rows,
                values: reply.values,
            },
        )
    }
    fn start_window(
        &mut self,
        _: &mut EvalContext,
        invocation: HostWindowInvocation<'_>,
    ) -> LocalResult<HostStart> {
        assert_eq!(
            invocation.arg_types,
            self.catalog.signature(invocation.slot)?.arg_types.as_ref()
        );
        assert_eq!(invocation.return_type, &bytes_ft());
        // Explicit bounded fixture ownership, separate from driver scratch.
        if (self.live.len() + 1)
            .checked_mul(invocation.physical_rows.len())
            .and_then(|n| n.checked_mul(std::mem::size_of::<usize>()))
            .is_none_or(|bytes| bytes > 1024 * 1024)
        {
            return Err(LocalError::ResourceLimit("fixture task bytes".into()));
        }
        self.events.push(Event::Start(
            invocation.slot.index(),
            invocation.physical_rows.to_vec(),
        ));
        let id = HostTaskId {
            slot: self.next,
            generation: if self.bad_token { 0 } else { 1 },
        };
        self.next += 1;
        self.live.insert(
            id,
            Task {
                slot: invocation.slot.index(),
                occurrence_start: invocation.occurrence_start,
                rows: invocation.physical_rows.to_vec(),
                phase: 0,
            },
        );
        Ok(HostStart::Pending {
            task: id,
            request: if self.bad_request {
                request(2, ArgMode::Fresh)
            } else {
                self.requests[0]
            },
        })
    }
    fn resume_window(
        &mut self,
        _: &mut EvalContext,
        id: &HostTaskId,
        reply: HostWindowArgReply<'_>,
    ) -> LocalResult<HostStep> {
        let task = self.live.get_mut(id).expect("live window task");
        assert_eq!(reply.physical_rows, task.rows);
        assert_eq!(reply.occurrence_start, task.occurrence_start);
        assert_eq!(reply.field_type, &bytes_ft());
        assert_eq!(reply.values.len(), task.rows.len());
        self.events
            .push(Event::Resume(task.slot, reply.index, reply.values.len()));
        task.phase += 1;
        if self.fail_after_cached && task.phase == 2 {
            self.read_failure = Some((0, 1));
        }
        if self.loop_reuse {
            return Ok(HostStep::NeedArg(request(0, ArgMode::Reuse)));
        }
        if task.phase < self.requests.len() {
            return Ok(HostStep::NeedArg(self.requests[task.phase]));
        }
        let result = if self.bad_result {
            bytes([Some(b"wrong width".to_vec())])
        } else {
            bytes((0..reply.values.len()).map(|row| {
                let ScalarValueRef::Bytes(value) = reply.values.get_scalar_ref(row) else {
                    panic!("carrier")
                };
                value.map(<[u8]>::to_vec)
            }))
        };
        self.live.remove(id);
        Ok(HostStep::Ready(result))
    }
    fn cancel(&mut self, id: &HostTaskId) {
        self.events.push(Event::Cancel(*id));
        self.live.remove(id);
    }
}
fn evaluate(
    program: &mut LocalBatchProgram,
    service: &mut Service,
    selection: &[usize],
    limits: ExecutionLimits,
) -> (LocalResult<VectorValue>, EvalContext) {
    let mut context = EvalContext::default();
    let result = program.eval_with_bindings(limits, &mut context, 3, selection, service);
    (result, context)
}
fn rows(slot: usize, selection: &[usize]) -> Vec<(usize, InputRow)> {
    selection
        .iter()
        .enumerate()
        .map(|(occurrence, &input_row)| {
            (
                slot,
                InputRow {
                    occurrence,
                    input_row,
                },
            )
        })
        .collect()
}

#[test]
fn window_host_nested_phases_keep_duplicate_occurrences_and_primary_error_cleanup() {
    let selection = [2, 0, 2];
    let mut service = Service::new();
    let root = host(
        &service.catalog,
        0,
        vec![
            host(&service.catalog, 1, vec![input(0), input(1)]),
            input(2),
        ],
    );
    let mut program = compile(&root, &service);
    assert!(program.requires_whole_selection());
    assert_eq!(program.max_batch_size(), usize::MAX);
    let (result, context) = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits::default(),
    );
    let result = result.unwrap();
    assert_eq!(
        service.reads(),
        [
            rows(0, &selection),
            rows(1, &selection),
            rows(2, &selection)
        ]
        .concat()
    );
    assert_eq!(context.warnings.warning_cnt, 9);
    assert!(service.live.is_empty());
    for row in 0..selection.len() {
        let ScalarValueRef::Bytes(Some(value)) = result.get_scalar_ref(row) else {
            panic!("output")
        };
        assert_eq!(value, format!("2/{row}:{}", selection[row]).as_bytes());
    }
    service.events.clear();
    service.read_failure = Some((1, 1));
    let (result, context) = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits::default(),
    );
    let Err(LocalError::Evaluation(error)) = result else {
        panic!("primary evaluation cause lost: {result:?}");
    };
    assert_eq!(
        Some(error.0.as_ref() as *const _ as usize),
        service.primary_error
    );
    assert_eq!(
        service.reads(),
        [rows(0, &selection), rows(1, &selection)[..2].to_vec()].concat()
    );
    assert!(service.live.is_empty());
    assert_eq!(
        context
            .warnings
            .warnings
            .iter()
            .map(|warning| warning.get_msg().to_owned())
            .collect::<Vec<_>>(),
        vec![
            "evaluation failed: read:0:0:2",
            "evaluation failed: read:0:1:0",
            "evaluation failed: read:0:2:2",
            "evaluation failed: read:1:0:2",
            "evaluation failed: read:1:1:0"
        ]
    );
    assert_eq!(
        service
            .events
            .iter()
            .filter(|event| matches!(event, Event::Cancel(_)))
            .count(),
        2
    );
    service.events.clear();
    service.read_failure = None;
    evaluate(
        &mut program,
        &mut service,
        &[1, 1],
        ExecutionLimits::default(),
    )
    .0
    .unwrap();
    assert_eq!(
        service.reads(),
        [rows(0, &[1, 1]), rows(1, &[1, 1]), rows(2, &[1, 1])].concat()
    );
}

#[test]
fn window_host_fresh_reuse_cache_is_vector_sized_and_invocation_local() {
    let selection = [1, 1, 0];
    let mut service = Service::new();
    service.requests = vec![
        request(0, ArgMode::Reuse),
        request(0, ArgMode::Reuse),
        request(0, ArgMode::Fresh),
        request(0, ArgMode::Reuse),
        request(1, ArgMode::Fresh),
    ];
    let root = host(&service.catalog, 2, vec![input(0), input(1)]);
    let mut program = compile(&root, &service);
    for _ in 0..2 {
        service.events.clear();
        evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits::default(),
        )
        .0
        .unwrap();
        assert_eq!(
            service.reads(),
            [
                rows(0, &selection),
                rows(0, &selection),
                rows(1, &selection)
            ]
            .concat()
        );
        assert!(service.live.is_empty());
    }
    service.events.clear();
    service.fail_after_cached = true;
    let result = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits::default(),
    )
    .0;
    let Err(LocalError::Evaluation(error)) = result else {
        panic!("fresh failure reused an old successful vector: {result:?}");
    };
    assert_eq!(
        Some(error.0.as_ref() as *const _ as usize),
        service.primary_error
    );
    assert_eq!(
        service.reads(),
        [rows(0, &selection), rows(0, &selection)[..2].to_vec()].concat()
    );
    assert!(service.live.is_empty());
    service.events.clear();
    evaluate(&mut program, &mut service, &[], ExecutionLimits::default())
        .0
        .unwrap();
    assert!(service.events.is_empty());
}

#[test]
fn window_host_protocol_failures_and_limits_cancel_without_retry() {
    let selection = [2, 0, 2];
    for fault in 0..3 {
        let mut service = Service::new();
        service.bad_request = fault == 0;
        service.bad_token = fault == 1;
        service.bad_result = fault == 2;
        let root = host(&service.catalog, 0, vec![input(0), input(1)]);
        let mut program = compile(&root, &service);
        let result = evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits::default(),
        )
        .0;
        assert!(
            matches!(result, Err(LocalError::HostContract(_))),
            "{result:?}"
        );
        assert!(service.live.is_empty());
        assert_eq!(
            service
                .events
                .iter()
                .filter(|event| matches!(event, Event::Cancel(_)))
                .count(),
            1
        );
    }
    let mut service = Service::new();
    service.loop_reuse = true;
    let root = host(&service.catalog, 0, vec![input(0), input(1)]);
    let mut program = compile(&root, &service);
    let result = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits {
            max_steps: 32,
            ..ExecutionLimits::default()
        },
    )
    .0;
    assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
    assert_eq!(service.reads(), rows(0, &selection));
    assert!(service.live.is_empty());
    service.events.clear();
    let result = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits {
            max_retained_bytes: 0,
            ..ExecutionLimits::default()
        },
    )
    .0;
    assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
    assert!(service.events.is_empty());
    let mut service = Service::new();
    let root = host(
        &service.catalog,
        0,
        vec![
            host(&service.catalog, 1, vec![input(0), input(1)]),
            input(2),
        ],
    );
    let mut program = compile(&root, &service);
    let result = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits {
            max_active_tasks: 1,
            ..ExecutionLimits::default()
        },
    )
    .0;
    assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
    assert!(service.reads().is_empty());
    assert!(service.live.is_empty());
}

#[test]
fn window_host_missing_callbacks_are_terminal_and_do_not_call_row_api() {
    struct Missing {
        catalog: HostCatalog,
        schema: Vec<FieldType>,
        row_calls: usize,
        reads: usize,
    }
    impl LocalRuntimeServices for Missing {
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
            self.reads += 1;
            Ok(bytes([None]))
        }
        fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
            Some(self)
        }
    }
    impl LocalHostServices for Missing {
        fn catalog_key(&self) -> &HostCatalogKey {
            self.catalog.key()
        }
        fn start(&mut self, _: &mut EvalContext, _: HostInvocation<'_>) -> LocalResult<HostStart> {
            self.row_calls += 1;
            Ok(HostStart::Ready(bytes([None])))
        }
        fn resume(
            &mut self,
            _: &mut EvalContext,
            _: &HostTaskId,
            _: HostArgReply<'_>,
        ) -> LocalResult<HostStep> {
            self.row_calls += 1;
            Ok(HostStep::Ready(bytes([None])))
        }
        fn cancel(&mut self, _: &HostTaskId) {}
    }
    let service = Service::new();
    let root = host(&service.catalog, 0, vec![input(0), input(1)]);
    let mut program = compile(&root, &service);
    let mut missing = Missing {
        catalog: service.catalog,
        schema: service.schema,
        row_calls: 0,
        reads: 0,
    };
    let result = program.eval_with_bindings(
        ExecutionLimits::default(),
        &mut EvalContext::default(),
        3,
        &[2, 0, 2],
        &mut missing,
    );
    assert!(matches!(result, Err(LocalError::HostContract(_))));
    assert_eq!((missing.row_calls, missing.reads), (0, 0));
}

#[test]
fn window_host_broadcasts_actual_nulls_and_keeps_row_root_admission() {
    let mut service = Service::new();
    service.requests = vec![request(0, ArgMode::Reuse), request(0, ArgMode::Fresh)];
    service.read_failure = Some((1, 0));
    let null = LocalExpr::Constant {
        value: ScalarValue::Bytes(None),
        field_type: bytes_ft(),
        literal_kind: LiteralKind::Typed,
    };
    let root = host(&service.catalog, 0, vec![null, input(1)]);
    let mut program = compile(&root, &service);
    let result = evaluate(
        &mut program,
        &mut service,
        &[2, 0, 2],
        ExecutionLimits::default(),
    )
    .0
    .unwrap();
    assert_eq!(result.len(), 3);
    for row in 0..3 {
        assert!(matches!(
            result.get_scalar_ref(row),
            ScalarValueRef::Bytes(None)
        ));
    }
    assert!(service.reads().is_empty());
    assert!(service.live.is_empty());
    let signature = || HostSignature {
        arg_types: vec![bytes_ft(); 2].into_boxed_slice(),
        return_type: bytes_ft(),
    };
    service.catalog = HostCatalog::for_sql_batch(vec![
        (SqlBatchHostOperation::AddWindowNative, signature()),
        (SqlBatchHostOperation::Concat, signature()),
    ])
    .unwrap();
    let root = host(&service.catalog, 1, vec![input(0), input(1)]);
    let mut program = compile(&root, &service);
    assert_eq!(program.max_batch_size(), 1);
    service.events.clear();
    let result = evaluate(
        &mut program,
        &mut service,
        &[2, 0, 2],
        ExecutionLimits::default(),
    )
    .0;
    assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
    assert!(service.events.is_empty());
}

fn row_in_window_service() -> Service {
    let mut service = Service::new();
    service.row_enabled = true;
    let signature = || HostSignature {
        arg_types: vec![bytes_ft(); 2].into_boxed_slice(),
        return_type: bytes_ft(),
    };
    service.catalog = HostCatalog::for_sql_batch(vec![
        (SqlBatchHostOperation::AddWindowNative, signature()),
        (SqlBatchHostOperation::Concat, signature()),
        (SqlBatchHostOperation::MultiplyWindowNative, signature()),
    ])
    .unwrap();
    service
}
#[test]
fn window_host_row_operand_keeps_full_parent_phase_and_original_occurrences() {
    let mut service = row_in_window_service();
    // Window -> row -> window. The inner window is scoped to the row's
    // ORIGINAL occurrence, not a new zero-based occurrence namespace.
    let root = host(
        &service.catalog,
        0,
        vec![
            host(
                &service.catalog,
                1,
                vec![
                    host(&service.catalog, 2, vec![input(0), input(1)]),
                    input(1),
                ],
            ),
            input(2),
        ],
    );
    let mut program = compile(&root, &service);
    assert!(program.requires_whole_selection());
    assert_eq!(program.max_batch_size(), usize::MAX);
    let selection: Vec<_> = (0..2050).map(|position| [2, 0, 2][position % 3]).collect();
    let result = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits::default(),
    )
    .0
    .unwrap();
    let mut expected = Vec::new();
    for (occurrence, &input_row) in selection.iter().enumerate() {
        for slot in [0, 1, 1] {
            expected.push((
                slot,
                InputRow {
                    occurrence,
                    input_row,
                },
            ));
        }
    }
    expected.extend(rows(2, &selection));
    assert_eq!(service.reads(), expected);
    assert_eq!(
        service
            .events
            .iter()
            .filter_map(|event| match event {
                Event::RowStart(row) => Some(*row),
                _ => None,
            })
            .collect::<Vec<_>>(),
        rows(0, &selection)
            .into_iter()
            .map(|(_, row)| row)
            .collect::<Vec<_>>()
    );
    for (row, &input_row) in selection.iter().enumerate() {
        let ScalarValueRef::Bytes(Some(value)) = result.get_scalar_ref(row) else {
            panic!("output")
        };
        assert_eq!(value, format!("2/{row}:{input_row}").as_bytes());
    }
    assert!(service.live.is_empty());
    service.events.clear();
    service.read_failure = Some((0, 1));
    let Err(LocalError::Evaluation(error)) = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits::default(),
    )
    .0
    else {
        panic!("lost original failure")
    };
    assert_eq!(
        Some(error.0.as_ref() as *const _ as usize),
        service.primary_error
    );
    assert_eq!(service.reads(), expected[..4]);
    assert!(service.live.is_empty());
    assert_eq!(
        service
            .events
            .iter()
            .filter(|event| matches!(event, Event::Cancel(_)))
            .count(),
        3
    );
    service.events.clear();
    service.read_failure = None;
    evaluate(
        &mut program,
        &mut service,
        &[1, 1],
        ExecutionLimits::default(),
    )
    .0
    .unwrap();
    assert_eq!(service.reads().len(), 8);
    assert!(service.live.is_empty());
}
#[test]
fn window_host_row_operand_fresh_restarts_each_row_and_limits_still_cancel() {
    let mut service = row_in_window_service();
    service.requests = vec![request(0, ArgMode::Reuse), request(0, ArgMode::Fresh)];
    let root = host(
        &service.catalog,
        0,
        vec![
            host(&service.catalog, 1, vec![input(0), input(1)]),
            input(2),
        ],
    );
    let mut program = compile(&root, &service);
    let selection = [2, 0, 2];
    evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits::default(),
    )
    .0
    .unwrap();
    let mut expected = Vec::new();
    for _ in 0..2 {
        for row in rows(0, &selection) {
            expected.extend([row, row]);
        }
    }
    assert_eq!(service.reads(), expected);
    assert!(service.live.is_empty());
    service.events.clear();
    let result = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits {
            max_active_tasks: 1,
            ..ExecutionLimits::default()
        },
    )
    .0;
    assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
    assert!(service.reads().is_empty());
    assert!(service.live.is_empty());
    service.events.clear();
    let result = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits {
            max_retained_bytes: 0,
            ..ExecutionLimits::default()
        },
    )
    .0;
    assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
    assert!(service.events.is_empty());
}

fn unary_bytes(signature: tipb::ScalarFuncSig, value: LocalExpr) -> LocalExpr {
    LocalExpr::Call {
        function: crate::FunctionRef::TiPb(signature),
        args: vec![value].into_boxed_slice(),
        return_type: bytes_ft(),
        metadata: crate::CallMetadata::None,
    }
}
#[test]
fn window_host_whole_selection_preserves_late_left_error_before_early_right() {
    let mut service = Service::new();
    let root = host(
        &service.catalog,
        0,
        vec![
            host(&service.catalog, 1, vec![input(0), input(1)]),
            input(2),
        ],
    );
    let mut program = compile(&root, &service);
    assert!(program.requires_whole_selection());
    assert_eq!(program.max_batch_size(), usize::MAX);
    let selection: Vec<_> = (0..2050).map(|position| [2, 0, 2][position % 3]).collect();
    service.read_failure = Some((0, 2049));
    service.second_read_failure = Some((2, 0));
    let (result, context) = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits::default(),
    );
    let Err(LocalError::Evaluation(error)) = result else {
        panic!("late left error lost: {result:?}");
    };
    assert_eq!(
        Some(error.0.as_ref() as *const _ as usize),
        service.primary_error
    );
    assert_eq!(service.reads(), rows(0, &selection));
    assert_eq!(context.warnings.warning_cnt, 2050);
    assert_eq!(
        service
            .events
            .iter()
            .filter(|event| matches!(event, Event::Cancel(_)))
            .count(),
        2
    );
    assert!(service.live.is_empty());
    service.events.clear();
    service.read_failure = None;
    service.second_read_failure = None;
    let result = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits::default(),
    )
    .0
    .unwrap();
    assert_eq!(result.len(), selection.len());
    assert_eq!(
        service.reads(),
        [
            rows(0, &selection),
            rows(1, &selection),
            rows(2, &selection)
        ]
        .concat()
    );
    for (position, &physical) in selection.iter().enumerate() {
        let ScalarValueRef::Bytes(Some(value)) = result.get_scalar_ref(position) else {
            panic!("output");
        };
        assert_eq!(value, format!("2/{position}:{physical}").as_bytes());
    }
    assert!(service.live.is_empty());
}
#[test]
fn window_host_whole_selection_runs_generated_kernels_and_scalar_broadcast_without_identity_slice()
{
    let selection: Vec<_> = (0..2050).map(|position| [2, 0, 2][position % 3]).collect();
    let mut service = Service::new();
    service.requests = vec![request(0, ArgMode::Fresh)];
    let root = host(
        &service.catalog,
        0,
        vec![
            unary_bytes(tipb::ScalarFuncSig::UpperUtf8, input(0)),
            input(1),
        ],
    );
    let mut program = compile(&root, &service);
    let output = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits::default(),
    )
    .0
    .unwrap();
    assert_eq!(service.reads(), rows(0, &selection));
    for (position, &physical) in selection.iter().enumerate() {
        let ScalarValueRef::Bytes(Some(value)) = output.get_scalar_ref(position) else {
            panic!("output");
        };
        assert_eq!(value, format!("0/{position}:{physical}").as_bytes());
    }
    assert!(service.live.is_empty());
    let scalar = LocalExpr::Constant {
        value: ScalarValue::Bytes(Some(b"MiXeD".to_vec())),
        field_type: bytes_ft(),
        literal_kind: LiteralKind::Typed,
    };
    let root = host(
        &service.catalog,
        0,
        vec![
            unary_bytes(tipb::ScalarFuncSig::UpperUtf8, scalar),
            input(1),
        ],
    );
    let mut program = compile(&root, &service);
    service.events.clear();
    let output = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits::default(),
    )
    .0
    .unwrap();
    assert!(service.reads().is_empty());
    for position in 0..selection.len() {
        assert!(
            matches!(output.get_scalar_ref(position), ScalarValueRef::Bytes(Some(value)) if value == b"MIXED")
        );
    }
    service.events.clear();
    let result = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits {
            max_steps: 100,
            ..ExecutionLimits::default()
        },
    )
    .0;
    assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
    assert!(service.reads().is_empty());
    assert!(service.live.is_empty());
    service.events.clear();
    let result = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits {
            max_retained_bytes: 0,
            ..ExecutionLimits::default()
        },
    )
    .0;
    assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
    assert!(service.events.is_empty());
}
#[test]
fn window_host_whole_selection_does_not_widen_a_legacy_ordinary_root() {
    let mut service = Service::new();
    let root = unary_bytes(tipb::ScalarFuncSig::UpperUtf8, input(0));
    let mut program = compile(&root, &service);
    assert!(!program.requires_whole_selection());
    assert_eq!(program.max_batch_size(), crate::types::BATCH_MAX_SIZE);
    let selection = vec![2; 2050];
    let result = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits::default(),
    )
    .0;
    assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
    assert!(service.events.is_empty());
    // Even a corrupted private facade cannot label an ordinary graph as a
    // whole-selection host root. Public callers cannot mutate these fields.
    program.whole_selection = true;
    program.max_batch_size = usize::MAX;
    let result = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits::default(),
    )
    .0;
    assert!(matches!(result, Err(LocalError::InvalidSpec(_))));
    assert!(service.events.is_empty());
}

#[test]
fn unary_window_descriptors_keep_full_fresh_reuse_and_terminal_cleanup() {
    for operation in [
        SqlBatchHostOperation::AbsIntWindowNative,
        SqlBatchHostOperation::CastDecimalWindowNative,
        SqlBatchHostOperation::CastJsonWindowNative,
    ] {
        let signature = HostSignature {
            arg_types: vec![bytes_ft()].into_boxed_slice(),
            return_type: bytes_ft(),
        };
        for arity in [0, 2, 3] {
            assert!(
                HostCatalog::for_sql_batch(vec![(
                    operation,
                    HostSignature {
                        arg_types: vec![bytes_ft(); arity].into_boxed_slice(),
                        return_type: bytes_ft()
                    }
                )])
                .is_err()
            );
        }
        let int: FieldType = FieldTypeTp::LongLong.into();
        assert!(
            HostCatalog::for_sql_batch(vec![(
                operation,
                HostSignature {
                    arg_types: vec![int].into_boxed_slice(),
                    return_type: bytes_ft()
                }
            )])
            .is_err()
        );
        let mut service = Service::new();
        service.catalog = HostCatalog::for_sql_batch(vec![(operation, signature)]).unwrap();
        service.requests = vec![
            request(0, ArgMode::Fresh),
            request(0, ArgMode::Reuse),
            request(0, ArgMode::Fresh),
        ];
        let root = host(&service.catalog, 0, vec![input(0)]);
        let mut program = compile(&root, &service);
        assert!(program.requires_whole_selection());
        let selection: Vec<_> = (0..2050).map(|index| [2, 0, 2][index % 3]).collect();
        let (result, context) = evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits::default(),
        );
        let result = result.unwrap();
        assert_eq!(result.len(), selection.len());
        assert_eq!(
            service.reads(),
            [rows(0, &selection), rows(0, &selection)].concat()
        );
        assert_eq!(context.warnings.warning_cnt, selection.len() * 2);
        assert!(service.live.is_empty());
        service.events.clear();
        service.read_failure = Some((0, 2049));
        let (result, _) = evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits::default(),
        );
        let Err(LocalError::Evaluation(error)) = result else {
            panic!("unary primary cause lost")
        };
        assert_eq!(
            Some(error.0.as_ref() as *const _ as usize),
            service.primary_error
        );
        assert_eq!(service.reads(), rows(0, &selection));
        assert!(service.live.is_empty());
        service.events.clear();
        service.read_failure = None;
        let (result, _) = evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits {
                max_retained_bytes: 0,
                ..ExecutionLimits::default()
            },
        );
        assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
        assert!(service.reads().is_empty());
        assert!(service.live.is_empty());
        service.events.clear();
        let (result, _) = evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits::default(),
        );
        assert!(result.is_ok());
        assert!(service.live.is_empty());
    }
}

fn control_call(sig: tipb::ScalarFuncSig, args: Vec<LocalExpr>, field: FieldType) -> LocalExpr {
    LocalExpr::Call {
        function: crate::FunctionRef::TiPb(sig),
        args: args.into_boxed_slice(),
        return_type: field,
        metadata: crate::CallMetadata::None,
    }
}
fn control_int(value: i64) -> LocalExpr {
    LocalExpr::Constant {
        value: ScalarValue::Int(Some(value)),
        field_type: FieldTypeTp::LongLong.into(),
        literal_kind: LiteralKind::Typed,
    }
}
fn window_control_operand(service: &Service, sig: tipb::ScalarFuncSig) -> LocalExpr {
    use tipb::ScalarFuncSig::*;
    let selected = host(&service.catalog, 1, vec![input(0), input(1)]);
    let null = || LocalExpr::Constant {
        value: ScalarValue::Bytes(None),
        field_type: bytes_ft(),
        literal_kind: LiteralKind::Typed,
    };
    let args = match sig {
        IfString => vec![
            control_call(
                LogicalOr,
                vec![control_int(0), control_int(1)],
                FieldTypeTp::LongLong.into(),
            ),
            selected,
            input(2),
        ],
        IfNullString => vec![null(), selected],
        CoalesceString => vec![null(), selected, input(2)],
        CaseWhenString => vec![control_int(0), input(2), control_int(1), selected, input(2)],
        _ => unreachable!(),
    };
    control_call(sig, args, bytes_ft())
}
#[test]
fn window_host_typed_controls_complete_each_row_before_next_parent_argument() {
    use tipb::ScalarFuncSig::*;
    for sig in [IfString, IfNullString, CoalesceString, CaseWhenString] {
        let mut service = Service::new();
        let root = host(
            &service.catalog,
            0,
            vec![window_control_operand(&service, sig), input(2)],
        );
        let mut program = compile(&root, &service);
        assert!(program.requires_whole_selection());
        assert_eq!(program.max_batch_size(), usize::MAX);
        let selection: Vec<_> = (0..2050).map(|row| [2, 0, 2][row % 3]).collect();
        let result = evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits::default(),
        )
        .0
        .unwrap();
        let mut expected = Vec::new();
        for (occurrence, &input_row) in selection.iter().enumerate() {
            for slot in [0, 1] {
                expected.push((
                    slot,
                    InputRow {
                        occurrence,
                        input_row,
                    },
                ));
            }
        }
        expected.extend(rows(2, &selection));
        assert_eq!(service.reads(), expected, "{sig:?}");
        assert_eq!(result.len(), selection.len());
        assert!(service.live.is_empty());
        // A typed control root still owns a row domain, not a window domain.
        let row_root = window_control_operand(&service, sig);
        assert_eq!(compile(&row_root, &service).max_batch_size(), 1);
    }
}
#[test]
fn window_host_typed_control_fresh_failure_and_resource_limits_are_terminal() {
    let mut service = Service::new();
    service.requests = vec![
        request(0, ArgMode::Reuse),
        request(0, ArgMode::Reuse),
        request(0, ArgMode::Fresh),
    ];
    let root = host(
        &service.catalog,
        0,
        vec![
            window_control_operand(&service, tipb::ScalarFuncSig::CoalesceString),
            input(2),
        ],
    );
    let mut program = compile(&root, &service);
    let selection = [2, 0, 2];
    evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits::default(),
    )
    .0
    .unwrap();
    // The nested window uses this same request schedule. Reuse skips reads;
    // Fresh restarts each complete control operand and each inner argument.
    let mut expected = Vec::new();
    for _ in 0..2 {
        for (occurrence, &input_row) in selection.iter().enumerate() {
            expected.extend(
                [(
                    0,
                    InputRow {
                        occurrence,
                        input_row,
                    },
                ); 2],
            );
        }
    }
    assert_eq!(service.reads(), expected);
    assert!(service.live.is_empty());
    service.events.clear();
    service.read_failure = Some((0, 1));
    let Err(LocalError::Evaluation(error)) = evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits::default(),
    )
    .0
    else {
        panic!("lost primary failure")
    };
    assert_eq!(
        Some(error.0.as_ref() as *const _ as usize),
        service.primary_error
    );
    assert_eq!(service.reads(), expected[..3]);
    assert!(service.live.is_empty());
    for limits in [
        ExecutionLimits {
            max_active_tasks: 1,
            ..ExecutionLimits::default()
        },
        ExecutionLimits {
            max_retained_bytes: 0,
            ..ExecutionLimits::default()
        },
        ExecutionLimits {
            max_frame_depth: 2,
            ..ExecutionLimits::default()
        },
        ExecutionLimits {
            max_steps: 8,
            ..ExecutionLimits::default()
        },
    ] {
        service.events.clear();
        service.read_failure = None;
        assert!(matches!(
            evaluate(&mut program, &mut service, &selection, limits).0,
            Err(LocalError::ResourceLimit(_))
        ));
        assert!(service.live.is_empty());
        assert!(service.reads().iter().all(|(slot, _)| *slot != 2));
    }
    service.events.clear();
    evaluate(
        &mut program,
        &mut service,
        &selection,
        ExecutionLimits::default(),
    )
    .0
    .unwrap();
    assert_eq!(service.reads(), expected);
}

#[test]
fn window_host_typed_control_scope_copies_fixed_and_spilled_carriers() {
    use tidb_query_datatype::codec::mysql::{Decimal, Duration, Time};
    use tipb::ScalarFuncSig::*;
    // The control's output is consumed by a genuine pure predicate, then used
    // as a substring index. Thus the complete typed vector crosses real
    // ordinary frames before returning to the Bytes window host.
    let values = [
        (
            CoalesceInt,
            IntIsNull,
            FieldTypeTp::LongLong,
            ScalarValue::Int(Some(4)),
        ),
        (
            CoalesceReal,
            RealIsNull,
            FieldTypeTp::Double,
            ScalarValue::Real(Some(
                tidb_query_datatype::codec::data_type::Real::new(4.0).unwrap(),
            )),
        ),
        (
            CoalesceDecimal,
            DecimalIsNull,
            FieldTypeTp::NewDecimal,
            ScalarValue::Decimal(Some(
                "12345678901234567890123456789012345678901234567890.1"
                    .parse::<Decimal>()
                    .unwrap(),
            )),
        ),
        (
            CoalesceTime,
            TimeIsNull,
            FieldTypeTp::DateTime,
            ScalarValue::DateTime(Some(
                Time::parse_datetime(&mut EvalContext::default(), "2020-01-02 03:04:05", 0, false)
                    .unwrap(),
            )),
        ),
        (
            CoalesceDuration,
            DurationIsNull,
            FieldTypeTp::Duration,
            ScalarValue::Duration(Some(Duration::from_nanos(4, 0).unwrap())),
        ),
    ];
    for (coalesce, is_null, tp, scalar) in values {
        let mut service = Service::new();
        let field: FieldType = tp.into();
        let null_scalar = match &scalar {
            ScalarValue::Int(_) => ScalarValue::Int(None),
            ScalarValue::Real(_) => ScalarValue::Real(None),
            ScalarValue::Decimal(_) => ScalarValue::Decimal(None),
            ScalarValue::DateTime(_) => ScalarValue::DateTime(None),
            ScalarValue::Duration(_) => ScalarValue::Duration(None),
            _ => unreachable!(),
        };
        let literal = |value| LocalExpr::Constant {
            value,
            field_type: field.clone(),
            literal_kind: LiteralKind::Typed,
        };
        let chosen = control_call(
            coalesce,
            vec![literal(null_scalar), literal(scalar)],
            field.clone(),
        );
        let index = control_call(is_null, vec![chosen], FieldTypeTp::LongLong.into());
        let operand = control_call(Substring2ArgsUtf8, vec![input(0), index], bytes_ft());
        let root = host(&service.catalog, 0, vec![operand, input(2)]);
        let mut program = compile(&root, &service);
        let selection = [2, 0, 2];
        evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits::default(),
        )
        .0
        .unwrap();
        let mut expected = rows(0, &selection);
        expected.extend(rows(2, &selection));
        assert_eq!(service.reads(), expected, "{coalesce:?}");
        assert!(service.live.is_empty());
    }
}

#[test]
fn window_raw_value_operands_keep_row_domain_inside_whole_parent() {
    use crate::impl_op::NativeBitwiseOp as B;
    for operation in [B::And, B::Or, B::Xor, B::LeftShift, B::RightShift]
        .into_iter()
        .map(SqlBatchHostOperation::BitwiseNative)
        .chain(std::iter::once(SqlBatchHostOperation::XorNative))
    {
        let mut service = row_in_window_service();
        let signature = || HostSignature {
            arg_types: vec![bytes_ft(); 2].into_boxed_slice(),
            return_type: bytes_ft(),
        };
        service.catalog = HostCatalog::for_sql_batch(vec![
            (SqlBatchHostOperation::AddWindowNative, signature()),
            (operation, signature()),
        ])
        .unwrap();
        let operand = host(&service.catalog, 1, vec![input(0), input(1)]);
        assert_eq!(compile(&operand, &service).max_batch_size(), 1);
        let root = host(&service.catalog, 0, vec![operand, input(2)]);
        let mut program = compile(&root, &service);
        assert!(program.requires_whole_selection());
        assert_eq!(program.max_batch_size(), usize::MAX);
        let selection: Vec<_> = (0..2050).map(|row| [2, 0, 2][row % 3]).collect();
        let (result, _) = evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits::default(),
        );
        assert_eq!(result.unwrap().len(), selection.len());
        let mut expected = Vec::new();
        for (occurrence, &input_row) in selection.iter().enumerate() {
            for slot in [0, 1] {
                expected.push((
                    slot,
                    InputRow {
                        occurrence,
                        input_row,
                    },
                ));
            }
        }
        expected.extend(rows(2, &selection));
        assert_eq!(service.reads(), expected);
        assert!(service.live.is_empty());
        service.events.clear();
        let (result, _) = evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits {
                max_retained_bytes: 0,
                ..ExecutionLimits::default()
            },
        );
        assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
        assert!(service.reads().is_empty());
        assert!(service.live.is_empty());
    }
}

#[test]
fn logical_xor_catalog_keeps_two_raw_handles_and_row_operation_identity() {
    let signature = |arity| HostSignature {
        arg_types: vec![bytes_ft(); arity].into_boxed_slice(),
        return_type: bytes_ft(),
    };
    for arity in [0, 1, 3] {
        assert!(
            HostCatalog::for_sql_batch(vec![(SqlBatchHostOperation::XorNative, signature(arity))])
                .is_err()
        );
    }
    let mut wrong = signature(2);
    wrong.arg_types[1] = FieldTypeTp::LongLong.into();
    assert!(HostCatalog::for_sql_batch(vec![(SqlBatchHostOperation::XorNative, wrong)]).is_err());
    let mut wrong = signature(2);
    wrong.return_type = FieldTypeTp::LongLong.into();
    assert!(HostCatalog::for_sql_batch(vec![(SqlBatchHostOperation::XorNative, wrong)]).is_err());
    let catalog =
        HostCatalog::for_sql_batch(vec![(SqlBatchHostOperation::XorNative, signature(2))]).unwrap();
    let operation = catalog
        .sql_batch_operation(catalog.slot(0).unwrap())
        .unwrap();
    assert_eq!(operation, SqlBatchHostOperation::XorNative);
    assert!(!operation.is_windowed());
}

#[test]
fn window_unary_sign_rows_keep_original_occurrences_across_parent_fresh_and_reuse() {
    use crate::impl_op::NativeUnarySign;
    for sign in [NativeUnarySign::Plus, NativeUnarySign::Minus] {
        let mut service = Service::new();
        service.row_enabled = true;
        service.requests = vec![
            request(0, ArgMode::Fresh),
            request(0, ArgMode::Reuse),
            request(0, ArgMode::Fresh),
        ];
        let signature = || HostSignature {
            arg_types: vec![bytes_ft()].into_boxed_slice(),
            return_type: bytes_ft(),
        };
        service.catalog = HostCatalog::for_sql_batch(vec![
            (SqlBatchHostOperation::CastJsonWindowNative, signature()),
            (SqlBatchHostOperation::UnarySignNative(sign), signature()),
        ])
        .unwrap();
        let child = host(&service.catalog, 1, vec![input(0)]);
        assert_eq!(compile(&child, &service).max_batch_size(), 1);
        let root = host(&service.catalog, 0, vec![child]);
        let mut program = compile(&root, &service);
        assert!(program.requires_whole_selection());
        let selection: Vec<_> = (0..2050).map(|row| [2, 0, 2][row % 3]).collect();
        let (result, _) = evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits::default(),
        );
        assert_eq!(result.unwrap().len(), selection.len());
        let mut expected = Vec::new();
        for _ in 0..2 {
            for (occurrence, &input_row) in selection.iter().enumerate() {
                for _ in 0..2 {
                    expected.push((
                        0,
                        InputRow {
                            occurrence,
                            input_row,
                        },
                    ));
                }
            }
        }
        assert_eq!(service.reads(), expected);
        assert!(service.live.is_empty());
        service.events.clear();
        let (result, _) = evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits {
                max_retained_bytes: 0,
                ..ExecutionLimits::default()
            },
        );
        assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
        assert!(service.reads().is_empty());
        assert!(service.live.is_empty());
    }
}

#[test]
fn unary_sign_catalog_rejects_extra_arguments_and_non_capability_carriers() {
    use crate::impl_op::NativeUnarySign;
    let signature = |arity| HostSignature {
        arg_types: vec![bytes_ft(); arity].into_boxed_slice(),
        return_type: bytes_ft(),
    };
    for sign in [NativeUnarySign::Plus, NativeUnarySign::Minus] {
        let op = SqlBatchHostOperation::UnarySignNative(sign);
        for arity in [0, 2, 3] {
            assert!(HostCatalog::for_sql_batch(vec![(op, signature(arity))]).is_err());
        }
        let mut wrong = signature(1);
        wrong.arg_types[0] = FieldTypeTp::LongLong.into();
        assert!(HostCatalog::for_sql_batch(vec![(op, wrong)]).is_err());
        let mut wrong = signature(1);
        wrong.return_type = FieldTypeTp::LongLong.into();
        assert!(HostCatalog::for_sql_batch(vec![(op, wrong)]).is_err());
        let catalog = HostCatalog::for_sql_batch(vec![(op, signature(1))]).unwrap();
        assert_eq!(
            catalog
                .sql_batch_operation(catalog.slot(0).unwrap())
                .unwrap(),
            op
        );
        assert!(!op.is_windowed());
    }
}

#[test]
fn quotient_window_and_row_descriptors_keep_occurrences_reuse_primary_errors_and_cleanup() {
    for (window, row) in [
        (
            SqlBatchHostOperation::ModuloWindowNative,
            SqlBatchHostOperation::ModuloNative,
        ),
        (
            SqlBatchHostOperation::DivideWindowNative,
            SqlBatchHostOperation::DivideNative,
        ),
        (
            SqlBatchHostOperation::IntegerDivideWindowNative,
            SqlBatchHostOperation::IntegerDivideNative,
        ),
    ] {
        let mut service = Service::new();
        service.row_enabled = true;
        service.requests = vec![
            request(0, ArgMode::Fresh),
            request(1, ArgMode::Fresh),
            request(0, ArgMode::Reuse),
            request(0, ArgMode::Fresh),
            request(1, ArgMode::Reuse),
        ];
        let signature = || HostSignature {
            arg_types: vec![bytes_ft(); 2].into_boxed_slice(),
            return_type: bytes_ft(),
        };
        service.catalog =
            HostCatalog::for_sql_batch(vec![(window, signature()), (row, signature())]).unwrap();
        let child = host(&service.catalog, 1, vec![input(0), input(1)]);
        assert_eq!(compile(&child, &service).max_batch_size(), 1);
        let root = host(&service.catalog, 0, vec![child, input(2)]);
        let mut program = compile(&root, &service);
        assert!(program.requires_whole_selection());
        assert_eq!(program.max_batch_size(), usize::MAX);
        let selection: Vec<_> = (0..2050).map(|position| [2, 0, 2][position % 3]).collect();
        let mut pass = Vec::new();
        for (occurrence, &input_row) in selection.iter().enumerate() {
            for slot in [0, 1, 0] {
                pass.push((
                    slot,
                    InputRow {
                        occurrence,
                        input_row,
                    },
                ));
            }
        }
        let expected: Vec<_> = pass
            .iter()
            .copied()
            .chain(rows(2, &selection))
            .chain(pass.iter().copied())
            .collect();
        let (result, _) = evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits::default(),
        );
        assert_eq!(result.unwrap().len(), selection.len());
        assert_eq!(service.reads(), expected);
        assert!(service.live.is_empty());
        service.events.clear();
        let (result, _) = evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits {
                max_retained_bytes: 0,
                ..ExecutionLimits::default()
            },
        );
        assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
        assert!(service.reads().is_empty());
        assert!(service.live.is_empty());
        service.events.clear();
        service.read_failure = Some((0, 17));
        let (result, _) = evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits::default(),
        );
        let Err(LocalError::Evaluation(error)) = result else {
            panic!("original input error missing")
        };
        assert_eq!(
            Some(error.0.as_ref() as *const _ as usize),
            service.primary_error
        );
        assert!(
            service
                .events
                .iter()
                .any(|event| matches!(event, Event::Cancel(_)))
        );
        assert!(service.live.is_empty());
        service.events.clear();
        service.read_failure = None;
        let (result, _) = evaluate(
            &mut program,
            &mut service,
            &selection,
            ExecutionLimits::default(),
        );
        assert_eq!(result.unwrap().len(), selection.len());
        assert_eq!(service.reads(), expected);
        assert!(service.live.is_empty());
        service.events.clear();
        let (result, _) = evaluate(&mut program, &mut service, &[], ExecutionLimits::default());
        assert_eq!(result.unwrap().len(), 0);
        assert!(service.events.is_empty());
    }
}

#[test]
fn quotient_catalog_keeps_six_distinct_binary_capability_protocols() {
    let operations = [
        SqlBatchHostOperation::ModuloNative,
        SqlBatchHostOperation::DivideNative,
        SqlBatchHostOperation::IntegerDivideNative,
        SqlBatchHostOperation::ModuloWindowNative,
        SqlBatchHostOperation::DivideWindowNative,
        SqlBatchHostOperation::IntegerDivideWindowNative,
    ];
    let signature = |arity| HostSignature {
        arg_types: vec![bytes_ft(); arity].into_boxed_slice(),
        return_type: bytes_ft(),
    };
    for (index, operation) in operations.into_iter().enumerate() {
        assert_eq!(operation.is_windowed(), index >= 3);
        for arity in [0, 1, 3] {
            assert!(HostCatalog::for_sql_batch(vec![(operation, signature(arity))]).is_err());
        }
        for wrong_return in [false, true] {
            let mut wrong = signature(2);
            if wrong_return {
                wrong.return_type = FieldTypeTp::LongLong.into();
            } else {
                wrong.arg_types[1] = FieldTypeTp::LongLong.into();
            }
            assert!(HostCatalog::for_sql_batch(vec![(operation, wrong)]).is_err());
        }
        let catalog = HostCatalog::for_sql_batch(
            operations
                .into_iter()
                .map(|operation| (operation, signature(2)))
                .collect(),
        )
        .unwrap();
        assert_eq!(
            catalog
                .sql_batch_operation(catalog.slot(index).unwrap())
                .unwrap(),
            operation
        );
        for other in operations.into_iter().take(index) {
            assert_ne!(operation, other);
        }
    }
}

// A protocol-only byte relay. No SQL expression/evaluator callback is stored.
struct RowDemandTask {
    rows: Vec<usize>,
    origin: usize,
    phase: usize,
    left: Vec<Option<Vec<u8>>>,
}
struct RowDemandService {
    catalog: HostCatalog,
    schema: Vec<FieldType>,
    live: HashMap<HostTaskId, RowDemandTask>,
    next: usize,
    mode: u8,
    reads: Vec<(usize, InputRow)>,
    replies: Vec<(usize, usize, Vec<usize>, Vec<Option<Vec<u8>>>)>,
    batches: Vec<usize>,
    cancelled: usize,
    primary: Option<usize>,
}
impl RowDemandService {
    fn new(mode: u8) -> Self {
        Self {
            catalog: HostCatalog::for_sql_batch(vec![(
                SqlBatchHostOperation::EagerValuesWindowNative,
                HostSignature {
                    arg_types: vec![bytes_ft(); 3].into_boxed_slice(),
                    return_type: bytes_ft(),
                },
            )])
            .unwrap(),
            schema: vec![bytes_ft(); 3],
            live: HashMap::new(),
            next: 0,
            mode,
            reads: vec![],
            replies: vec![],
            batches: vec![],
            cancelled: 0,
            primary: None,
        }
    }
    fn program(&self) -> LocalBatchProgram {
        let root = host(&self.catalog, 0, vec![input(0), input(1), input(2)]);
        let mut source = vec![];
        sites(&root, &mut 0, &mut source);
        let facts = BatchFacts::sql_native_batch_with_hosts(
            &root,
            &self.schema,
            source,
            CompileLimits::default(),
            &self.catalog,
        )
        .unwrap();
        compile_batch(&root, &self.schema, LocalCompileContext::default(), &facts).unwrap()
    }
    fn evaluate(
        &mut self,
        program: &mut LocalBatchProgram,
        selection: &[usize],
        limits: ExecutionLimits,
    ) -> LocalResult<VectorValue> {
        program.eval_with_bindings(limits, &mut EvalContext::default(), 3, selection, self)
    }
}
impl LocalRuntimeServices for RowDemandService {
    fn binding_schema(&self) -> &[FieldType] {
        &self.schema
    }
    fn read_input(
        &mut self,
        _ctx: &mut EvalContext,
        slot: usize,
        row: InputRow,
        expected: &FieldType,
    ) -> LocalResult<VectorValue> {
        assert_eq!(expected, &bytes_ft());
        self.reads.push((slot, row));
        if slot == 1 && self.mode == 3 {
            let error = other_err!("scoped primary input");
            self.primary = Some(error.0.as_ref() as *const _ as usize);
            return Err(LocalError::Evaluation(error));
        }
        if slot == 1 && self.mode == 4 {
            panic!("scoped input panic");
        }
        if slot == 1 && self.mode == 7 {
            return Ok(VectorValue::from_scalar(&ScalarValue::Int(Some(7)), 1));
        }
        Ok(bytes([Some(
            format!(
                "{slot}/{}:{}/{}",
                row.occurrence,
                row.input_row,
                self.reads.len()
            )
            .into_bytes(),
        )]))
    }
    fn read_input_batch(
        &mut self,
        _ctx: &mut EvalContext,
        slot: usize,
        _rows: &[usize],
        _expected: &FieldType,
    ) -> LocalResult<Option<VectorValue>> {
        self.batches.push(slot);
        Ok(None)
    }
    fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
        Some(self)
    }
}
impl LocalHostServices for RowDemandService {
    fn catalog_key(&self) -> &HostCatalogKey {
        self.catalog.key()
    }
    fn start(
        &mut self,
        ctx: &mut EvalContext,
        invocation: HostInvocation<'_>,
    ) -> LocalResult<HostStart> {
        assert_eq!(self.mode, 8, "window must use its genuine callback");
        self.start_window(
            ctx,
            HostWindowInvocation {
                slot: invocation.slot,
                occurrence_start: invocation.row.occurrence,
                physical_rows: &[invocation.row.input_row],
                arg_types: invocation.arg_types,
                return_type: invocation.return_type,
            },
        )
    }
    fn resume(
        &mut self,
        ctx: &mut EvalContext,
        id: &HostTaskId,
        reply: HostArgReply<'_>,
    ) -> LocalResult<HostStep> {
        assert_eq!(self.mode, 8, "window must use its genuine callback");
        let task = self.live.get(id).unwrap();
        let rows = task.rows.clone();
        let origin = task.origin;
        self.resume_window(
            ctx,
            id,
            HostWindowArgReply {
                index: reply.index,
                occurrence_start: origin,
                field_type: reply.field_type,
                physical_rows: &rows,
                values: reply.values,
            },
        )
    }
    fn start_window(
        &mut self,
        _ctx: &mut EvalContext,
        invocation: HostWindowInvocation<'_>,
    ) -> LocalResult<HostStart> {
        let id = HostTaskId {
            slot: self.next,
            generation: self.next as u64 + 1,
        };
        self.next += 1;
        self.live.insert(
            id,
            RowDemandTask {
                rows: invocation.physical_rows.to_vec(),
                origin: invocation.occurrence_start,
                phase: 0,
                left: vec![],
            },
        );
        Ok(HostStart::Pending {
            task: id,
            request: request(0, ArgMode::Fresh),
        })
    }
    fn resume_window(
        &mut self,
        _ctx: &mut EvalContext,
        id: &HostTaskId,
        reply: HostWindowArgReply<'_>,
    ) -> LocalResult<HostStep> {
        let task = self.live.get_mut(id).unwrap();
        assert_eq!(reply.field_type, &bytes_ft());
        let values = (0..reply.values.len())
            .map(|i| {
                let ScalarValueRef::Bytes(v) = reply.values.get_scalar_ref(i) else {
                    panic!("carrier changed")
                };
                v.map(<[u8]>::to_vec)
            })
            .collect::<Vec<_>>();
        self.replies.push((
            reply.index,
            reply.occurrence_start,
            reply.physical_rows.to_vec(),
            values.clone(),
        ));
        let phase = task.phase;
        task.phase += 1;
        if phase == 0 {
            assert_eq!(reply.index, 0);
            assert_eq!(reply.occurrence_start, task.origin);
            assert_eq!(reply.physical_rows, task.rows);
            task.left = values;
            if self.mode == 1 {
                return Ok(HostStep::NeedArgRow {
                    index: 3,
                    position: 0,
                });
            }
            if self.mode == 2 {
                return Ok(HostStep::NeedArgRow {
                    index: 1,
                    position: usize::MAX,
                });
            }
            if self.mode == 5 {
                return Ok(HostStep::Ready(bytes([Some(vec![0])])));
            }
            return Ok(HostStep::NeedArgRow {
                index: 1,
                position: task.rows.len() - 1,
            });
        }
        if self.mode == 6 {
            return Ok(HostStep::NeedArgRow {
                index: 1,
                position: 0,
            });
        }
        match phase {
            1..=3 => {
                let position = if phase == 2 { 0 } else { task.rows.len() - 1 };
                assert_eq!(reply.index, 1);
                assert_eq!(reply.occurrence_start, task.origin + position);
                assert_eq!(reply.physical_rows, [task.rows[position]]);
                assert_eq!(reply.values.len(), 1);
                Ok(match phase {
                    1 => HostStep::NeedArgRow {
                        index: 1,
                        position: 0,
                    },
                    2 => HostStep::NeedArgRow {
                        index: 1,
                        position: task.rows.len() - 1,
                    },
                    _ => HostStep::NeedArg(request(0, ArgMode::Reuse)),
                })
            }
            4 => {
                assert_eq!(values, task.left);
                assert_eq!(reply.physical_rows, task.rows);
                Ok(HostStep::NeedArg(request(1, ArgMode::Reuse)))
            }
            5 => {
                assert_eq!(reply.index, 1);
                assert_eq!(reply.occurrence_start, task.origin);
                assert_eq!(reply.physical_rows, task.rows);
                Ok(HostStep::NeedArg(request(2, ArgMode::Fresh)))
            }
            6 => {
                assert_eq!(reply.index, 2);
                assert_eq!(reply.physical_rows, task.rows);
                let result = bytes(task.left.clone());
                self.live.remove(id);
                Ok(HostStep::Ready(result))
            }
            _ => unreachable!(),
        }
    }
    fn cancel(&mut self, id: &HostTaskId) {
        self.cancelled += 1;
        self.live.remove(id);
    }
}
#[test]
fn window_scoped_row_demand_keeps_fresh_occurrences_and_whole_argument_caches_separate() {
    for selection in [vec![2, 0, 2], vec![0], (0..2050).map(|i| i % 3).collect()] {
        let mut service = RowDemandService::new(0);
        let mut program = service.program();
        assert!(program.requires_whole_selection());
        let output = service
            .evaluate(&mut program, &selection, ExecutionLimits::default())
            .unwrap();
        let last = selection.len() - 1;
        assert_eq!(
            service.reads,
            [
                rows(0, &selection),
                vec![
                    (
                        1,
                        InputRow {
                            occurrence: last,
                            input_row: selection[last]
                        }
                    ),
                    (
                        1,
                        InputRow {
                            occurrence: 0,
                            input_row: selection[0]
                        }
                    ),
                    (
                        1,
                        InputRow {
                            occurrence: last,
                            input_row: selection[last]
                        }
                    )
                ],
                rows(1, &selection),
                rows(2, &selection)
            ]
            .concat()
        );
        assert_ne!(
            service.replies[1].3, service.replies[3].3,
            "a repeated single row must be Fresh"
        );
        assert_eq!(
            service.replies[0].3, service.replies[4].3,
            "whole Reuse retains its own original vector"
        );
        assert_eq!(
            service.batches.iter().filter(|&&slot| slot == 1).count(),
            1,
            "only the later whole demand may use the batch hook"
        );
        assert_eq!(output.len(), selection.len());
        assert!(service.live.is_empty());
        assert_eq!(service.cancelled, 0);
        service.reads.clear();
        service.replies.clear();
        service.batches.clear();
        service
            .evaluate(&mut program, &selection, ExecutionLimits::default())
            .unwrap();
        assert!(service.live.is_empty());
    }
}
#[test]
fn window_scoped_row_rejects_bad_demand_and_preserves_failures_cleanup_and_reuse() {
    for mode in [1, 2, 3, 4, 5, 6, 7] {
        let mut service = RowDemandService::new(mode);
        let mut program = service.program();
        let limits = ExecutionLimits {
            max_steps: 100,
            ..ExecutionLimits::default()
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            service.evaluate(&mut program, &[2, 0, 2], limits)
        }));
        match mode {
            3 => {
                let Err(LocalError::Evaluation(error)) = outcome.unwrap() else {
                    panic!("primary cause changed")
                };
                assert_eq!(Some(error.0.as_ref() as *const _ as usize), service.primary);
            }
            4 => assert!(outcome.is_err()),
            6 => assert!(matches!(
                outcome.unwrap(),
                Err(LocalError::ResourceLimit(_))
            )),
            _ => assert!(outcome.unwrap().is_err()),
        }
        assert!(service.live.is_empty(), "mode={mode}");
        assert_eq!(service.cancelled, 1, "mode={mode}");
        assert!(service.reads.iter().all(|(slot, _)| *slot < 2));
        if matches!(mode, 1 | 2 | 5) {
            assert_eq!(service.reads, rows(0, &[2, 0, 2]));
        }
        service.mode = 0;
        service.reads.clear();
        service.replies.clear();
        service.batches.clear();
        service
            .evaluate(&mut program, &[2, 0, 2], ExecutionLimits::default())
            .unwrap();
        assert!(service.live.is_empty());
    }
}

#[test]
fn window_scoped_row_requires_a_window_catalog_and_valid_complete_signature() {
    let mut service = RowDemandService::new(8);
    service.catalog = HostCatalog::for_sql_batch(vec![(
        SqlBatchHostOperation::EagerValuesNative,
        HostSignature {
            arg_types: vec![bytes_ft(); 3].into_boxed_slice(),
            return_type: bytes_ft(),
        },
    )])
    .unwrap();
    let mut program = service.program();
    let result = service.evaluate(&mut program, &[2], ExecutionLimits::default());
    assert!(matches!(result, Err(LocalError::HostContract(_))));
    assert_eq!(service.reads, rows(0, &[2]));
    assert!(service.live.is_empty());
    assert_eq!(service.cancelled, 1);
    assert!(
        HostCatalog::for_sql_batch(vec![(
            SqlBatchHostOperation::EagerValuesWindowNative,
            HostSignature {
                arg_types: Box::new([]),
                return_type: bytes_ft()
            }
        )])
        .is_err()
    );
    let integer: FieldType = FieldTypeTp::LongLong.into();
    assert!(
        HostCatalog::for_sql_batch(vec![(
            SqlBatchHostOperation::EagerValuesWindowNative,
            HostSignature {
                arg_types: vec![integer].into_boxed_slice(),
                return_type: bytes_ft()
            }
        )])
        .is_err()
    );
}
