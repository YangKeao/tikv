// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! These are local admission/demand tests, not proof of native PB ingestion,
//! nonidentity coercion, native diagnostics, or native numeric batch semantics.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    thread,
};

use tidb_query_datatype::{
    FieldTypeAccessor, FieldTypeFlag, FieldTypeTp,
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
fn plus(left: LocalExpr, right: LocalExpr) -> LocalExpr {
    call(Sig::PlusInt, vec![left, right])
}
fn source(node: u64) -> OrdinarySourceId {
    OrdinarySourceId::new(7, node)
}
fn site(ordinal: usize, profile: OrdinaryProfile) -> OrdinaryCallSite {
    match profile {
        OrdinaryProfile::TypedRow => OrdinaryCallSite::typed_row(ordinal, source(ordinal as u64)),
        OrdinaryProfile::PbRow => {
            OrdinaryCallSite::pb_row(ordinal, source(ordinal as u64), Sig::PlusInt as i32)
        }
        _ => panic!("test site requires an admitted row profile"),
    }
}
fn sites(spec: &LocalExpr, profile: OrdinaryProfile) -> Vec<OrdinaryCallSite> {
    let mut pending = vec![spec];
    let mut ordinal = 0;
    let mut result = Vec::new();
    while let Some(expr) = pending.pop() {
        if let LocalExpr::Call { args, .. } = expr {
            result.push(site(ordinal, profile));
            pending.extend(args.iter().rev());
        }
        ordinal += 1;
    }
    result
}
fn facts(spec: &LocalExpr, schema: &[FieldType], profile: OrdinaryProfile) -> OrdinaryProfileSpec {
    OrdinaryProfileSpec::new(
        spec,
        schema,
        profile,
        sites(spec, profile),
        CompileLimits::default(),
    )
    .unwrap()
}
fn compile(spec: &LocalExpr, schema: &[FieldType], profile: OrdinaryProfile) -> LocalProgram {
    let facts = facts(spec, schema, profile);
    compile_local_profiled(spec, schema, LocalCompileContext::default(), &facts).unwrap()
}
fn rejected(spec: &LocalExpr, schema: &[FieldType]) {
    assert!(matches!(
        OrdinaryProfileSpec::new(
            spec,
            schema,
            OrdinaryProfile::TypedRow,
            sites(spec, OrdinaryProfile::TypedRow),
            CompileLimits::default()
        ),
        Err(LocalError::InvalidSpec(_))
    ));
}

#[test]
fn profile_ordinals_count_all_nodes_and_preserve_mixed_sources() {
    let expr = plus(
        constant(Some(4)),
        plus(input(0), plus(constant(Some(1)), input(1))),
    );
    let same_source = OrdinarySourceId::new(u64::MAX, 0);
    assert_eq!(same_source.unit(), u64::MAX);
    assert_eq!(same_source.node(), 0);
    for consumer in [OrdinaryProfile::TypedRow, OrdinaryProfile::PbRow] {
        let declared = vec![
            site(0, consumer),
            OrdinaryCallSite::pb_row(2, same_source, 203),
            OrdinaryCallSite::typed_row(4, same_source),
        ];
        let profile = OrdinaryProfileSpec::new(
            &expr,
            &[ft(), ft()],
            consumer,
            declared.clone(),
            CompileLimits::default(),
        )
        .unwrap();
        assert_eq!(profile.consumer(), consumer);
        assert_eq!(profile.node_count(), 7);
        assert_eq!(profile.call_sites(), declared.as_slice());
        assert_eq!(profile.site(2).unwrap().source(), same_source);
        assert_eq!(profile.site(2).unwrap().original_pb_signature(), Some(203));
        assert_eq!(
            profile.site(4).unwrap().profile(),
            OrdinaryProfile::TypedRow
        );
        assert_eq!(profile.site(4).unwrap().original_pb_signature(), None);
        for ordinal in [1, 3, 5, 6, usize::MAX] {
            assert!(profile.site(ordinal).is_none());
        }
        profile
            .validate(&expr, &[ft(), ft()], CompileLimits::default())
            .unwrap();
    }
}

#[test]
fn profile_rejects_incomplete_unsorted_duplicate_or_mismatched_sites() {
    let expr = plus(constant(None), plus(constant(Some(1)), constant(Some(2))));
    let typed = OrdinaryProfile::TypedRow;
    let root = site(0, typed);
    let child = site(2, typed);
    for records in [
        vec![],
        vec![root.clone()],
        vec![child.clone()],
        vec![child.clone(), root.clone()],
        vec![root.clone(), root.clone(), child.clone()],
        vec![root.clone(), site(1, typed), child.clone()],
        vec![root.clone(), child.clone(), site(5, typed)],
        vec![site(0, OrdinaryProfile::PbRow), child.clone()],
    ] {
        assert!(matches!(
            OrdinaryProfileSpec::new(&expr, &[], typed, records, CompileLimits::default()),
            Err(LocalError::InvalidSpec(_))
        ));
    }
    for signature in [-1, 0, 222, i32::MAX] {
        let records = vec![
            root.clone(),
            OrdinaryCallSite::pb_row(2, source(2), signature),
        ];
        assert!(matches!(
            OrdinaryProfileSpec::new(&expr, &[], typed, records, CompileLimits::default()),
            Err(LocalError::InvalidSpec(_))
        ));
    }
    // The two row profiles are distinct even though this seed's demand agrees.
    assert!(matches!(
        OrdinaryProfileSpec::new(
            &expr,
            &[],
            OrdinaryProfile::PbRow,
            vec![root, child],
            CompileLimits::default()
        ),
        Err(LocalError::InvalidSpec(_))
    ));
}

#[test]
fn profile_leaf_roots_need_no_sites_but_still_require_an_admitted_consumer() {
    for profile in [OrdinaryProfile::TypedRow, OrdinaryProfile::PbRow] {
        for expr in [constant(None), input(0)] {
            let facts = facts(&expr, &[ft()], profile);
            assert_eq!(facts.node_count(), 1);
            assert!(facts.call_sites().is_empty());
            assert!(facts.site(0).is_none());
            let mut program =
                compile_local_profiled(&expr, &[ft()], LocalCompileContext::default(), &facts)
                    .unwrap();
            let mut services = Bindings::new(vec![vec![Some(9)]]);
            let expected = if matches!(&expr, LocalExpr::Constant { .. }) {
                None
            } else {
                Some(9)
            };
            assert_eq!(
                run(&mut program, &mut services, 1, &[0])
                    .unwrap()
                    .to_int_vec(),
                vec![expected]
            );
        }
    }
    for profile in [
        OrdinaryProfile::AstValueScalar,
        OrdinaryProfile::NativeNumericBatch,
    ] {
        assert!(matches!(
            OrdinaryProfileSpec::new(
                &constant(None),
                &[],
                profile,
                vec![],
                CompileLimits::default()
            ),
            Err(LocalError::InvalidSpec(_))
        ));
    }
    assert!(matches!(
        OrdinaryProfileSpec::new(
            &constant(None),
            &[],
            OrdinaryProfile::TypedRow,
            vec![site(0, OrdinaryProfile::TypedRow)],
            CompileLimits::default()
        ),
        Err(LocalError::InvalidSpec(_))
    ));
}

#[test]
fn profile_admission_is_exact_signed_longlong_typed_int_and_203() {
    let mut unsigned = ft();
    unsigned.as_mut_accessor().set_flag(FieldTypeFlag::UNSIGNED);
    for field_type in [
        FieldTypeTp::Tiny.into(),
        FieldTypeTp::Long.into(),
        FieldTypeTp::Double.into(),
        FieldTypeTp::VarString.into(),
        unsigned,
    ] {
        rejected(
            &LocalExpr::Constant {
                value: ScalarValue::Int(None),
                field_type: field_type.clone(),
                literal_kind: LiteralKind::Typed,
            },
            &[],
        );
        let mut wrong_result = plus(constant(Some(1)), constant(Some(2)));
        if let LocalExpr::Call { return_type, .. } = &mut wrong_result {
            *return_type = field_type.clone();
        }
        rejected(&wrong_result, &[]);
        // Even an unused schema column must stay inside the closed domain.
        rejected(&constant(Some(1)), &[field_type]);
    }
    for value in [
        ScalarValue::Bytes(None),
        ScalarValue::Bytes(Some(vec![1])),
        ScalarValue::Real(None),
    ] {
        rejected(
            &LocalExpr::Constant {
                value,
                field_type: ft(),
                literal_kind: LiteralKind::Typed,
            },
            &[],
        );
    }
    for literal_kind in [LiteralKind::Text, LiteralKind::BinaryLiteral] {
        for value in [None, Some(1)] {
            rejected(
                &LocalExpr::Constant {
                    value: ScalarValue::Int(value),
                    field_type: ft(),
                    literal_kind,
                },
                &[],
            );
        }
    }
    for sig in [
        Sig::PlusIntSignedSigned,
        Sig::PlusReal,
        Sig::EqInt,
        Sig::GtInt,
        Sig::ModIntSignedSigned,
        Sig::ModIntSignedUnsigned,
        Sig::ModIntUnsignedSigned,
        Sig::ModIntUnsignedUnsigned,
        Sig::ModReal,
        Sig::ModDecimal,
        Sig::AbsInt,
        Sig::LogicalAnd,
        Sig::IfInt,
        Sig::CastIntAsInt,
    ] {
        // NULL left does not exempt an unadmitted descendant from compilation.
        rejected(
            &plus(
                constant(None),
                call(sig, vec![constant(Some(1)), constant(Some(2))]),
            ),
            &[],
        );
    }
    for arity in [0, 1, 3] {
        rejected(
            &call(
                Sig::PlusInt,
                (0..arity).map(|_| constant(Some(1))).collect(),
            ),
            &[],
        );
    }
    let mut expr = plus(constant(Some(1)), constant(Some(2)));
    if let LocalExpr::Call { metadata, .. } = &mut expr {
        *metadata = CallMetadata::InUnion { in_union: false };
    }
    rejected(&expr, &[]);
    rejected(&input(0), &[]);
    let mut changed = ft();
    changed.set_flen(7);
    rejected(&input(0), &[changed]);
    let catalog = HostCatalog::new(vec![HostSignature {
        arg_types: vec![].into_boxed_slice(),
        return_type: ft(),
    }])
    .unwrap();
    rejected(
        &LocalExpr::HostCall {
            slot: catalog.slot(0).unwrap(),
            args: vec![].into_boxed_slice(),
            return_type: ft(),
        },
        &[],
    );
}

#[test]
fn profile_snapshot_rejects_stale_values_slots_shape_types_and_full_schema() {
    let expr = plus(input(0), constant(Some(5)));
    let schema = [ft(), ft()];
    let profile = facts(&expr, &schema, OrdinaryProfile::TypedRow);
    for changed in [
        plus(input(0), constant(Some(6))),
        plus(input(0), constant(None)),
        plus(input(1), constant(Some(5))),
        plus(constant(Some(5)), input(0)),
        plus(input(0), plus(constant(Some(5)), constant(Some(0)))),
        constant(Some(5)),
        call(Sig::PlusIntSignedSigned, vec![input(0), constant(Some(5))]),
    ] {
        assert!(matches!(
            profile.validate(&changed, &schema, CompileLimits::default()),
            Err(LocalError::InvalidSpec(_))
        ));
        assert!(matches!(
            compile_local_profiled(&changed, &schema, LocalCompileContext::default(), &profile),
            Err(LocalError::InvalidSpec(_))
        ));
    }
    let mut changed = plus(input(0), constant(Some(5)));
    if let LocalExpr::Call { return_type, .. } = &mut changed {
        return_type.set_flen(7);
    }
    assert!(
        profile
            .validate(&changed, &schema, CompileLimits::default())
            .is_err()
    );
    let mut changed = plus(input(0), constant(Some(5)));
    if let LocalExpr::Call { args, .. } = &mut changed {
        if let LocalExpr::Constant { field_type, .. } = &mut args[1] {
            field_type.set_decimal(4);
        }
    }
    assert!(
        profile
            .validate(&changed, &schema, CompileLimits::default())
            .is_err()
    );
    let mut changed_schema = schema.clone();
    changed_schema[1].set_flen(7); // Unused slot, same EvalType, changed complete metadata.
    assert!(
        profile
            .validate(&expr, &changed_schema, CompileLimits::default())
            .is_err()
    );
    assert!(
        profile
            .validate(&expr, &[ft()], CompileLimits::default())
            .is_err()
    );
    assert!(
        profile
            .validate(&expr, &[ft(), ft(), ft()], CompileLimits::default())
            .is_err()
    );
    profile
        .validate(&expr, &schema, CompileLimits::default())
        .unwrap();
}

#[test]
fn profile_new_and_validate_obey_exact_node_and_depth_limits() {
    let expr = plus(constant(None), constant(Some(2)));
    let exact = CompileLimits {
        max_nodes: 3,
        max_depth: 2,
    };
    let profile = OrdinaryProfileSpec::new(
        &expr,
        &[],
        OrdinaryProfile::TypedRow,
        vec![site(0, OrdinaryProfile::TypedRow)],
        exact,
    )
    .unwrap();
    profile.validate(&expr, &[], exact).unwrap();
    for limits in [
        CompileLimits {
            max_nodes: 0,
            ..exact
        },
        CompileLimits {
            max_nodes: 2,
            ..exact
        },
        CompileLimits {
            max_depth: 0,
            ..exact
        },
        CompileLimits {
            max_depth: 1,
            ..exact
        },
    ] {
        assert!(matches!(
            OrdinaryProfileSpec::new(
                &expr,
                &[],
                OrdinaryProfile::TypedRow,
                vec![site(0, OrdinaryProfile::TypedRow)],
                limits
            ),
            Err(LocalError::ResourceLimit(_))
        ));
        assert!(matches!(
            profile.validate(&expr, &[], limits),
            Err(LocalError::ResourceLimit(_))
        ));
        assert!(matches!(
            compile_local_profiled(&expr, &[], LocalCompileContext { limits }, &profile),
            Err(LocalError::ResourceLimit(_))
        ));
    }
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    Binding,
    Resource,
    Evaluation,
    WrongType,
    WrongLength,
    Panic,
}
struct Bindings {
    schema: Vec<FieldType>,
    values: Vec<Vec<Option<i64>>>,
    reads: Vec<(usize, usize, usize)>,
    fault: Option<(usize, Fault)>,
    warnings: bool,
}
impl Bindings {
    fn new(values: Vec<Vec<Option<i64>>>) -> Self {
        Self {
            schema: vec![ft(); values.len()],
            values,
            reads: vec![],
            fault: None,
            warnings: false,
        }
    }
}
fn warn(ctx: &mut EvalContext, message: String) {
    ctx.warnings.append_warning(Error::Eval(message, 1234));
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
        self.reads.push((row.occurrence, row.input_row, slot));
        if self.warnings {
            warn(
                ctx,
                format!("read:{}:{}:{}", row.occurrence, row.input_row, slot),
            );
        }
        if let Some((target, fault)) = self.fault {
            if target == slot {
                return match fault {
                    Fault::Binding => Err(LocalError::BindingContract("primary binding".into())),
                    Fault::Resource => Err(LocalError::ResourceLimit("primary resource".into())),
                    Fault::Evaluation => {
                        Err(LocalError::Evaluation(other_err!("primary evaluation")))
                    }
                    Fault::WrongType => Ok(VectorValue::from_scalar(&ScalarValue::Bytes(None), 1)),
                    Fault::WrongLength => Ok(VectorValue::from_scalar(&ScalarValue::Int(None), 2)),
                    Fault::Panic => panic!("primary read panic"),
                };
            }
        }
        Ok(VectorValue::from_scalar(
            &ScalarValue::Int(self.values[slot][row.input_row]),
            1,
        ))
    }
    fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
        panic!("host-free profile consulted the optional host hook")
    }
}
fn run(
    program: &mut LocalProgram,
    services: &mut Bindings,
    physical_rows: usize,
    selection: &[usize],
) -> LocalResult<VectorValue> {
    program.eval_with_bindings(
        ExecutionLimits::default(),
        &mut EvalContext::default(),
        physical_rows,
        selection,
        services,
    )
}

#[test]
fn profiled_203_null_stops_while_legacy_222_stays_eager() {
    for profile in [OrdinaryProfile::TypedRow, OrdinaryProfile::PbRow] {
        let expr = plus(input(0), input(1));
        let mut program = compile(&expr, &[ft(), ft()], profile);
        let mut services = Bindings::new(vec![vec![None, None], vec![Some(9), Some(11)]]);
        services.fault = Some((1, Fault::Binding));
        assert_eq!(
            run(&mut program, &mut services, 2, &[1, 0, 1])
                .unwrap()
                .to_int_vec(),
            vec![None; 3]
        );
        assert_eq!(services.reads, vec![(0, 1, 0), (1, 0, 0), (2, 1, 0)]);
        services.reads.clear();
        services.values[0][0] = Some(5);
        assert!(matches!(
            run(&mut program, &mut services, 2, &[0]),
            Err(LocalError::BindingContract(_))
        ));
        assert_eq!(services.reads, vec![(0, 0, 0), (0, 0, 1)]);
    }
    let expr = call(Sig::PlusIntSignedSigned, vec![constant(None), input(0)]);
    let mut program = compile_local(&expr, &[ft()], LocalCompileContext::default()).unwrap();
    let mut services = Bindings::new(vec![vec![Some(1)]]);
    services.fault = Some((0, Fault::Binding));
    assert!(matches!(
        run(&mut program, &mut services, 1, &[0]),
        Err(LocalError::BindingContract(_))
    ));
    assert_eq!(services.reads, vec![(0, 0, 0)]);
    assert!(matches!(
        compile_local(
            &plus(constant(Some(1)), constant(Some(2))),
            &[],
            LocalCompileContext::default()
        ),
        Err(LocalError::InvalidSpec(_))
    ));
}

#[test]
fn profiled_mixed_origins_demand_nested_calls_independently() {
    let expr = plus(input(0), plus(input(1), input(2)));
    let declared = vec![
        OrdinaryCallSite::typed_row(0, source(99)),
        OrdinaryCallSite::pb_row(2, source(99), 203),
    ];
    let facts = OrdinaryProfileSpec::new(
        &expr,
        &[ft(), ft(), ft()],
        OrdinaryProfile::TypedRow,
        declared,
        CompileLimits::default(),
    )
    .unwrap();
    let mut program = compile_local_profiled(
        &expr,
        &[ft(), ft(), ft()],
        LocalCompileContext::default(),
        &facts,
    )
    .unwrap();
    let [crate::RpnExpressionNode::OrdinaryFnCall { prepared, args }] = program.expression.as_ref()
    else {
        panic!("profiled root must retain its ordinary call descriptor");
    };
    assert_eq!(prepared.function(), FunctionRef::TiPb(Sig::PlusInt));
    assert_eq!(prepared.site(), &facts.call_sites()[0]);
    let [crate::RpnExpressionNode::OrdinaryFnCall { prepared, .. }] = args[1].as_ref() else {
        panic!("nested profiled call must retain its own descriptor");
    };
    assert_eq!(prepared.function(), FunctionRef::TiPb(Sig::PlusInt));
    assert_eq!(prepared.site(), &facts.call_sites()[1]);
    let mut services = Bindings::new(vec![
        vec![None, Some(1), Some(2)],
        vec![Some(i64::MAX), None, Some(3)],
        vec![Some(1), Some(4), Some(5)],
    ]);
    assert_eq!(
        run(&mut program, &mut services, 3, &[0, 1, 2])
            .unwrap()
            .to_int_vec(),
        vec![None, None, Some(10)]
    );
    assert_eq!(
        services.reads,
        vec![
            (0, 0, 0),
            (1, 1, 0),
            (1, 1, 1),
            (2, 2, 0),
            (2, 2, 1),
            (2, 2, 2)
        ]
    );
}

#[test]
fn profiled_selection_and_owned_plan_survive_rebinding_without_cache() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<OrdinaryProfileSpec>();
    let mut program = {
        let expr = plus(input(0), constant(Some(1)));
        let profile = facts(&expr, &[ft()], OrdinaryProfile::TypedRow);
        compile_local_profiled(&expr, &[ft()], LocalCompileContext::default(), &profile).unwrap()
    }; // Source tree and facts are gone; the compiled call owns its metadata.
    let mut services = Bindings::new(vec![(0..1025).map(|n| Some(i64::from(n))).collect()]);
    let state = ExecutionLimits::default();
    let mut ctx = EvalContext::default();
    assert!(
        program
            .eval_with_bindings(state, &mut ctx, 1025, &[], &mut services)
            .unwrap()
            .is_empty()
    );
    assert!(services.reads.is_empty());
    let selection: Vec<usize> = (0..1025).rev().collect();
    let output = program
        .eval_with_bindings(state, &mut ctx, 1025, &selection, &mut services)
        .unwrap();
    assert_eq!(
        output.to_int_vec(),
        selection
            .iter()
            .map(|&n| Some(n as i64 + 1))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        services.reads,
        selection
            .iter()
            .enumerate()
            .map(|(o, &p)| (o, p, 0))
            .collect::<Vec<_>>()
    );
    services.reads.clear();
    services.values[0][2] = None;
    assert_eq!(
        program
            .eval_with_bindings(state, &mut ctx, 1025, &[2, 0, 2], &mut services)
            .unwrap()
            .to_int_vec(),
        vec![None, Some(1), None]
    );
    assert_eq!(services.reads, vec![(0, 2, 0), (1, 0, 0), (2, 2, 0)]);
}

#[test]
fn profiled_child_errors_keep_primary_variant_and_warning_prefix() {
    for fault in [
        Fault::Binding,
        Fault::Resource,
        Fault::Evaluation,
        Fault::WrongType,
        Fault::WrongLength,
    ] {
        for slot in [0, 1] {
            let mut program = compile(
                &plus(input(0), input(1)),
                &[ft(), ft()],
                OrdinaryProfile::TypedRow,
            );
            let mut services = Bindings::new(vec![vec![Some(1), Some(2)], vec![Some(3), Some(4)]]);
            services.fault = Some((slot, fault));
            services.warnings = true;
            let mut ctx = EvalContext::default();
            warn(&mut ctx, "prior".into());
            let error = program
                .eval_with_bindings(
                    ExecutionLimits::default(),
                    &mut ctx,
                    2,
                    &[0, 1],
                    &mut services,
                )
                .unwrap_err();
            match fault {
                Fault::Resource => assert!(matches!(error, LocalError::ResourceLimit(_))),
                Fault::Evaluation => assert!(matches!(error, LocalError::Evaluation(_))),
                _ => assert!(matches!(error, LocalError::BindingContract(_))),
            }
            if matches!(fault, Fault::Binding | Fault::Resource | Fault::Evaluation) {
                assert!(error.to_string().contains("primary"));
            }
            let expected = if slot == 0 {
                vec![(0, 0, 0)]
            } else {
                vec![(0, 0, 0), (0, 0, 1)]
            };
            assert_eq!(services.reads, expected);
            assert_eq!(ctx.warnings.warning_cnt as usize, slot + 2);
            let suffixes = ["prior", "read:0:0:0", "read:0:0:1"];
            for (warning, suffix) in ctx.warnings.warnings.iter().zip(suffixes) {
                assert!(warning.get_msg().ends_with(suffix));
            }
        }
    }
}

#[test]
fn profiled_kernel_error_stops_later_rows_without_replaying_operands() {
    let mut program = compile(
        &plus(input(0), constant(Some(1))),
        &[ft()],
        OrdinaryProfile::PbRow,
    );
    let mut services = Bindings::new(vec![vec![Some(i64::MAX), Some(0)]]);
    services.warnings = true;
    let mut ctx = EvalContext::default();
    warn(&mut ctx, "prior".into());
    let state = ExecutionLimits::default();
    let error = program
        .eval_with_bindings(state, &mut ctx, 2, &[0, 1], &mut services)
        .unwrap_err();
    assert!(matches!(error, LocalError::Evaluation(_)));
    assert!(error.to_string().contains("BIGINT"));
    assert_eq!(services.reads, vec![(0, 0, 0)]);
    assert_eq!(ctx.warnings.warning_cnt, 2);
    assert!(ctx.warnings.warnings[0].get_msg().ends_with("prior"));
    assert!(ctx.warnings.warnings[1].get_msg().ends_with("read:0:0:0"));
    services.values[0][0] = Some(0);
    services.reads.clear();
    assert_eq!(
        program
            .eval_with_bindings(state, &mut ctx, 2, &[0], &mut services)
            .unwrap()
            .to_int_vec(),
        vec![Some(1)]
    );
    assert_eq!(services.reads, vec![(0, 0, 0)]);
}

#[test]
fn profiled_input_unwind_does_not_poison_the_next_invocation() {
    let mut program = compile(
        &plus(input(0), input(1)),
        &[ft(), ft()],
        OrdinaryProfile::TypedRow,
    );
    let mut services = Bindings::new(vec![vec![Some(5)], vec![Some(7)]]);
    services.fault = Some((0, Fault::Panic));
    let state = ExecutionLimits::default();
    let mut ctx = EvalContext::default();
    let panic = catch_unwind(AssertUnwindSafe(|| {
        program.eval_with_bindings(state, &mut ctx, 1, &[0], &mut services)
    }))
    .unwrap_err();
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
    assert_eq!(message, Some("primary read panic"));
    assert_eq!(services.reads, vec![(0, 0, 0)]);
    services.reads.clear();
    services.fault = None;
    assert_eq!(
        program
            .eval_with_bindings(state, &mut ctx, 1, &[0], &mut services)
            .unwrap()
            .to_int_vec(),
        vec![Some(12)]
    );
    assert_eq!(services.reads, vec![(0, 0, 0), (0, 0, 1)]);
}

#[test]
fn profiled_batch_preflight_remains_effect_free_even_for_empty_selection() {
    let mut program = compile(
        &plus(input(0), constant(Some(1))),
        &[ft(), ft()],
        OrdinaryProfile::TypedRow,
    );
    let mut services = Bindings::new(vec![vec![None], vec![Some(4)]]);
    services.schema[1].set_flen(7);
    services.fault = Some((0, Fault::Panic));
    for selection in [&[][..], &[0][..]] {
        assert!(matches!(
            run(&mut program, &mut services, 1, selection),
            Err(LocalError::InvalidBatch(_))
        ));
    }
    services.schema[1] = ft();
    assert!(matches!(
        run(&mut program, &mut services, 1, &[1]),
        Err(LocalError::InvalidBatch(_))
    ));
    assert!(services.reads.is_empty());
    let columns = LazyBatchColumnVec::from(vec![
        VectorValue::from_scalar(&ScalarValue::Int(None), 1),
        VectorValue::from_scalar(&ScalarValue::Int(Some(4)), 1),
    ]);
    assert_eq!(
        program
            .eval(
                ExecutionLimits::default(),
                &mut EvalContext::default(),
                LocalBatch {
                    columns: &columns,
                    physical_rows: 1,
                    selection: &[0]
                }
            )
            .unwrap()
            .to_int_vec(),
        vec![None]
    );
}

#[test]
fn profiled_limits_precede_reads_and_do_not_reserve_host_tasks() {
    let mut program = compile(
        &plus(input(0), input(1)),
        &[ft(), ft()],
        OrdinaryProfile::TypedRow,
    );
    for limits in [
        ExecutionLimits {
            max_steps: 0,
            ..ExecutionLimits::default()
        },
        ExecutionLimits {
            max_frame_depth: 1,
            ..ExecutionLimits::default()
        },
        ExecutionLimits {
            max_retained_bytes: 0,
            ..ExecutionLimits::default()
        },
    ] {
        let mut services = Bindings::new(vec![vec![Some(1)], vec![Some(2)]]);
        let result =
            program.eval_with_bindings(limits, &mut EvalContext::default(), 1, &[0], &mut services);
        assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
        assert!(services.reads.is_empty());
    }
    let mut services = Bindings::new(vec![vec![Some(1)], vec![Some(2)]]);
    assert_eq!(
        program
            .eval_with_bindings(
                ExecutionLimits {
                    max_active_tasks: 0,
                    ..ExecutionLimits::default()
                },
                &mut EvalContext::default(),
                1,
                &[0],
                &mut services
            )
            .unwrap()
            .to_int_vec(),
        vec![Some(3)]
    );
}

#[test]
fn profiled_work_budget_meters_requests_accepts_null_finish_and_kernel() {
    // root + left request/node/accept + right request/node/accept + kernel = 8.
    let mut program = compile(
        &plus(constant(Some(1)), constant(Some(2))),
        &[],
        OrdinaryProfile::TypedRow,
    );
    for max_steps in [7, 8] {
        let mut services = Bindings::new(vec![]);
        let result = program.eval_with_bindings(
            ExecutionLimits {
                max_steps,
                ..ExecutionLimits::default()
            },
            &mut EvalContext::default(),
            1,
            &[0],
            &mut services,
        );
        if max_steps == 7 {
            assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
        } else {
            assert_eq!(result.unwrap().to_int_vec(), vec![Some(3)]);
        }
    }
    // A NULL left constant still charges its finish, but never requests right.
    let mut program = compile(
        &plus(constant(None), input(0)),
        &[ft()],
        OrdinaryProfile::PbRow,
    );
    for max_steps in [4, 5] {
        let mut services = Bindings::new(vec![vec![Some(1)]]);
        services.fault = Some((0, Fault::Panic));
        let result = program.eval_with_bindings(
            ExecutionLimits {
                max_steps,
                ..ExecutionLimits::default()
            },
            &mut EvalContext::default(),
            1,
            &[0],
            &mut services,
        );
        if max_steps == 4 {
            assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
        } else {
            assert_eq!(result.unwrap().to_int_vec(), vec![None]);
        }
        assert!(services.reads.is_empty());
    }
    // Two imported operands add two read charges. A budget of 7 admits the
    // right node, but denies its read; 8/9 admit both reads but not the kernel.
    let mut program = compile(
        &plus(input(0), input(1)),
        &[ft(), ft()],
        OrdinaryProfile::TypedRow,
    );
    for max_steps in [7, 8, 9, 10] {
        let mut services = Bindings::new(vec![vec![Some(1)], vec![Some(2)]]);
        let result = program.eval_with_bindings(
            ExecutionLimits {
                max_steps,
                ..ExecutionLimits::default()
            },
            &mut EvalContext::default(),
            1,
            &[0],
            &mut services,
        );
        assert_eq!(services.reads.len(), if max_steps == 7 { 1 } else { 2 });
        if max_steps < 10 {
            assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
        } else {
            assert_eq!(result.unwrap().to_int_vec(), vec![Some(3)]);
        }
    }
}

#[test]
fn profiled_deep_execution_snapshot_metadata_and_drop_are_iterative() {
    thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn_wrapper(|| {
            for depth in [33, 64, 256] {
                let mut expr = input(0);
                for _ in 0..depth {
                    expr = plus(expr, constant(Some(0)));
                }
                let limits = CompileLimits {
                    max_nodes: depth * 2 + 1,
                    max_depth: depth + 1,
                };
                let facts = OrdinaryProfileSpec::new(
                    &expr,
                    &[ft()],
                    OrdinaryProfile::TypedRow,
                    sites(&expr, OrdinaryProfile::TypedRow),
                    limits,
                )
                .unwrap();
                assert_eq!(facts.node_count(), depth * 2 + 1);
                assert_eq!(facts.call_sites().len(), depth);
                let detached = facts.clone();
                assert!(format!("{detached:?}").contains("TypedRow"));
                detached.validate(&expr, &[ft()], limits).unwrap();
                let mut program =
                    compile_local_profiled(&expr, &[ft()], LocalCompileContext { limits }, &facts)
                        .unwrap();
                assert_eq!(program.expression.node_count(), depth * 2 + 1);
                assert_eq!(program.expression.work_count(), depth * 2 + 1);
                assert_eq!(program.expression.column_ref_count(), 1);
                assert_eq!(program.expression.referenced_column_offsets(), &[0]);
                let mut services = Bindings::new(vec![vec![Some(11)]]);
                assert_eq!(
                    run(&mut program, &mut services, 1, &[0])
                        .unwrap()
                        .to_int_vec(),
                    vec![Some(11)]
                );
                assert_eq!(services.reads, vec![(0, 0, 0)]);
                drop(program);
                drop(facts);
                drop(detached);
                drop(expr);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

// C3c is a separate admission and scheduling contract. These source-authored
// fixtures do not establish a native TiDB consumer route or diagnostic parity.
fn batch_site(ordinal: usize) -> OrdinaryCallSite {
    OrdinaryCallSite::sql_native_numeric_batch(ordinal, source(ordinal as u64))
}

fn batch_sites(spec: &LocalExpr) -> Vec<OrdinaryCallSite> {
    let mut pending = vec![spec];
    let mut ordinal = 0;
    let mut result = Vec::new();
    while let Some(expr) = pending.pop() {
        if let LocalExpr::Call { args, .. } = expr {
            result.push(batch_site(ordinal));
            pending.extend(args.iter().rev());
        }
        ordinal += 1;
    }
    result
}

fn batch_facts(spec: &LocalExpr, schema: &[FieldType]) -> NumericBatchFacts {
    NumericBatchFacts::sql_native_numeric_batch(
        spec,
        schema,
        batch_sites(spec),
        CompileLimits::default(),
    )
    .unwrap()
}

fn compile_batch(spec: &LocalExpr, schema: &[FieldType]) -> LocalNumericBatchProgram {
    compile_numeric_batch(
        spec,
        schema,
        LocalCompileContext::default(),
        &batch_facts(spec, schema),
    )
    .unwrap()
}

fn batch_rejected(spec: &LocalExpr, schema: &[FieldType]) {
    assert!(matches!(
        NumericBatchFacts::sql_native_numeric_batch(
            spec,
            schema,
            batch_sites(spec),
            CompileLimits::default(),
        ),
        Err(LocalError::InvalidSpec(_))
    ));
}

fn run_batch(
    program: &mut LocalNumericBatchProgram,
    services: &mut dyn LocalRuntimeServices,
    physical_rows: usize,
    selection: &[usize],
) -> LocalResult<VectorValue> {
    program.eval_with_bindings(
        ExecutionLimits::default(),
        &mut EvalContext::default(),
        physical_rows,
        selection,
        services,
    )
}

fn batch_kernel_site(ordinal: usize, occurrence: usize, input_row: usize) -> LocalFailureSite {
    LocalFailureSite::Kernel {
        call: batch_site(ordinal),
        row: InputRow {
            occurrence,
            input_row,
        },
    }
}

/// Faults target an occurrence, not a physical row: repeated selections must
/// still make independent imports. The old Bindings helper records every call.
struct BatchBindings {
    inner: Bindings,
    fault_on: Option<(usize, usize, Fault)>,
}

impl BatchBindings {
    fn new(values: Vec<Vec<Option<i64>>>) -> Self {
        Self {
            inner: Bindings::new(values),
            fault_on: None,
        }
    }
}

impl LocalRuntimeServices for BatchBindings {
    fn binding_schema(&self) -> &[FieldType] {
        &self.inner.schema
    }

    fn read_input(
        &mut self,
        ctx: &mut EvalContext,
        slot: usize,
        row: InputRow,
        expected: &FieldType,
    ) -> LocalResult<VectorValue> {
        self.inner.fault = self.fault_on.and_then(|(target_slot, occurrence, fault)| {
            (slot == target_slot && row.occurrence == occurrence).then_some((slot, fault))
        });
        self.inner.read_input(ctx, slot, row, expected)
    }

    fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
        panic!("numeric batch consulted the optional host hook")
    }
}

#[test]
fn numeric_batch_facts_cover_all_node_ordinals_without_source_id_allocation() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<NumericBatchFacts>();
    let expr = plus(
        constant(Some(4)),
        plus(input(0), plus(constant(Some(1)), input(1))),
    );
    let same_source = OrdinarySourceId::new(u64::MAX, u64::MAX);
    let declared: Vec<_> = [0, 2, 4]
        .into_iter()
        .map(|ordinal| OrdinaryCallSite::sql_native_numeric_batch(ordinal, same_source))
        .collect();
    let facts = NumericBatchFacts::sql_native_numeric_batch(
        &expr,
        &[ft(), ft()],
        declared.clone(),
        CompileLimits::default(),
    )
    .unwrap();
    assert_eq!(facts.node_count(), 7);
    assert_eq!(facts.call_sites(), declared.as_slice());
    for ordinal in [0, 2, 4] {
        let site = facts.site(ordinal).unwrap();
        assert_eq!(site.ordinal(), ordinal);
        assert_eq!(site.source(), same_source);
        assert_eq!(site.profile(), OrdinaryProfile::NativeNumericBatch);
        assert_eq!(site.original_pb_signature(), None);
    }
    for ordinal in [1, 3, 5, 6, usize::MAX] {
        assert!(facts.site(ordinal).is_none());
    }
    let detached = facts.clone();
    assert!(format!("{detached:?}").contains("NativeNumericBatch"));
    detached
        .validate(&expr, &[ft(), ft()], CompileLimits::default())
        .unwrap();
}

#[test]
fn numeric_batch_sites_reject_missing_extra_unsorted_duplicate_and_row_labels() {
    let expr = plus(constant(None), plus(constant(Some(1)), constant(Some(2))));
    for declared in [
        vec![],
        vec![batch_site(0)],
        vec![batch_site(2)],
        vec![batch_site(2), batch_site(0)],
        vec![batch_site(0), batch_site(0), batch_site(2)],
        vec![batch_site(0), batch_site(1), batch_site(2)],
        vec![batch_site(0), batch_site(2), batch_site(5)],
        vec![site(0, OrdinaryProfile::TypedRow), batch_site(2)],
        vec![batch_site(0), site(2, OrdinaryProfile::TypedRow)],
        vec![site(0, OrdinaryProfile::PbRow), batch_site(2)],
        vec![batch_site(0), site(2, OrdinaryProfile::PbRow)],
    ] {
        assert!(matches!(
            NumericBatchFacts::sql_native_numeric_batch(
                &expr,
                &[],
                declared,
                CompileLimits::default(),
            ),
            Err(LocalError::InvalidSpec(_))
        ));
    }
    assert!(matches!(
        NumericBatchFacts::sql_native_numeric_batch(
            &constant(None),
            &[],
            vec![batch_site(0)],
            CompileLimits::default(),
        ),
        Err(LocalError::InvalidSpec(_))
    ));
}

#[test]
fn numeric_batch_labels_cannot_enter_the_old_row_consumer_or_child_route() {
    let expr = plus(constant(None), plus(constant(Some(1)), constant(Some(2))));
    for consumer in [OrdinaryProfile::TypedRow, OrdinaryProfile::PbRow] {
        for declared in [
            vec![batch_site(0), site(2, consumer)],
            vec![site(0, consumer), batch_site(2)],
            batch_sites(&expr),
        ] {
            assert!(matches!(
                OrdinaryProfileSpec::new(&expr, &[], consumer, declared, CompileLimits::default(),),
                Err(LocalError::InvalidSpec(_))
            ));
        }
    }
    for expr in [constant(None), expr] {
        assert!(matches!(
            OrdinaryProfileSpec::new(
                &expr,
                &[],
                OrdinaryProfile::NativeNumericBatch,
                batch_sites(&expr),
                CompileLimits::default(),
            ),
            Err(LocalError::InvalidSpec(_))
        ));
    }
}

#[test]
fn numeric_batch_strict_raw_type_gate_covers_nodes_and_unused_schema() {
    let mut bad_types: Vec<FieldType> = vec![
        FieldTypeTp::Tiny.into(),
        FieldTypeTp::Long.into(),
        FieldTypeTp::Double.into(),
        FieldTypeTp::VarString.into(),
        FieldTypeTp::Null.into(),
    ];
    let mut array = ft();
    array.set_array(true);
    bad_types.push(array);
    for raw_type in [-1, 264, i32::MAX] {
        let mut field = ft();
        field.set_tp(raw_type);
        bad_types.push(field);
    }
    for bit in [5, 8, 11, 18, 21, 25, 26, 27, 28, 29, 30, 31] {
        let mut field = ft();
        field.set_flag(1u32 << bit);
        bad_types.push(field);
    }
    let clean = plus(constant(None), constant(Some(1)));
    let facts = batch_facts(&clean, &[]);
    for field in bad_types {
        let mut result = plus(constant(None), constant(Some(1)));
        if let LocalExpr::Call { return_type, .. } = &mut result {
            *return_type = field.clone();
        }
        let leaf = LocalExpr::Constant {
            value: ScalarValue::Int(None),
            field_type: field.clone(),
            literal_kind: LiteralKind::Typed,
        };
        for expr in [leaf.clone(), plus(constant(None), leaf), result] {
            batch_rejected(&expr, &[]);
            assert!(matches!(
                facts.validate(&expr, &[], CompileLimits::default()),
                Err(LocalError::InvalidSpec(_))
            ));
            assert!(matches!(
                compile_numeric_batch(&expr, &[], LocalCompileContext::default(), &facts),
                Err(LocalError::InvalidSpec(_))
            ));
        }
        batch_rejected(&constant(Some(1)), &[ft(), field.clone()]);
        batch_rejected(
            &LocalExpr::InputSlot {
                slot: 0,
                field_type: field.clone(),
            },
            &[field],
        );
    }
}

#[test]
fn numeric_batch_preserves_documented_raw_flags_without_narrowing_old_row_policy() {
    let excluded = (1u32 << 5) | (1 << 8) | (1 << 11) | (1 << 18) | (1 << 21);
    let allowed = ((1u32 << 25) - 1) & !excluded;
    for flags in (0..25)
        .map(|bit| 1u32 << bit)
        .filter(|flag| flag & excluded == 0)
        .chain([allowed])
    {
        let mut field = ft();
        field.set_flag(flags);
        field.set_flen(19);
        field.set_decimal(0);
        field.set_collate(63);
        let expr = LocalExpr::InputSlot {
            slot: 0,
            field_type: field.clone(),
        };
        let facts = batch_facts(&expr, std::slice::from_ref(&field));
        facts
            .validate(
                &expr,
                std::slice::from_ref(&field),
                CompileLimits::default(),
            )
            .unwrap();
        let program = compile_numeric_batch(
            &expr,
            std::slice::from_ref(&field),
            LocalCompileContext::default(),
            &facts,
        )
        .unwrap();
        assert_eq!(program.return_type(), &field);
        assert_eq!(program.return_type().get_flag(), flags);
        let mut changed = field.clone();
        changed.set_flen(20);
        assert!(
            facts
                .validate(&expr, &[changed], CompileLimits::default())
                .is_err()
        );
    }
    // C3c's array/raw-flag exclusions do not retroactively alter C3a.
    let mut field = ft();
    field.set_array(true);
    field.set_flag((1 << 8) | (1 << 25));
    let expr = LocalExpr::Constant {
        value: ScalarValue::Int(None),
        field_type: field,
        literal_kind: LiteralKind::Typed,
    };
    facts(&expr, &[], OrdinaryProfile::TypedRow)
        .validate(&expr, &[], CompileLimits::default())
        .unwrap();
    batch_rejected(&expr, &[]);
}

#[test]
fn numeric_batch_snapshot_rejects_changed_values_slots_shape_and_full_types() {
    let expr = plus(input(0), constant(Some(5)));
    let schema = [ft(), ft()];
    let facts = batch_facts(&expr, &schema);
    let mut changed_result = plus(input(0), constant(Some(5)));
    if let LocalExpr::Call { return_type, .. } = &mut changed_result {
        return_type.set_flen(7);
    }
    let mut changed_leaf = plus(input(0), constant(Some(5)));
    if let LocalExpr::Call { args, .. } = &mut changed_leaf {
        if let LocalExpr::Constant { field_type, .. } = &mut args[1] {
            field_type.set_decimal(4);
        }
    }
    for changed in [
        plus(input(0), constant(Some(6))),
        plus(input(0), constant(None)),
        plus(input(1), constant(Some(5))),
        plus(constant(Some(5)), input(0)),
        plus(input(0), plus(constant(Some(5)), constant(Some(0)))),
        constant(Some(5)),
        changed_result,
        changed_leaf,
    ] {
        assert!(matches!(
            facts.validate(&changed, &schema, CompileLimits::default()),
            Err(LocalError::InvalidSpec(_))
        ));
        assert!(matches!(
            compile_numeric_batch(&changed, &schema, LocalCompileContext::default(), &facts),
            Err(LocalError::InvalidSpec(_))
        ));
    }
    let mut changed_schema = schema.clone();
    let changed_collation = changed_schema[1].get_collate().wrapping_add(1);
    changed_schema[1].set_collate(changed_collation);
    for schema in [changed_schema.to_vec(), vec![ft()], vec![ft(); 3]] {
        assert!(matches!(
            compile_numeric_batch(&expr, &schema, LocalCompileContext::default(), &facts),
            Err(LocalError::InvalidSpec(_))
        ));
    }
}

#[test]
fn numeric_batch_only_admits_exact_203_arity_metadata_and_typed_int_literals() {
    for sig in [
        Sig::PlusIntSignedSigned,
        Sig::PlusReal,
        Sig::EqInt,
        Sig::CastIntAsInt,
        Sig::LogicalAnd,
        Sig::LogicalOr,
        Sig::IfInt,
        Sig::IfString,
        Sig::IfNullInt,
        Sig::CaseWhenInt,
        Sig::CoalesceInt,
    ] {
        let args = match sig {
            Sig::IfInt | Sig::IfString | Sig::CaseWhenInt => {
                vec![constant(Some(1)), constant(Some(2)), constant(Some(3))]
            }
            Sig::CastIntAsInt | Sig::CoalesceInt => vec![constant(Some(1))],
            _ => vec![constant(Some(1)), constant(Some(2))],
        };
        let other = call(sig, args);
        batch_rejected(&other, &[]);
        batch_rejected(&plus(constant(None), other), &[]);
    }
    for arity in [0, 1, 3] {
        batch_rejected(
            &call(
                Sig::PlusInt,
                (0..arity).map(|_| constant(Some(1))).collect(),
            ),
            &[],
        );
    }
    let mut metadata = plus(constant(None), constant(Some(1)));
    if let LocalExpr::Call { metadata, .. } = &mut metadata {
        *metadata = CallMetadata::InUnion { in_union: false };
    }
    batch_rejected(&metadata, &[]);
    for value in [ScalarValue::Bytes(None), ScalarValue::Real(None)] {
        batch_rejected(
            &LocalExpr::Constant {
                value,
                field_type: ft(),
                literal_kind: LiteralKind::Typed,
            },
            &[],
        );
    }
    for literal_kind in [LiteralKind::Text, LiteralKind::BinaryLiteral] {
        for value in [None, Some(1)] {
            batch_rejected(
                &LocalExpr::Constant {
                    value: ScalarValue::Int(value),
                    field_type: ft(),
                    literal_kind,
                },
                &[],
            );
        }
    }
    batch_rejected(&input(0), &[]);
    let catalog = HostCatalog::new(vec![HostSignature {
        arg_types: vec![].into_boxed_slice(),
        return_type: ft(),
    }])
    .unwrap();
    let host = LocalExpr::HostCall {
        slot: catalog.slot(0).unwrap(),
        args: vec![].into_boxed_slice(),
        return_type: ft(),
    };
    batch_rejected(&host, &[]);
    batch_rejected(&plus(constant(None), host), &[]);
}

#[test]
fn numeric_batch_fact_capture_revalidation_and_compile_obey_exact_limits() {
    let expr = plus(constant(None), constant(Some(2)));
    let exact = CompileLimits {
        max_nodes: 3,
        max_depth: 2,
    };
    let facts = NumericBatchFacts::sql_native_numeric_batch(&expr, &[], vec![batch_site(0)], exact)
        .unwrap();
    facts.validate(&expr, &[], exact).unwrap();
    compile_numeric_batch(&expr, &[], LocalCompileContext { limits: exact }, &facts).unwrap();
    for limits in [
        CompileLimits {
            max_nodes: 0,
            ..exact
        },
        CompileLimits {
            max_nodes: 2,
            ..exact
        },
        CompileLimits {
            max_depth: 0,
            ..exact
        },
        CompileLimits {
            max_depth: 1,
            ..exact
        },
    ] {
        assert!(matches!(
            NumericBatchFacts::sql_native_numeric_batch(&expr, &[], vec![batch_site(0)], limits,),
            Err(LocalError::ResourceLimit(_))
        ));
        assert!(matches!(
            facts.validate(&expr, &[], limits),
            Err(LocalError::ResourceLimit(_))
        ));
        assert!(matches!(
            compile_numeric_batch(&expr, &[], LocalCompileContext { limits }, &facts),
            Err(LocalError::ResourceLimit(_))
        ));
    }
}

#[test]
fn numeric_batch_leaf_and_call_roots_cover_empty_one_1024_and_reject_1025() {
    for shape in 0..4 {
        let expr = match shape {
            0 => constant(Some(7)),
            1 => constant(None),
            2 => input(0),
            _ => plus(input(0), constant(Some(1))),
        };
        let schema = if shape < 2 { vec![] } else { vec![ft()] };
        let facts = batch_facts(&expr, &schema);
        if shape < 3 {
            assert_eq!(facts.node_count(), 1);
            assert!(facts.call_sites().is_empty());
        }
        let mut program = compile_batch(&expr, &schema);
        assert_eq!(program.return_type(), &ft());
        let state = ExecutionLimits::default();
        for count in [0, 1, 1024, 1025] {
            let mut services = Bindings::new(if shape < 2 {
                vec![]
            } else {
                vec![(0..1025).map(|n| Some(i64::from(n))).collect()]
            });
            if count == 0 || count == 1025 {
                services.fault = Some((0, Fault::Panic));
            }
            let mut ctx = EvalContext::default();
            warn(&mut ctx, "prior".into());
            let selection: Vec<usize> = (0..count).collect();
            let result = program.eval_with_bindings_reported(
                state,
                &mut ctx,
                1025,
                &selection,
                &mut services,
            );
            if count == 1025 {
                let error = result.unwrap_err();
                assert!(error.site().is_none());
                assert!(services.reads.is_empty());
            } else {
                let expected: Vec<_> = (0..count)
                    .map(|n| match shape {
                        0 => Some(7),
                        1 => None,
                        2 => Some(n as i64),
                        _ => Some(n as i64 + 1),
                    })
                    .collect();
                assert_eq!(result.unwrap().to_int_vec(), expected);
                assert_eq!(services.reads.len(), if shape < 2 { 0 } else { count });
            }
            assert_eq!(ctx.warnings.warning_cnt, 1);
        }
        // The cap is on selected occurrences, not the physical row universe.
        let mut services = Bindings::new(if shape < 2 {
            vec![]
        } else {
            vec![vec![Some(9); 1025]]
        });
        assert!(run_batch(&mut program, &mut services, 1025, &[1024]).is_ok());
    }
}

#[test]
fn numeric_batch_owned_program_rebinds_without_retaining_source_or_values() {
    let mut program = {
        let expr = plus(input(0), constant(Some(1)));
        let facts = batch_facts(&expr, &[ft()]);
        compile_numeric_batch(&expr, &[ft()], LocalCompileContext::default(), &facts).unwrap()
    };
    let mut services = Bindings::new(vec![vec![Some(5), None, Some(-3)]]);
    let state = ExecutionLimits::default();
    let mut ctx = EvalContext::default();
    for (values, expected) in [
        (
            vec![Some(5), None, Some(-3)],
            vec![Some(-2), Some(6), Some(-2)],
        ),
        (vec![Some(-1), Some(8), None], vec![None, Some(0), None]),
    ] {
        services.values[0] = values;
        services.reads.clear();
        let output = program
            .eval_with_bindings(state, &mut ctx, 3, &[2, 0, 2], &mut services)
            .unwrap();
        assert_eq!(output.to_int_vec(), expected);
        assert_eq!(services.reads, [(0, 2, 0), (1, 0, 0), (2, 2, 0)]);
    }
}

#[test]
fn numeric_batch_finishes_both_operand_phases_in_occurrence_order() {
    let mut program = compile_batch(&plus(input(0), input(1)), &[ft(), ft()]);
    let mut services = Bindings::new(vec![
        vec![Some(1), None, Some(4)],
        vec![Some(10), Some(20), Some(40)],
    ]);
    services.warnings = true;
    let mut ctx = EvalContext::default();
    warn(&mut ctx, "prior".into());
    let output = program
        .eval_with_bindings(
            ExecutionLimits::default(),
            &mut ctx,
            3,
            &[2, 0, 2],
            &mut services,
        )
        .unwrap();
    assert_eq!(output.to_int_vec(), [Some(44), Some(11), Some(44)]);
    let expected = [
        (0, 2, 0),
        (1, 0, 0),
        (2, 2, 0),
        (0, 2, 1),
        (1, 0, 1),
        (2, 2, 1),
    ];
    assert_eq!(services.reads, expected);
    assert_eq!(ctx.warnings.warning_cnt, 7);
    assert!(ctx.warnings.warnings[0].get_msg().ends_with("prior"));
    for (warning, (occurrence, physical, slot)) in ctx.warnings.warnings[1..].iter().zip(expected) {
        assert!(
            warning
                .get_msg()
                .ends_with(&format!("read:{occurrence}:{physical}:{slot}"))
        );
    }
}

#[test]
fn numeric_batch_null_left_demands_right_even_for_one_occurrence() {
    let expr = plus(input(0), input(1));
    let mut program = compile_batch(&expr, &[ft(), ft()]);
    let mut services = Bindings::new(vec![vec![None], vec![Some(9)]]);
    services.fault = Some((1, Fault::Binding));
    let error = program
        .eval_with_bindings_reported(
            ExecutionLimits::default(),
            &mut EvalContext::default(),
            1,
            &[0],
            &mut services,
        )
        .unwrap_err();
    assert_eq!(error.stage(), LocalFailureStage::Input);
    assert_eq!(
        error.site(),
        Some(&LocalFailureSite::InputSlot {
            slot: 1,
            row: InputRow {
                occurrence: 0,
                input_row: 0
            },
        })
    );
    assert_eq!(services.reads, [(0, 0, 0), (0, 0, 1)]);
    // The separate row route keeps its historical NULL-stop behavior.
    let mut row_program = compile(&expr, &[ft(), ft()], OrdinaryProfile::TypedRow);
    services.reads.clear();
    assert_eq!(
        run(&mut row_program, &mut services, 1, &[0])
            .unwrap()
            .to_int_vec(),
        [None]
    );
    assert_eq!(services.reads, [(0, 0, 0)]);
    services.fault = None;
    services.reads.clear();
    assert_eq!(
        run_batch(&mut program, &mut services, 1, &[0, 0, 0])
            .unwrap()
            .to_int_vec(),
        [None; 3]
    );
    assert_eq!(
        services.reads,
        [
            (0, 0, 0),
            (1, 0, 0),
            (2, 0, 0),
            (0, 0, 1),
            (1, 0, 1),
            (2, 0, 1)
        ]
    );
}

#[test]
fn numeric_batch_null_left_does_not_hide_a_nested_right_kernel_error() {
    let expr = plus(constant(None), plus(input(0), constant(Some(1))));
    let mut program = compile_batch(&expr, &[ft()]);
    let mut services = Bindings::new(vec![vec![Some(i64::MAX)]]);
    let error = program
        .eval_with_bindings_reported(
            ExecutionLimits::default(),
            &mut EvalContext::default(),
            1,
            &[0],
            &mut services,
        )
        .unwrap_err();
    assert_eq!(error.stage(), LocalFailureStage::Kernel);
    assert_eq!(error.site(), Some(&batch_kernel_site(2, 0, 0)));
    assert_eq!(services.reads, [(0, 0, 0)]);
}

#[test]
fn numeric_batch_late_right_child_error_beats_early_parent_overflow() {
    // Parent lane 0 would overflow, but the entire right child must finish first.
    let expr = plus(input(0), plus(input(1), constant(Some(1))));
    let mut program = compile_batch(&expr, &[ft(), ft()]);
    let mut services = Bindings::new(vec![
        vec![Some(i64::MAX), Some(0)],
        vec![Some(0), Some(i64::MAX)],
    ]);
    services.warnings = true;
    let mut ctx = EvalContext::default();
    let error = program
        .eval_with_bindings_reported(
            ExecutionLimits::default(),
            &mut ctx,
            2,
            &[0, 1],
            &mut services,
        )
        .unwrap_err();
    assert_eq!(error.stage(), LocalFailureStage::Kernel);
    assert_eq!(error.site(), Some(&batch_kernel_site(2, 1, 1)));
    assert_eq!(services.reads, [(0, 0, 0), (1, 1, 0), (0, 0, 1), (1, 1, 1)]);
    assert_eq!(ctx.warnings.warning_cnt, 4);
}

#[test]
fn numeric_batch_later_left_child_error_precedes_every_right_child_effect() {
    // The in-cap form of the 1025 root-tiling counterexample: a root row loop
    // would fail in the right child at occurrence 0 instead of left at 1.
    let expr = plus(
        plus(input(0), constant(Some(1))),
        plus(input(1), constant(Some(1))),
    );
    let mut program = compile_batch(&expr, &[ft(), ft()]);
    let mut services = Bindings::new(vec![
        vec![Some(0), Some(i64::MAX)],
        vec![Some(i64::MAX), Some(0)],
    ]);
    services.fault = Some((1, Fault::Panic));
    services.warnings = true;
    let mut ctx = EvalContext::default();
    let error = program
        .eval_with_bindings_reported(
            ExecutionLimits::default(),
            &mut ctx,
            2,
            &[0, 1],
            &mut services,
        )
        .unwrap_err();
    assert_eq!(error.stage(), LocalFailureStage::Kernel);
    assert_eq!(error.site(), Some(&batch_kernel_site(1, 1, 1)));
    assert_eq!(services.reads, [(0, 0, 0), (1, 1, 0)]);
    assert_eq!(ctx.warnings.warning_cnt, 2);
}

#[test]
fn numeric_batch_parent_kernel_error_follows_all_reads_without_replay() {
    let mut program = compile_batch(&plus(input(0), input(1)), &[ft(), ft()]);
    let mut services = Bindings::new(vec![vec![Some(i64::MAX), Some(2)], vec![Some(1), Some(3)]]);
    services.warnings = true;
    let mut ctx = EvalContext::default();
    warn(&mut ctx, "prior".into());
    let error = program
        .eval_with_bindings_reported(
            ExecutionLimits::default(),
            &mut ctx,
            2,
            &[0, 1],
            &mut services,
        )
        .unwrap_err();
    assert_eq!(error.stage(), LocalFailureStage::Kernel);
    assert_eq!(error.site(), Some(&batch_kernel_site(0, 0, 0)));
    assert!(matches!(error.error(), LocalError::Evaluation(_)));
    assert_eq!(services.reads, [(0, 0, 0), (1, 1, 0), (0, 0, 1), (1, 1, 1)]);
    assert_eq!(ctx.warnings.warning_cnt, 5);
}

#[test]
fn numeric_batch_repeated_selection_kernel_site_keeps_occurrence_and_physical_row() {
    let mut program = compile_batch(&plus(input(0), constant(Some(1))), &[ft()]);
    let mut services = Bindings::new(vec![vec![Some(i64::MAX), None, Some(2)]]);
    let report = program
        .eval_with_bindings_reported(
            ExecutionLimits::default(),
            &mut EvalContext::default(),
            3,
            &[2, 0, 2],
            &mut services,
        )
        .unwrap_err();
    assert_eq!(report.site(), Some(&batch_kernel_site(0, 1, 0)));
    assert_eq!(services.reads, [(0, 2, 0), (1, 0, 0), (2, 2, 0)]);
    drop(program);
    drop(services);
    assert_eq!(report.stage(), LocalFailureStage::Kernel);
    assert_eq!(report.site(), Some(&batch_kernel_site(0, 1, 0)));
}

#[test]
fn numeric_batch_repeated_input_error_has_exact_coordinates_and_no_stale_site() {
    let mut program = compile_batch(&plus(input(0), input(1)), &[ft(), ft()]);
    let mut services = BatchBindings::new(vec![vec![Some(1); 3], vec![Some(2); 3]]);
    services.fault_on = Some((1, 2, Fault::Resource));
    services.inner.warnings = true;
    let state = ExecutionLimits::default();
    let mut ctx = EvalContext::default();
    let report = program
        .eval_with_bindings_reported(state, &mut ctx, 3, &[2, 0, 2], &mut services)
        .unwrap_err();
    assert!(matches!(report.error(), LocalError::ResourceLimit(_)));
    assert_eq!(report.stage(), LocalFailureStage::Input);
    assert_eq!(
        report.site(),
        Some(&LocalFailureSite::InputSlot {
            slot: 1,
            row: InputRow {
                occurrence: 2,
                input_row: 2
            },
        })
    );
    assert_eq!(
        services.inner.reads,
        [
            (0, 2, 0),
            (1, 0, 0),
            (2, 2, 0),
            (0, 2, 1),
            (1, 0, 1),
            (2, 2, 1)
        ]
    );
    assert_eq!(ctx.warnings.warning_cnt, 6);
    services.fault_on = None;
    services.inner.reads.clear();
    assert_eq!(
        program
            .eval_with_bindings_reported(state, &mut ctx, 3, &[2, 0, 2], &mut services)
            .unwrap()
            .to_int_vec(),
        [Some(3); 3]
    );
    let before = ctx.warnings.warning_cnt;
    services.inner.reads.clear();
    let error = program
        .eval_with_bindings_reported(state, &mut ctx, 3, &[3], &mut services)
        .unwrap_err();
    assert_eq!(error.stage(), LocalFailureStage::Validation);
    assert!(error.site().is_none());
    assert!(services.inner.reads.is_empty());
    assert_eq!(ctx.warnings.warning_cnt, before);
    assert_eq!(report.stage(), LocalFailureStage::Input);
}

#[test]
fn numeric_batch_input_errors_and_malformed_successes_preserve_warning_prefixes() {
    for fault in [
        Fault::Binding,
        Fault::Resource,
        Fault::Evaluation,
        Fault::WrongType,
        Fault::WrongLength,
    ] {
        let mut program = compile_batch(&plus(input(0), input(1)), &[ft(), ft()]);
        let mut services = BatchBindings::new(vec![vec![Some(1); 3], vec![Some(2); 3]]);
        services.fault_on = Some((1, 1, fault));
        services.inner.warnings = true;
        let mut ctx = EvalContext::default();
        warn(&mut ctx, "prior".into());
        let error = program
            .eval_with_bindings_reported(
                ExecutionLimits::default(),
                &mut ctx,
                3,
                &[2, 0, 2],
                &mut services,
            )
            .unwrap_err();
        match fault {
            Fault::Resource => assert!(matches!(error.error(), LocalError::ResourceLimit(_))),
            Fault::Evaluation => assert!(matches!(error.error(), LocalError::Evaluation(_))),
            _ => assert!(matches!(error.error(), LocalError::BindingContract(_))),
        }
        if matches!(fault, Fault::WrongType | Fault::WrongLength) {
            assert_eq!(error.stage(), LocalFailureStage::Validation);
            assert!(error.site().is_none());
        } else {
            assert_eq!(error.stage(), LocalFailureStage::Input);
            assert_eq!(
                error.site(),
                Some(&LocalFailureSite::InputSlot {
                    slot: 1,
                    row: InputRow {
                        occurrence: 1,
                        input_row: 0
                    },
                })
            );
            assert!(error.to_string().contains("primary"));
        }
        let expected = [(0, 2, 0), (1, 0, 0), (2, 2, 0), (0, 2, 1), (1, 0, 1)];
        assert_eq!(services.inner.reads, expected);
        assert_eq!(ctx.warnings.warning_cnt, 6);
        assert!(ctx.warnings.warnings[0].get_msg().ends_with("prior"));
        for (warning, (occurrence, physical, slot)) in
            ctx.warnings.warnings[1..].iter().zip(expected)
        {
            assert!(
                warning
                    .get_msg()
                    .ends_with(&format!("read:{occurrence}:{physical}:{slot}"))
            );
        }
    }
}

#[test]
fn numeric_batch_schema_and_selection_preflight_stays_effect_free_when_empty() {
    let mut program = compile_batch(&plus(input(0), constant(Some(1))), &[ft(), ft()]);
    let mut services = Bindings::new(vec![vec![None], vec![Some(4)]]);
    services.fault = Some((0, Fault::Panic));
    services.warnings = true;
    let state = ExecutionLimits::default();
    let mut ctx = EvalContext::default();
    warn(&mut ctx, "prior".into());
    for change in 0..4 {
        services.schema = vec![ft(), ft()];
        match change {
            0 => services.schema[1].set_flen(7),
            1 => services.schema[1].set_array(true),
            2 => services.schema[1].set_flag(1 << 25),
            _ => {
                services.schema.pop();
            }
        }
        for selection in [&[][..], &[0][..]] {
            let error = program
                .eval_with_bindings_reported(state, &mut ctx, 1, selection, &mut services)
                .unwrap_err();
            assert!(matches!(error.error(), LocalError::InvalidBatch(_)));
            assert_eq!(error.stage(), LocalFailureStage::Validation);
            assert!(error.site().is_none());
        }
    }
    services.schema = vec![ft(), ft()];
    let error = program
        .eval_with_bindings_reported(state, &mut ctx, 1, &[1], &mut services)
        .unwrap_err();
    assert_eq!(error.stage(), LocalFailureStage::Validation);
    assert!(error.site().is_none());
    assert!(services.reads.is_empty());
    assert_eq!(ctx.warnings.warning_cnt, 1);
    assert!(
        program
            .eval_with_bindings_reported(state, &mut ctx, 1, &[], &mut services)
            .unwrap()
            .is_empty()
    );
    assert!(services.reads.is_empty());
}

#[test]
fn numeric_batch_zero_limits_precede_reads_and_no_host_task_reservation_is_needed() {
    let mut program = compile_batch(&plus(input(0), input(1)), &[ft(), ft()]);
    for limits in [
        ExecutionLimits {
            max_steps: 0,
            ..ExecutionLimits::default()
        },
        ExecutionLimits {
            max_frame_depth: 1,
            ..ExecutionLimits::default()
        },
        ExecutionLimits {
            max_retained_bytes: 0,
            ..ExecutionLimits::default()
        },
    ] {
        let mut services = Bindings::new(vec![vec![Some(1)], vec![Some(2)]]);
        services.fault = Some((0, Fault::Panic));
        let error = program
            .eval_with_bindings_reported(
                limits,
                &mut EvalContext::default(),
                1,
                &[0],
                &mut services,
            )
            .unwrap_err();
        assert!(matches!(error.error(), LocalError::ResourceLimit(_)));
        assert_eq!(error.stage(), LocalFailureStage::Resource);
        assert!(error.site().is_none());
        assert!(services.reads.is_empty());
    }
    let mut services = Bindings::new(vec![vec![Some(1)], vec![Some(2)]]);
    assert_eq!(
        program
            .eval_with_bindings(
                ExecutionLimits {
                    max_active_tasks: 0,
                    ..ExecutionLimits::default()
                },
                &mut EvalContext::default(),
                1,
                &[0],
                &mut services,
            )
            .unwrap()
            .to_int_vec(),
        [Some(3)]
    );
}

#[test]
fn numeric_batch_work_budget_is_shared_across_occurrences_and_fresh_per_invocation() {
    let mut program = compile_batch(&plus(input(0), input(1)), &[ft(), ft()]);
    let mut services = Bindings::new(vec![vec![Some(1); 1024], vec![Some(2); 1024]]);
    let state = ExecutionLimits {
        max_steps: 256,
        ..ExecutionLimits::default()
    };
    let mut ctx = EvalContext::default();
    assert_eq!(
        program
            .eval_with_bindings(state, &mut ctx, 1024, &[0], &mut services)
            .unwrap()
            .to_int_vec(),
        [Some(3)]
    );
    services.reads.clear();
    let selection: Vec<usize> = (0..1024).collect();
    let error = program
        .eval_with_bindings_reported(state, &mut ctx, 1024, &selection, &mut services)
        .unwrap_err();
    assert_eq!(error.stage(), LocalFailureStage::Resource);
    assert!(error.site().is_none());
    assert!(services.reads.len() < 2048);
    services.reads.clear();
    assert_eq!(
        program
            .eval_with_bindings_reported(state, &mut ctx, 1024, &[7], &mut services)
            .unwrap()
            .to_int_vec(),
        [Some(3)]
    );
    assert_eq!(services.reads, [(0, 7, 0), (0, 7, 1)]);
}

#[test]
fn numeric_batch_provider_spare_capacity_is_charged_after_read_before_next_effect() {
    use tidb_query_datatype::{EvalType, codec::data_type::ChunkedVec};

    struct SpareCapacity {
        schema: Vec<FieldType>,
        reads: Vec<(usize, InputRow)>,
    }
    impl LocalRuntimeServices for SpareCapacity {
        fn binding_schema(&self) -> &[FieldType] {
            &self.schema
        }
        fn read_input(
            &mut self,
            _: &mut EvalContext,
            slot: usize,
            row: InputRow,
            expected: &FieldType,
        ) -> LocalResult<VectorValue> {
            assert_eq!(expected, &self.schema[slot]);
            self.reads.push((slot, row));
            assert_eq!(
                slot, 0,
                "oversized prior reply must prevent the next effect"
            );
            let mut value = VectorValue::with_capacity(128 * 1024, EvalType::Int);
            let VectorValue::Int(values) = &mut value else {
                unreachable!()
            };
            values.push_data(1);
            Ok(value)
        }
        fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
            panic!("numeric batch must not consult host services")
        }
    }
    let mut services = SpareCapacity {
        schema: vec![ft(), ft()],
        reads: vec![],
    };
    let mut program = compile_batch(&plus(input(0), input(1)), &[ft(), ft()]);
    let error = program
        .eval_with_bindings_reported(
            ExecutionLimits {
                max_retained_bytes: 64 * 1024,
                ..ExecutionLimits::default()
            },
            &mut EvalContext::default(),
            1,
            &[0],
            &mut services,
        )
        .unwrap_err();
    assert_eq!(error.stage(), LocalFailureStage::Resource);
    assert!(error.site().is_none());
    assert_eq!(
        services.reads,
        [(
            0,
            InputRow {
                occurrence: 0,
                input_row: 0
            }
        )]
    );
}

#[test]
fn numeric_batch_input_unwind_does_not_poison_reused_program_state_or_reports() {
    let mut program = compile_batch(&plus(input(0), input(1)), &[ft(), ft()]);
    let mut services = BatchBindings::new(vec![vec![Some(5); 2], vec![Some(7); 2]]);
    services.fault_on = Some((1, 1, Fault::Panic));
    let state = ExecutionLimits::default();
    let mut ctx = EvalContext::default();
    let panic = catch_unwind(AssertUnwindSafe(|| {
        program.eval_with_bindings_reported(state, &mut ctx, 2, &[0, 1], &mut services)
    }))
    .unwrap_err();
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
    assert_eq!(message, Some("primary read panic"));
    assert_eq!(
        services.inner.reads,
        [(0, 0, 0), (1, 1, 0), (0, 0, 1), (1, 1, 1)]
    );
    services.fault_on = None;
    services.inner.reads.clear();
    assert_eq!(
        program
            .eval_with_bindings_reported(state, &mut ctx, 2, &[1, 0], &mut services)
            .unwrap()
            .to_int_vec(),
        [Some(12); 2]
    );
    assert_eq!(
        services.inner.reads,
        [(0, 1, 0), (1, 0, 0), (0, 1, 1), (1, 0, 1)]
    );
}

#[test]
fn numeric_batch_equal_source_ids_do_not_merge_distinct_subtree_occurrences() {
    let expr = plus(
        plus(input(0), constant(Some(1))),
        plus(input(0), constant(Some(1))),
    );
    let facts = NumericBatchFacts::sql_native_numeric_batch(
        &expr,
        &[ft()],
        [0, 1, 4]
            .into_iter()
            .map(|ordinal| OrdinaryCallSite::sql_native_numeric_batch(ordinal, source(99)))
            .collect(),
        CompileLimits::default(),
    )
    .unwrap();
    let mut program =
        compile_numeric_batch(&expr, &[ft()], LocalCompileContext::default(), &facts).unwrap();
    let mut services = Bindings::new(vec![vec![Some(3), Some(7)]]);
    assert_eq!(
        run_batch(&mut program, &mut services, 2, &[1, 0])
            .unwrap()
            .to_int_vec(),
        [Some(16), Some(8)]
    );
    assert_eq!(services.reads, [(0, 1, 0), (1, 0, 0), (0, 1, 0), (1, 0, 0)]);
}

#[test]
fn numeric_batch_deep_snapshots_compile_execution_and_drop_are_iterative() {
    thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn_wrapper(|| {
            for depth in [33, 64, 256] {
                let mut expr = input(0);
                for _ in 0..depth {
                    expr = plus(expr, constant(Some(0)));
                }
                let limits = CompileLimits {
                    max_nodes: depth * 2 + 1,
                    max_depth: depth + 1,
                };
                let facts = NumericBatchFacts::sql_native_numeric_batch(
                    &expr,
                    &[ft()],
                    batch_sites(&expr),
                    limits,
                )
                .unwrap();
                assert_eq!(facts.node_count(), depth * 2 + 1);
                assert_eq!(facts.call_sites().len(), depth);
                let detached = facts.clone();
                assert!(format!("{detached:?}").contains("NativeNumericBatch"));
                detached.validate(&expr, &[ft()], limits).unwrap();
                let mut program =
                    compile_numeric_batch(&expr, &[ft()], LocalCompileContext { limits }, &facts)
                        .unwrap();
                let mut services = Bindings::new(vec![vec![Some(1), Some(7), Some(11)]]);
                assert_eq!(
                    run_batch(&mut program, &mut services, 3, &[2, 0, 2])
                        .unwrap()
                        .to_int_vec(),
                    [Some(11), Some(1), Some(11)]
                );
                assert_eq!(services.reads, [(0, 2, 0), (1, 0, 0), (2, 2, 0)]);
                drop(program);
                drop(detached);
                drop(facts);
                drop(expr);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn numeric_batch_leaf_dispatch_rejects_old_row_entries_even_when_empty() {
    for expr in [constant(Some(7)), input(0)] {
        let is_input = matches!(&expr, LocalExpr::InputSlot { .. });
        let schema = if is_input { vec![ft()] } else { vec![] };
        let mut program = compile_batch(&expr, &schema);
        for count in [0, 1] {
            let selection: Vec<usize> = (0..count).collect();
            let mut services = Bindings::new(if is_input {
                vec![vec![Some(5); count]]
            } else {
                vec![]
            });
            services.fault = Some((0, Fault::Panic));
            services.warnings = true;
            let state = ExecutionLimits::default();
            let mut ctx = EvalContext::default();

            // Exercise real dispatch after valid schema/selection preflight,
            // not just ProgramEntry comparison. Leaf roots have no call site
            // from which the old route could infer their batch-only admission.
            assert!(matches!(
                program
                    .inner
                    .eval_with_bindings(state, &mut ctx, count, &selection, &mut services,),
                Err(LocalError::InvalidSpec(_))
            ));
            let report = program
                .inner
                .eval_with_bindings_reported(state, &mut ctx, count, &selection, &mut services)
                .unwrap_err();
            assert!(matches!(report.error(), LocalError::InvalidSpec(_)));
            assert_eq!(report.stage(), LocalFailureStage::Validation);
            assert!(report.site().is_none());

            let columns = LazyBatchColumnVec::from(if is_input {
                vec![VectorValue::from_scalar(&ScalarValue::Int(Some(5)), count)]
            } else {
                vec![]
            });
            assert!(matches!(
                program.inner.eval(
                    state,
                    &mut ctx,
                    LocalBatch {
                        columns: &columns,
                        physical_rows: count,
                        selection: &selection,
                    },
                ),
                Err(LocalError::InvalidSpec(_))
            ));
            assert!(services.reads.is_empty());
            assert_eq!(ctx.warnings.warning_cnt, 0);
            assert!(ctx.warnings.warnings.is_empty());

            // Refused inner routes must not poison the actual opaque facade.
            // Bindings also panics if any route consults the optional host hook.
            services.fault = None;
            services.warnings = false;
            let expected = vec![Some(if is_input { 5 } else { 7 }); count];
            for reported in [false, true] {
                services.reads.clear();
                let output = if reported {
                    program
                        .eval_with_bindings_reported(
                            state,
                            &mut ctx,
                            count,
                            &selection,
                            &mut services,
                        )
                        .unwrap()
                } else {
                    program
                        .eval_with_bindings(state, &mut ctx, count, &selection, &mut services)
                        .unwrap()
                };
                assert_eq!(output.to_int_vec(), expected);
                assert_eq!(
                    services.reads,
                    if is_input {
                        selection
                            .iter()
                            .enumerate()
                            .map(|(occurrence, &physical)| (occurrence, physical, 0))
                            .collect::<Vec<_>>()
                    } else {
                        vec![]
                    },
                );
                assert_eq!(ctx.warnings.warning_cnt, 0);
                assert!(ctx.warnings.warnings.is_empty());
            }
        }
    }
}
