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
    FieldTypeTp::VarString.into()
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
    if let LocalExpr::HostCall { args, .. } = node {
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
        if self.read_failure == Some((slot, row.occurrence)) {
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
        assert_eq!(invocation.arg_types, &[bytes_ft(), bytes_ft()]);
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
    assert_eq!(program.max_batch_size(), crate::types::BATCH_MAX_SIZE);
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
    assert_eq!(program.max_batch_size(), crate::types::BATCH_MAX_SIZE);
    let selection = [2, 0, 2];
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
