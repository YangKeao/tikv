// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Owned child-result continuation conformance, using the existing host
//! fixtures.
use super::*;

struct ResultServices {
    base: Services,
    failures: Vec<HostArgFailure>,
    failure_indices: Vec<usize>,
    rethrow: bool,
    reads: usize,
    inject_at: Option<usize>,
    injected: Option<LocalError>,
    cancelled_before_reply: Vec<HostTaskId>,
}
impl ResultServices {
    fn new(base: Services) -> Self {
        Self {
            base,
            failures: vec![],
            failure_indices: vec![],
            rethrow: false,
            reads: 0,
            inject_at: None,
            injected: None,
            cancelled_before_reply: vec![],
        }
    }
    fn run(
        &mut self,
        program: &mut LocalProgram,
        limits: ExecutionLimits,
    ) -> LocalResult<VectorValue> {
        program.eval_with_bindings(limits, &mut EvalContext::default(), 1, &[0], self)
    }
}
impl LocalRuntimeServices for ResultServices {
    fn binding_schema(&self) -> &[FieldType] {
        self.base.binding_schema()
    }
    fn read_input(
        &mut self,
        ctx: &mut EvalContext,
        slot: usize,
        row: InputRow,
        expected: &FieldType,
    ) -> LocalResult<VectorValue> {
        self.reads += 1;
        if self.inject_at == Some(self.reads) {
            return Err(self.injected.take().unwrap());
        }
        self.base.read_input(ctx, slot, row, expected)
    }
    fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
        if self.base.host_enabled {
            Some(self)
        } else {
            None
        }
    }
}
impl LocalHostServices for ResultServices {
    fn catalog_key(&self) -> &HostCatalogKey {
        self.base.catalog_key()
    }
    fn start(&mut self, ctx: &mut EvalContext, call: HostInvocation<'_>) -> LocalResult<HostStart> {
        self.base.start(ctx, call)
    }
    fn resume(
        &mut self,
        ctx: &mut EvalContext,
        task: &HostTaskId,
        reply: HostArgReply<'_>,
    ) -> LocalResult<HostStep> {
        let step = self.base.resume(ctx, task, reply)?;
        if matches!(step, HostStep::Ready(_)) && self.rethrow && !self.failures.is_empty() {
            return Err(self.failures.remove(0).into_error());
        }
        Ok(step)
    }
    fn resume_error(
        &mut self,
        _: &mut EvalContext,
        task: &HostTaskId,
        failure: HostArgFailure,
    ) -> LocalResult<HostStep> {
        self.failure_indices.push(failure.index());
        self.cancelled_before_reply.extend(self.base.canceled());
        self.failures.push(failure);
        let state = self
            .base
            .live
            .get_mut(task)
            .expect("requesting task remains live");
        state.replies += 1;
        let Code::Sequence(requests) = &state.code else {
            panic!("explicit result fixture")
        };
        if state.replies < requests.len() {
            return Ok(HostStep::NeedArg(requests[state.replies]));
        }
        Err(self.failures.remove(0).into_error())
    }
    fn cancel(&mut self, task: &HostTaskId) {
        self.base.cancel(task)
    }
}

#[test]
fn host_result_keeps_original_cause_and_continues_rhs_after_nested_cleanup() {
    let base = Services::new(
        vec![
            (
                Code::Sequence(vec![
                    request(0, ArgMode::FreshResult),
                    request(1, ArgMode::Fresh),
                ]),
                2,
            ),
            (Code::Relay, 1),
            (Code::Counter, 0),
        ],
        vec![vec![Some(9)]],
    );
    let graph = host(
        &base.catalog,
        0,
        vec![
            host(&base.catalog, 1, vec![input(0)]),
            host(&base.catalog, 2, vec![]),
        ],
    );
    let mut program = base.compile(&graph);
    let mut services = ResultServices::new(base);
    services.rethrow = true;
    let error = other_err!("one actual owned provider cause");
    let address = error.0.as_ref() as *const _ as usize;
    services.injected = Some(LocalError::Evaluation(error));
    services.inject_at = Some(1);
    let failure = services
        .run(&mut program, ExecutionLimits::default())
        .unwrap_err();
    let LocalError::Evaluation(error) = failure else {
        panic!("original error class")
    };
    assert_eq!(
        error.0.as_ref() as *const _ as usize,
        address,
        "move exact boxed cause without reconstruction"
    );
    assert_eq!(services.failure_indices, vec![0]);
    assert_eq!(
        services.base.effects, 1,
        "RHS runs once after actual left failure"
    );
    assert_eq!(
        services.cancelled_before_reply.len(),
        1,
        "inner relay cancelled before result callback"
    );
    assert_eq!(
        services.base.canceled().len(),
        2,
        "inner and root cleanup exactly once"
    );
    assert!(services.base.live.is_empty());
    services.rethrow = false;
    services.inject_at = None;
    assert_eq!(
        services
            .run(&mut program, ExecutionLimits::default())
            .unwrap()
            .to_int_vec(),
        vec![Some(2)]
    );
    assert!(services.base.live.is_empty());
}
#[test]
fn host_result_default_modes_and_default_error_handler_still_abort() {
    for mode in [ArgMode::Fresh, ArgMode::Reuse, ArgMode::FreshResult] {
        let mut services = Services::new(
            vec![
                (
                    Code::Sequence(vec![request(0, mode), request(1, ArgMode::Fresh)]),
                    2,
                ),
                (Code::StartError, 0),
                (Code::Counter, 0),
            ],
            vec![],
        );
        let graph = host(
            &services.catalog,
            0,
            vec![
                host(&services.catalog, 1, vec![]),
                host(&services.catalog, 2, vec![]),
            ],
        );
        let mut program = services.compile(&graph);
        assert!(matches!(
            run(&mut program, &mut services, 1, &[0]),
            Err(LocalError::Evaluation(_))
        ));
        assert_eq!(services.effects, 0);
        assert!(services.live.is_empty());
        assert_eq!(services.canceled().len(), 1);
    }
}
#[test]
fn host_result_failure_clears_success_cache_without_replay() {
    let base = Services::new(
        vec![(
            Code::Sequence(vec![
                request(0, ArgMode::Reuse),
                request(0, ArgMode::FreshResult),
                request(0, ArgMode::Reuse),
            ]),
            1,
        )],
        vec![vec![Some(17)]],
    );
    let mut program = base.compile(&host(&base.catalog, 0, vec![input(0)]));
    let mut services = ResultServices::new(base);
    services.inject_at = Some(2);
    services.injected = Some(LocalError::BindingContract(
        "original binding failure".into(),
    ));
    assert_eq!(
        services
            .run(&mut program, ExecutionLimits::default())
            .unwrap()
            .to_int_vec(),
        vec![Some(17)]
    );
    assert_eq!(services.reads, 3, "failure cannot reuse an earlier success");
    assert_eq!(services.failure_indices, vec![0]);
    assert!(matches!(
        services.failures.pop().unwrap().into_error(),
        LocalError::BindingContract(_)
    ));
    assert!(services.base.live.is_empty());
}
#[test]
fn host_result_global_step_limits_and_panic_cleanup_are_not_reset() {
    for maximum in 0..45 {
        let base = Services::new(
            vec![
                (
                    Code::Sequence(vec![
                        request(0, ArgMode::FreshResult),
                        request(1, ArgMode::Fresh),
                    ]),
                    2,
                ),
                (Code::StartError, 0),
                (Code::Counter, 0),
            ],
            vec![],
        );
        let mut program = base.compile(&host(
            &base.catalog,
            0,
            vec![
                host(&base.catalog, 1, vec![]),
                host(&base.catalog, 2, vec![]),
            ],
        ));
        let mut services = ResultServices::new(base);
        let result = services.run(
            &mut program,
            ExecutionLimits {
                max_steps: maximum,
                ..ExecutionLimits::default()
            },
        );
        assert!(
            result.is_ok() || matches!(result, Err(LocalError::ResourceLimit(_))),
            "global budget: {result:?}"
        );
        assert!(services.base.effects <= 1);
        assert!(services.base.live.is_empty());
    }
    let mut base = Services::new(
        vec![
            (Code::Sequence(vec![request(0, ArgMode::FreshResult)]), 1),
            (Code::Relay, 1),
        ],
        vec![vec![Some(9)]],
    );
    base.read_fault = Some((0, ReadFault::Panic));
    let mut program = base.compile(&host(
        &base.catalog,
        0,
        vec![host(&base.catalog, 1, vec![input(0)])],
    ));
    let mut services = ResultServices::new(base);
    assert!(
        catch_unwind(AssertUnwindSafe(
            || services.run(&mut program, ExecutionLimits::default())
        ))
        .is_err()
    );
    assert!(
        services.failure_indices.is_empty(),
        "panic is never an error reply"
    );
    assert_eq!(services.base.canceled().len(), 2);
    assert!(services.base.live.is_empty());
}
#[test]
fn host_result_provider_change_preserves_primary_without_foreign_cleanup() {
    let mut base = Services::new(
        vec![(Code::Sequence(vec![request(0, ArgMode::FreshResult)]), 1)],
        vec![vec![Some(9)]],
    );
    let mut program = base.compile(&host(&base.catalog, 0, vec![input(0)]));
    base.catalog_after_start = Some(HostCatalog::new(vec![]).unwrap());
    let mut services = ResultServices::new(base);
    services.inject_at = Some(1);
    services.injected = Some(LocalError::BindingContract(
        "actual primary with unreachable provider".into(),
    ));
    assert!(
        matches!(services.run(&mut program,ExecutionLimits::default()),Err(LocalError::BindingContract(reason)) if reason=="actual primary with unreachable provider")
    );
    assert!(services.failure_indices.is_empty());
    assert!(
        services.base.canceled().is_empty(),
        "do not cancel through a changed catalog"
    );
}

#[test]
fn host_result_nested_boundaries_retain_cause_and_cancel_only_their_descendants() {
    let base = Services::new(
        vec![
            (
                Code::Sequence(vec![
                    request(0, ArgMode::FreshResult),
                    request(1, ArgMode::Fresh),
                ]),
                2,
            ),
            (Code::Sequence(vec![request(0, ArgMode::FreshResult)]), 1),
            (Code::Relay, 1),
            (Code::Counter, 0),
        ],
        vec![vec![Some(9)]],
    );
    let graph = host(
        &base.catalog,
        0,
        vec![
            host(
                &base.catalog,
                1,
                vec![host(&base.catalog, 2, vec![input(0)])],
            ),
            host(&base.catalog, 3, vec![]),
        ],
    );
    let mut program = base.compile(&graph);
    let mut services = ResultServices::new(base);
    services.rethrow = true;
    services.inject_at = Some(1);
    let cause = other_err!("one nested result cause");
    let address = cause.0.as_ref() as *const _ as usize;
    services.injected = Some(LocalError::Evaluation(cause));
    let Err(LocalError::Evaluation(error)) = services.run(&mut program, ExecutionLimits::default())
    else {
        panic!("keep nested error");
    };
    assert_eq!(error.0.as_ref() as *const _ as usize, address);
    assert_eq!(services.failure_indices, vec![0, 0]);
    assert_eq!(services.base.effects, 1);
    assert_eq!(services.base.canceled().len(), 3);
    assert!(services.base.live.is_empty());
}
#[test]
fn host_result_ledger_is_bounded_before_child_demand() {
    let base = Services::new(
        vec![(Code::Sequence(vec![request(0, ArgMode::FreshResult)]), 1)],
        vec![vec![Some(9)]],
    );
    let mut program = base.compile(&host(&base.catalog, 0, vec![input(0)]));
    let mut services = ResultServices::new(base);
    let limits = ExecutionLimits {
        max_steps: 0,
        max_frame_depth: 0,
        max_active_tasks: 0,
        max_retained_bytes: 0,
    };
    for expected_reads in 1..=2 {
        assert_eq!(
            services.run(&mut program, limits).unwrap().to_int_vec(),
            vec![Some(9)]
        );
        assert_eq!(services.reads, expected_reads);
        assert!(services.base.live.is_empty());
    }
}

#[test]
fn host_result_preserves_each_child_error_class_with_sufficient_global_budget() {
    for original in [
        LocalError::InvalidSpec("actual child specification failure".into()),
        LocalError::InvalidBatch("actual child batch failure".into()),
        LocalError::BindingContract("actual child binding failure".into()),
        LocalError::HostContract("actual child host failure".into()),
        LocalError::ResourceLimit("actual child resource refusal".into()),
        LocalError::Evaluation(other_err!("actual child evaluation failure")),
    ] {
        let kind = std::mem::discriminant(&original);
        let base = Services::new(
            vec![
                (
                    Code::Sequence(vec![
                        request(0, ArgMode::FreshResult),
                        request(1, ArgMode::Fresh),
                    ]),
                    2,
                ),
                (Code::Counter, 0),
            ],
            vec![vec![Some(9)]],
        );
        let mut program = base.compile(&host(
            &base.catalog,
            0,
            vec![input(0), host(&base.catalog, 1, vec![])],
        ));
        let mut services = ResultServices::new(base);
        services.rethrow = true;
        services.inject_at = Some(1);
        services.injected = Some(original);
        let actual = services
            .run(&mut program, ExecutionLimits::default())
            .unwrap_err();
        assert_eq!(std::mem::discriminant(&actual), kind);
        assert_eq!(
            services.base.effects, 1,
            "each actual child failure reaches its explicit eager consumer"
        );
        assert!(services.base.live.is_empty());
    }
}
