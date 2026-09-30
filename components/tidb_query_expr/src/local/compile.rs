// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use tidb_query_datatype::codec::data_type::ScalarValue;
use tipb::{FieldType, ScalarFuncSig};

use super::{
    CheckedResultFlow, ControlLineageFacts, EvaluatedArgsRole, EvaluatedBytesOp,
    EvaluatedKernelKind, HostCatalog, HostCatalogKey, LocalCompileContext, LocalControlProgram,
    LocalError, LocalExpr, LocalResult, NumericBatchFacts, OrdinaryProfileSpec, PreparedHostCall,
    PreparedOrdinaryCall, registry,
};
use crate::{
    FunctionRef, RpnExpression, RpnExpressionNode,
    types::function::{
        CallArg, CallBuild, CallShape, ControlKind, PreparedCall, prepare_call,
        prepare_selected_call,
    },
};

/// Compiled ownership domain, not inferred from a root node or its annotations.
/// In particular, a numeric-batch leaf has neither an Ordinary call site nor a
/// lineage tag, but still must not enter the row evaluator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProgramEntry {
    Row,
    ControlLineage,
    SqlNumericBatch,
    EvaluatedAscii,
    EvaluatedBytes,
}

/// Worker-owned compiled RPN. Any+Send metadata intentionally prevents an
/// implicit Sync promise. No public accessor can mutate its validated nodes.
#[derive(Debug)]
pub struct LocalProgram {
    pub(super) expression: RpnExpression,
    pub(super) schema: Vec<FieldType>,
    pub(super) host_catalog: Option<HostCatalogKey>,
    return_type: FieldType,
    entry: ProgramEntry,
}

impl LocalProgram {
    pub fn return_type(&self) -> &FieldType {
        &self.return_type
    }

    /// Routes check this after pure preflight and before any effect, including
    /// for empty selections. No public getter can turn it into a consumer knob.
    pub(super) fn check_entry(&self, expected: ProgramEntry) -> LocalResult<()> {
        if self.entry != expected {
            return Err(LocalError::InvalidSpec(
                "compiled program entry differs from the requested evaluation domain".into(),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn into_expression_for_test(self) -> RpnExpression {
        self.expression
    }
}

/// Opaque ownership facade for checked SQL numeric-batch evaluation. It has no
/// raw-RPN, LocalProgram, decoded or row-evaluation escape. The binding methods
/// in the shared batch module select the operand-major execution domain.
#[derive(Debug)]
pub struct LocalNumericBatchProgram {
    pub(super) inner: LocalProgram,
}

impl LocalNumericBatchProgram {
    pub fn return_type(&self) -> &FieldType {
        self.inner.return_type()
    }
}

/// Fresh scalar-only transport ABI, not a native SQL or wire descriptor. Do not
/// populate charset, element or unknown-field containers: the closed worker's
/// owner accounting relies on construction, not equality, for zero type heap.
pub(super) fn evaluated_ascii_bytes_type() -> FieldType {
    FieldType::from(tidb_query_datatype::FieldTypeTp::Blob)
}

pub(super) fn evaluated_ascii_int_type() -> FieldType {
    FieldType::from(tidb_query_datatype::FieldTypeTp::LongLong)
}

fn invalid(error: tidb_query_common::Error) -> LocalError {
    LocalError::InvalidSpec(error.to_string())
}

fn evaluated_bytes_error(
    operation: EvaluatedBytesOp,
    ascii: &'static str,
    bytes: &'static str,
) -> LocalError {
    // The existing ASCII entry retains its diagnostics as well as its guards.
    LocalError::InvalidSpec(
        if operation == EvaluatedBytesOp::Ascii {
            ascii
        } else {
            bytes
        }
        .into(),
    )
}

fn check_evaluated_bytes_source(
    operation: EvaluatedBytesOp,
    spec: &LocalExpr,
    schema: &[FieldType],
) -> LocalResult<()> {
    let arity = operation.input_types().len();
    let calls = operation.call_count();
    if !matches!(spec, LocalExpr::Call { .. }) {
        return Err(evaluated_bytes_error(
            operation,
            "evaluated ASCII requires its fixed value-call source",
            "evaluated Bytes requires its fixed value-call source",
        ));
    }
    let invalid_shape = || {
        evaluated_bytes_error(
            operation,
            "evaluated ASCII requires only canonical slot0 Bytes to Int with no metadata",
            "evaluated Bytes requires its selected nested calls on ordered canonical slots with no metadata",
        )
    };
    let arity_matches = match (operation, operation.input_role()) {
        (EvaluatedBytesOp::PiRaw, EvaluatedArgsRole::NoArgs) => arity == 0 && calls == 1,
        (EvaluatedBytesOp::PiRaw, _) | (_, EvaluatedArgsRole::NoArgs) => false,
        _ => (1..=3).contains(&arity),
    };
    if !arity_matches
        || !(1..=2).contains(&calls)
        || schema.len() != arity
        || schema
            .iter()
            .enumerate()
            .any(|(slot, field_type)| operation.input_field_type(slot).as_ref() != Some(field_type))
        || spec.field_type() != &operation.return_type()
    {
        return Err(invalid_shape());
    }
    let mut current = spec;
    for index in (0..calls).rev() {
        let primitive = operation.call_operation(index).ok_or_else(&invalid_shape)?;
        let LocalExpr::Call {
            function,
            args,
            return_type,
            metadata,
        } = current
        else {
            return Err(invalid_shape());
        };
        if *function != primitive.function_ref()
            || return_type != &primitive.return_type()
            || !matches!(metadata, crate::CallMetadata::None)
            || args.len() != (if index == 0 { arity } else { 1 })
            || args.len() != primitive.input_types().len()
            || args.iter().enumerate().any(|(slot, arg)| {
                primitive.input_field_type(slot).as_ref() != Some(arg.field_type())
            })
        {
            return Err(invalid_shape());
        }
        if index == 0 {
            if args.iter().enumerate().any(|(expected, arg)| {
                !matches!(arg, LocalExpr::InputSlot { slot, field_type }
                    if *slot == expected && Some(field_type) == schema.get(expected))
            }) {
                return Err(invalid_shape());
            }
        } else {
            current = &args[0];
        }
    }
    Ok(())
}

fn check_evaluated_bytes_kernel(
    operation: EvaluatedBytesOp,
    call_index: usize,
    node: &RpnExpressionNode,
) -> LocalResult<()> {
    let primitive = operation.call_operation(call_index).ok_or_else(|| {
        LocalError::InvalidSpec("evaluated operation has no call at this position".into())
    })?;
    let official = primitive.fn_meta();
    if !matches!(
        node,
        RpnExpressionNode::FnCall { func_meta, args_len, field_type, metadata }
            if *args_len == primitive.input_types().len()
                && *args_len == (if call_index == 0 { operation.input_types().len() } else { 1 })
                && func_meta.name == official.name
                && std::ptr::fn_addr_eq(func_meta.fn_ptr, official.fn_ptr)
                && field_type == &primitive.return_type()
                && metadata.is::<()>()
    ) {
        return Err(evaluated_bytes_error(
            operation,
            "evaluated ASCII preparation changed its unary Int/unit-metadata kernel",
            "evaluated Bytes preparation changed its selected arity/canonical/unit-metadata kernel",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum CompileMode<'a> {
    Legacy(Option<&'a HostCatalog>),
    Profiled(&'a OrdinaryProfileSpec),
    Lineaged(&'a ControlLineageFacts),
    NumericBatch(&'a NumericBatchFacts),
    EvaluatedAscii,
    EvaluatedBytes(EvaluatedBytesOp),
}

impl CompileMode<'_> {
    fn entry(self) -> ProgramEntry {
        match self {
            Self::Legacy(_) | Self::Profiled(_) => ProgramEntry::Row,
            Self::Lineaged(_) => ProgramEntry::ControlLineage,
            Self::NumericBatch(_) => ProgramEntry::SqlNumericBatch,
            Self::EvaluatedAscii => ProgramEntry::EvaluatedAscii,
            Self::EvaluatedBytes(_) => ProgramEntry::EvaluatedBytes,
        }
    }

    fn evaluated_bytes_operation(self) -> Option<EvaluatedBytesOp> {
        match self {
            Self::EvaluatedAscii => Some(EvaluatedBytesOp::Ascii),
            Self::EvaluatedBytes(operation) => Some(operation),
            _ => None,
        }
    }

    fn is_lineaged(self) -> bool {
        matches!(self, Self::Lineaged(_))
    }

    fn check_type(self, field_type: &FieldType) -> LocalResult<()> {
        if let Some(operation) = self.evaluated_bytes_operation() {
            // The exact source guard fixes every input slot and the selected
            // final result of the fixed call chain, never a general mixed tree.
            if field_type != &operation.return_type()
                && !(0..operation.input_types().len())
                    .any(|slot| operation.input_field_type(slot).as_ref() == Some(field_type))
            {
                return Err(evaluated_bytes_error(
                    operation,
                    "evaluated ASCII field type differs from its canonical ABI",
                    "evaluated Bytes field type differs from its canonical ABI",
                ));
            }
        } else if !self.is_lineaged() {
            registry::check_signed_int_type(field_type).map_err(invalid)?;
        }
        // The only Lineaged factory revalidates complete source/schema/role
        // facts before entering this compiler. Do not run the legacy signed-Int
        // gate over its already checked Int/Bytes source declarations.
        Ok(())
    }
}

fn check_literal(value: &ScalarValue, mode: CompileMode<'_>) -> LocalResult<()> {
    if mode.is_lineaged() {
        if !matches!(value, ScalarValue::Int(_) | ScalarValue::Bytes(_)) {
            return Err(LocalError::InvalidSpec(
                "lineaged literal representation is not Int or Bytes".into(),
            ));
        }
    } else if !matches!(value, ScalarValue::Int(_)) {
        return Err(LocalError::InvalidSpec(
            "literal representation is not Int".into(),
        ));
    }
    Ok(())
}

fn argument(expr: &LocalExpr, mode: CompileMode<'_>) -> LocalResult<CallArg> {
    mode.check_type(expr.field_type())?;
    Ok(match expr {
        LocalExpr::Constant {
            value,
            field_type,
            literal_kind,
        } => {
            // Reject an invalid, possibly large payload before cloning it into
            // a shallow descriptor, not only when later emitting the leaf.
            check_literal(value, mode)?;
            CallArg::constant(value.clone(), field_type.clone(), *literal_kind)
        }
        _ => CallArg::dynamic(expr.field_type().clone()),
    })
}

fn add_buffer(
    buffers: &mut Vec<Vec<RpnExpressionNode>>,
    result_flows: &mut Vec<Option<CheckedResultFlow>>,
) -> usize {
    let index = buffers.len();
    buffers.push(Vec::new());
    result_flows.push(None);
    index
}

fn take_expression(
    buffers: &mut [Vec<RpnExpressionNode>],
    result_flows: &mut [Option<CheckedResultFlow>],
    index: usize,
    mode: CompileMode<'_>,
) -> LocalResult<RpnExpression> {
    let expression = RpnExpression::from(std::mem::take(&mut buffers[index]));
    if matches!(mode, CompileMode::NumericBatch(_)) && expression.len() != 1 {
        return Err(LocalError::InvalidSpec(
            "numeric-batch compilation requires one node per structured subprogram".into(),
        ));
    }
    if let Some(operation) = mode.evaluated_bytes_operation() {
        let arity = operation.input_types().len();
        if expression.len() != arity + operation.call_count()
            || expression.as_ref()[..arity].iter().enumerate().any(|(slot, node)| {
                !matches!(node, RpnExpressionNode::ColumnRef { offset } if *offset == slot)
            })
        {
            return Err(evaluated_bytes_error(
                operation,
                "evaluated ASCII compilation requires exactly slot0 then its unary kernel",
                "evaluated Bytes compilation requires ordered slots then its selected kernel",
            ));
        }
        for (call_index, node) in expression.as_ref()[arity..].iter().enumerate() {
            check_evaluated_bytes_kernel(operation, call_index, node)?;
        }
    }
    match (mode.is_lineaged(), result_flows[index].take()) {
        (true, Some(flow)) => expression.with_result_flow(flow),
        (false, None) => Ok(expression),
        _ => Err(LocalError::InvalidSpec(
            "compiled result-flow annotation differs from its entry mode".into(),
        )),
    }
}

/// Validate the selector's control tag, not select another kernel. The shared
/// prepare_call has already required unit metadata and identity retained args
/// for controls; preserve those guarantees before forming structured children.
fn check_lineaged_control(
    function: FunctionRef,
    arity: usize,
    prepared: &PreparedCall,
    flow: Option<CheckedResultFlow>,
) -> LocalResult<()> {
    let expected_kind = match function {
        FunctionRef::TiPb(ScalarFuncSig::IfInt | ScalarFuncSig::IfString) => ControlKind::If,
        FunctionRef::TiPb(ScalarFuncSig::IfNullInt | ScalarFuncSig::IfNullString) => {
            ControlKind::IfNull
        }
        FunctionRef::TiPb(ScalarFuncSig::CaseWhenInt | ScalarFuncSig::CaseWhenString) => {
            ControlKind::CaseWhen
        }
        FunctionRef::TiPb(ScalarFuncSig::CoalesceInt | ScalarFuncSig::CoalesceString) => {
            ControlKind::Coalesce
        }
        FunctionRef::TiPb(ScalarFuncSig::LogicalAnd) => ControlKind::And,
        FunctionRef::TiPb(ScalarFuncSig::LogicalOr) => ControlKind::Or,
        _ => {
            return Err(LocalError::InvalidSpec(
                "lineaged compilation requires an admitted control signature".into(),
            ));
        }
    };
    let control = prepared.short_circuit_meta().ok_or_else(|| {
        LocalError::InvalidSpec("lineaged call lost its canonical control tag".into())
    })?;
    let flow_matches = if expected_kind.is_logical() {
        matches!(flow, Some(CheckedResultFlow::OwnResult { .. }))
    } else {
        matches!(flow, Some(CheckedResultFlow::PreserveSelected { .. }))
    };
    if FunctionRef::TiPb(control.sig) != function
        || control.kind != expected_kind
        || prepared.retained_args().len() != arity
        || prepared
            .retained_args()
            .iter()
            .enumerate()
            .any(|(position, &index)| position != index)
        || !flow_matches
    {
        return Err(LocalError::InvalidSpec(
            "lineaged control preparation changed its signature, role or argument order".into(),
        ));
    }
    Ok(())
}

enum BuildStep<'a> {
    Visit {
        expr: &'a LocalExpr,
        depth: usize,
        output: usize,
    },
    Emit {
        call: PreparedCall,
        output: usize,
    },
    FinishControl {
        call: PreparedCall,
        output: usize,
        children: Vec<usize>,
    },
    FinishHost {
        prepared: PreparedHostCall,
        output: usize,
        children: Vec<usize>,
    },
    FinishOrdinary {
        prepared: PreparedOrdinaryCall,
        output: usize,
        children: Vec<usize>,
    },
}

pub fn compile_local(
    spec: &LocalExpr,
    schema: &[FieldType],
    cx: LocalCompileContext,
) -> LocalResult<LocalProgram> {
    compile(spec, schema, cx, CompileMode::Legacy(None))
}

/// Compiles closed, catalog-registered host calls into official RPN nodes.
/// The catalog contains immutable type facts, never native sessions or
/// closures.
pub fn compile_local_with_hosts(
    spec: &LocalExpr,
    schema: &[FieldType],
    cx: LocalCompileContext,
    hosts: &HostCatalog,
) -> LocalResult<LocalProgram> {
    compile(spec, schema, cx, CompileMode::Legacy(Some(hosts)))
}

/// Compiles the closed signed PlusInt203 row-profile domain. Facts retain
/// caller assertions and source identities; they do not prove PB ingestion or
/// implement native source-shaped diagnostics. Existing eager entrypoints and
/// host/control admission are not widened by this API.
pub fn compile_local_profiled(
    spec: &LocalExpr,
    schema: &[FieldType],
    cx: LocalCompileContext,
    facts: &OrdinaryProfileSpec,
) -> LocalResult<LocalProgram> {
    facts.validate(spec, schema, cx.limits)?;
    compile(spec, schema, cx, CompileMode::Profiled(facts))
}

/// Compiles the closed SQL TypedRow Int/Bytes control domain with required
/// materialization identity. Facts are revalidated against every source node,
/// including dead branches, before the shared compiler prepares any call.
///
/// This does not authenticate native SQL origin, widen old entrypoints or admit
/// PB, ordinary203 composition, hosts, AST or native batch profiles. The caller
/// retains the matching immutable materialization table and separately bounds
/// source/literal/metadata bytes; CompileLimits is a node/depth policy only.
pub fn compile_control_with_lineage(
    spec: &LocalExpr,
    schema: &[FieldType],
    cx: LocalCompileContext,
    facts: &ControlLineageFacts,
) -> LocalResult<LocalControlProgram> {
    facts.revalidate(spec, schema, cx.limits)?;
    Ok(LocalControlProgram {
        inner: compile(spec, schema, cx, CompileMode::Lineaged(facts))?,
    })
}

/// Compiles the closed signed LongLong PlusInt203 SQL numeric-batch domain.
/// Every source/schema declaration and batch-only call site is revalidated with
/// the current limits before the shared compiler clones descriptors or prepares
/// a call. The opaque binding entry evaluates each complete left operand before
/// its complete right operand, then invokes the same prepared kernel per lane.
///
/// SQL origin and fixed native source kinds remain producer assertions, not
/// authenticated facts. This does not admit PB, AST-value consumers,
/// parameters, controls, hosts, casts or mixed control lineage, or widen the
/// row entrypoints. CompileLimits bounds nodes/depth; source/type metadata
/// bytes remain a caller responsibility. The evaluator separately bounds
/// selection occurrences.
pub fn compile_numeric_batch(
    spec: &LocalExpr,
    schema: &[FieldType],
    cx: LocalCompileContext,
    facts: &NumericBatchFacts,
) -> LocalResult<LocalNumericBatchProgram> {
    facts.validate(spec, schema, cx.limits)?;
    Ok(LocalNumericBatchProgram {
        inner: compile(spec, schema, cx, CompileMode::NumericBatch(facts))?,
    })
}

/// Fixed internal construction for the opaque ready-value worker. No caller
/// descriptor, child expression, source profile or SQL context enters this ABI.
/// The worker owner must prewarm metadata before publishing the prepared
/// worker.
pub(super) fn compile_evaluated_ascii(cx: LocalCompileContext) -> LocalResult<LocalProgram> {
    compile_evaluated_bytes(EvaluatedBytesOp::Ascii, cx)
}

pub(super) fn compile_evaluated_bytes(
    operation: EvaluatedBytesOp,
    cx: LocalCompileContext,
) -> LocalResult<LocalProgram> {
    let schema = (0..operation.input_types().len())
        .map(|slot| {
            operation.input_field_type(slot).ok_or_else(|| {
                LocalError::InvalidSpec("evaluated operation has no canonical input type".into())
            })
        })
        .collect::<LocalResult<Vec<_>>>()?;
    let first = operation
        .call_operation(0)
        .ok_or_else(|| LocalError::InvalidSpec("evaluated operation has no first call".into()))?;
    let mut spec = LocalExpr::Call {
        function: first.function_ref(),
        args: schema
            .iter()
            .enumerate()
            .map(|(slot, field_type)| LocalExpr::InputSlot {
                slot,
                field_type: field_type.clone(),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice(),
        return_type: first.return_type(),
        metadata: crate::CallMetadata::None,
    };
    for index in 1..operation.call_count() {
        let primitive = operation.call_operation(index).ok_or_else(|| {
            LocalError::InvalidSpec("evaluated operation has no call at this position".into())
        })?;
        spec = LocalExpr::Call {
            function: primitive.function_ref(),
            args: vec![spec].into_boxed_slice(),
            return_type: primitive.return_type(),
            metadata: crate::CallMetadata::None,
        };
    }
    let mode = match operation {
        EvaluatedBytesOp::Ascii => CompileMode::EvaluatedAscii,
        _ => CompileMode::EvaluatedBytes(operation),
    };
    compile(&spec, &schema, cx, mode)
}

fn compile(
    spec: &LocalExpr,
    schema: &[FieldType],
    cx: LocalCompileContext,
    mode: CompileMode<'_>,
) -> LocalResult<LocalProgram> {
    if let Some(operation) = mode.evaluated_bytes_operation() {
        // Check the whole closed source before preparing any descriptor. There
        // are only the selected nested calls over ordered slots, even on private
        // entries.
        check_evaluated_bytes_source(operation, spec, schema)?;
    }
    for field_type in schema {
        mode.check_type(field_type)?;
    }
    let mut steps = vec![BuildStep::Visit {
        expr: spec,
        depth: 1,
        output: 0,
    }];
    let mut buffers: Vec<Vec<RpnExpressionNode>> = vec![Vec::new()];
    // One optional annotation belongs to each actual expression buffer. This
    // carries no executable children, and is populated only in lineaged mode.
    let mut result_flows = vec![None];
    let mut visited = 0usize;
    let mut scheduled = 1usize;
    let mut host_catalog = None;
    while let Some(step) = steps.pop() {
        match step {
            BuildStep::Emit { call, output } => {
                let node = call.into_node();
                if let Some(operation) = mode.evaluated_bytes_operation() {
                    let call_index = buffers[output]
                        .len()
                        .checked_sub(operation.input_types().len())
                        .ok_or_else(|| {
                            LocalError::InvalidSpec(
                                "evaluated call precedes its input slots".into(),
                            )
                        })?;
                    check_evaluated_bytes_kernel(operation, call_index, &node)?;
                }
                buffers[output].push(node);
            }
            BuildStep::FinishControl {
                call,
                output,
                children,
            } => {
                let args = children
                    .into_iter()
                    .map(|index| take_expression(&mut buffers, &mut result_flows, index, mode))
                    .collect::<LocalResult<Vec<_>>>()?;
                buffers[output].push(
                    call.into_control(args.into_boxed_slice())
                        .map_err(invalid)?,
                );
            }
            BuildStep::FinishHost {
                prepared,
                output,
                children,
            } => {
                let args = children
                    .into_iter()
                    .map(|index| take_expression(&mut buffers, &mut result_flows, index, mode))
                    .collect::<LocalResult<Vec<_>>>()?
                    .into_boxed_slice();
                buffers[output].push(RpnExpressionNode::HostCall { prepared, args });
            }
            BuildStep::FinishOrdinary {
                prepared,
                output,
                children,
            } => {
                let args = children
                    .into_iter()
                    .map(|index| take_expression(&mut buffers, &mut result_flows, index, mode))
                    .collect::<LocalResult<Vec<_>>>()?
                    .into_boxed_slice();
                buffers[output].push(RpnExpressionNode::OrdinaryFnCall { prepared, args });
            }
            BuildStep::Visit {
                expr,
                depth,
                output,
            } => {
                visited = visited
                    .checked_add(1)
                    .ok_or_else(|| LocalError::ResourceLimit("node count overflow".into()))?;
                if visited > cx.limits.max_nodes || depth > cx.limits.max_depth {
                    return Err(LocalError::ResourceLimit(
                        "construction node/depth budget exceeded".into(),
                    ));
                }
                mode.check_type(expr.field_type())?;
                let ordinal = visited - 1;
                if let CompileMode::Lineaged(facts) = mode {
                    if result_flows[output].is_some() || !buffers[output].is_empty() {
                        return Err(LocalError::InvalidSpec(
                            "lineaged compilation cannot flatten source nodes into one buffer"
                                .into(),
                        ));
                    }
                    // Capture ALL-NODE preorder identity now, never at Finish
                    // after descendants have advanced the traversal counter.
                    result_flows[output] = Some(facts.flow(ordinal).ok_or_else(|| {
                        LocalError::InvalidSpec(
                            "lineage is missing the current producer occurrence".into(),
                        )
                    })?);
                }
                if let LocalExpr::Call { args, .. } | LocalExpr::HostCall { args, .. } = expr {
                    // Bound descriptor/child-buffer allocation before cloning facts.
                    scheduled = scheduled.checked_add(args.len()).ok_or_else(|| {
                        LocalError::ResourceLimit("construction node count overflow".into())
                    })?;
                    if scheduled > cx.limits.max_nodes
                        || (!args.is_empty() && depth >= cx.limits.max_depth)
                    {
                        return Err(LocalError::ResourceLimit(
                            "construction node/depth budget exceeded".into(),
                        ));
                    }
                }
                match expr {
                    LocalExpr::Constant {
                        value, field_type, ..
                    } => {
                        check_literal(value, mode)?;
                        buffers[output].push(RpnExpressionNode::Constant {
                            value: value.clone(),
                            field_type: field_type.clone(),
                        });
                    }
                    LocalExpr::InputSlot { slot, field_type } => {
                        if schema.get(*slot) != Some(field_type) {
                            return Err(LocalError::InvalidSpec(format!(
                                "slot {} is absent or its complete field type differs",
                                slot
                            )));
                        }
                        buffers[output].push(RpnExpressionNode::ColumnRef { offset: *slot });
                    }
                    LocalExpr::HostCall {
                        slot,
                        args,
                        return_type,
                    } => {
                        let CompileMode::Legacy(Some(hosts)) = mode else {
                            return Err(LocalError::InvalidSpec(
                                "host call requires an explicit registered catalog".into(),
                            ));
                        };
                        let arg_types = args
                            .iter()
                            .map(|arg| arg.field_type().clone())
                            .collect::<Vec<_>>();
                        let prepared =
                            PreparedHostCall::prepare(hosts, *slot, &arg_types, return_type)?;
                        host_catalog = Some(*prepared.catalog_key());
                        let children: Vec<_> = args
                            .iter()
                            .map(|_| add_buffer(&mut buffers, &mut result_flows))
                            .collect();
                        steps.push(BuildStep::FinishHost {
                            prepared,
                            output,
                            children: children.clone(),
                        });
                        for (arg, child_output) in args.iter().zip(children).rev() {
                            steps.push(BuildStep::Visit {
                                expr: arg,
                                depth: depth.saturating_add(1),
                                output: child_output,
                            });
                        }
                    }
                    LocalExpr::Call {
                        function,
                        args,
                        return_type,
                        metadata,
                    } => {
                        let shape = CallShape::new(
                            *function,
                            return_type.clone(),
                            args.iter()
                                .map(|arg| argument(arg, mode))
                                .collect::<LocalResult<Vec<_>>>()?,
                        );
                        let site = match mode {
                            CompileMode::Profiled(facts) => {
                                // Profile validation used ALL-node source preorder.
                                // This route retains every argument in that order.
                                Some(
                                    facts
                                        .site(ordinal)
                                        .ok_or_else(|| {
                                            LocalError::InvalidSpec(
                                                "profile is missing the current call occurrence"
                                                    .into(),
                                            )
                                        })?
                                        .clone(),
                                )
                            }
                            CompileMode::NumericBatch(facts) => Some(
                                facts
                                    .site(ordinal)
                                    .ok_or_else(|| {
                                        LocalError::InvalidSpec(
                                            "numeric-batch facts are missing the current call occurrence"
                                                .into(),
                                        )
                                    })?
                                    .clone(),
                            ),
                            CompileMode::Legacy(_) => {
                                registry::check_local_admission(&shape, metadata)
                                    .map_err(invalid)?;
                                None
                            }
                            CompileMode::Lineaged(_)
                            | CompileMode::EvaluatedAscii
                            | CompileMode::EvaluatedBytes(_) => None,
                        };
                        let mut call = CallBuild::local(shape, metadata.clone());
                        let prepared = match mode.evaluated_bytes_operation() {
                            Some(operation)
                                if matches!(
                                    operation.kernel_kind(),
                                    EvaluatedKernelKind::ClosedPrivate(_)
                                ) =>
                            {
                                // The closed source has fixed this private identity.
                                // Reuse both validators and its real metadata constructor.
                                prepare_selected_call(&mut call, operation.fn_meta().into())
                            }
                            _ => prepare_call(&mut call),
                        }
                        .map_err(invalid)?;
                        let mut ready_eager_control = false;
                        if let Some(operation) = mode.evaluated_bytes_operation() {
                            // Only the exact closed Int2 ready recipes may retain a
                            // canonical AND/OR tag while emitting the prepared kernel.
                            let tag_matches = match operation {
                                EvaluatedBytesOp::LogicalAnd => {
                                    args.len() == 2
                                        && prepared.short_circuit_meta().is_some_and(|control| {
                                            control.sig == ScalarFuncSig::LogicalAnd
                                                && control.kind == ControlKind::And
                                        })
                                }
                                EvaluatedBytesOp::LogicalOr => {
                                    args.len() == 2
                                        && prepared.short_circuit_meta().is_some_and(|control| {
                                            control.sig == ScalarFuncSig::LogicalOr
                                                && control.kind == ControlKind::Or
                                        })
                                }
                                _ => prepared.short_circuit_meta().is_none(),
                            };
                            if !prepared.retained_args().iter().copied().eq(0..args.len())
                                || !tag_matches
                            {
                                return Err(evaluated_bytes_error(
                                    operation,
                                    "evaluated ASCII preparation changed its operand or control shape",
                                    "evaluated Bytes preparation changed its operand or control shape",
                                ));
                            }
                            ready_eager_control = matches!(
                                operation,
                                EvaluatedBytesOp::LogicalAnd | EvaluatedBytesOp::LogicalOr
                            );
                        }
                        if mode.is_lineaged() {
                            check_lineaged_control(
                                *function,
                                args.len(),
                                &prepared,
                                result_flows[output],
                            )?;
                        }
                        let retained = prepared.retained_args().to_vec();
                        if let Some(site) = site {
                            let prepared = prepared.into_ordinary(site).map_err(invalid)?;
                            let children: Vec<_> = retained
                                .iter()
                                .map(|_| add_buffer(&mut buffers, &mut result_flows))
                                .collect();
                            steps.push(BuildStep::FinishOrdinary {
                                prepared,
                                output,
                                children: children.clone(),
                            });
                            for (index, child_output) in retained.into_iter().zip(children).rev() {
                                steps.push(BuildStep::Visit {
                                    expr: &args[index],
                                    depth: depth.saturating_add(1),
                                    output: child_output,
                                });
                            }
                        } else if prepared.short_circuit_meta().is_some() && !ready_eager_control {
                            let children: Vec<_> = retained
                                .iter()
                                .map(|_| add_buffer(&mut buffers, &mut result_flows))
                                .collect();
                            steps.push(BuildStep::FinishControl {
                                call: prepared,
                                output,
                                children: children.clone(),
                            });
                            for (index, child_output) in retained.into_iter().zip(children).rev() {
                                steps.push(BuildStep::Visit {
                                    expr: &args[index],
                                    depth: depth.saturating_add(1),
                                    output: child_output,
                                });
                            }
                        } else {
                            if mode.is_lineaged() {
                                return Err(LocalError::InvalidSpec(
                                    "lineaged compilation cannot emit an eager ordinary call"
                                        .into(),
                                ));
                            }
                            // Ready AND/OR have only already-evaluated slots; source
                            // control demand stays on the structured branch above.
                            // Legacy ordinary calls remain eager here.
                            steps.push(BuildStep::Emit {
                                call: prepared,
                                output,
                            });
                            for index in retained.into_iter().rev() {
                                steps.push(BuildStep::Visit {
                                    expr: &args[index],
                                    depth: depth.saturating_add(1),
                                    output,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    if let CompileMode::Lineaged(facts) = mode {
        if visited != facts.node_count() {
            return Err(LocalError::InvalidSpec(
                "lineaged compilation did not retain every source occurrence".into(),
            ));
        }
    }
    if let CompileMode::NumericBatch(facts) = mode {
        if visited != facts.node_count() {
            return Err(LocalError::InvalidSpec(
                "numeric-batch compilation did not retain every source occurrence".into(),
            ));
        }
    }
    if let Some(operation) = mode.evaluated_bytes_operation() {
        if visited != operation.input_types().len() + operation.call_count()
            || host_catalog.is_some()
        {
            return Err(evaluated_bytes_error(
                operation,
                "evaluated ASCII compilation changed its fixed source or attached hosts",
                "evaluated Bytes compilation changed its fixed source or attached hosts",
            ));
        }
    }
    Ok(LocalProgram {
        expression: take_expression(&mut buffers, &mut result_flows, 0, mode)?,
        schema: schema.to_vec(),
        host_catalog,
        return_type: spec.field_type().clone(),
        entry: mode.entry(),
    })
}

#[cfg(test)]
mod lineage_compile_tests {
    use tidb_query_datatype::FieldTypeTp;

    use super::*;
    use crate::local::{
        CompileLimits, ControlProducerFact, LineageCarrier, LiteralKind, OrdinaryCallSite,
        OrdinaryProfile, OrdinarySourceId, ResultMetaId,
    };

    fn constant(value: ScalarValue, field_type: &FieldType) -> LocalExpr {
        LocalExpr::Constant {
            value,
            field_type: field_type.clone(),
            literal_kind: LiteralKind::Typed,
        }
    }

    fn call(signature: ScalarFuncSig, args: Vec<LocalExpr>, field_type: &FieldType) -> LocalExpr {
        LocalExpr::Call {
            function: FunctionRef::TiPb(signature),
            args: args.into_boxed_slice(),
            return_type: field_type.clone(),
            metadata: crate::CallMetadata::None,
        }
    }

    fn id(ordinal: usize) -> ResultMetaId {
        ResultMetaId::new(7, 100 + 11 * ordinal as u64)
    }

    #[test]
    fn compile_lineage_attaches_visit_flow_to_every_actual_subprogram() {
        let int_type: FieldType = FieldTypeTp::LongLong.into();
        let mut bytes_type: FieldType = FieldTypeTp::VarString.into();
        bytes_type.set_collate(63);
        let mut source_type = bytes_type.clone();
        source_type.set_collate(-46);
        let spec = call(
            ScalarFuncSig::IfString,
            vec![
                call(
                    ScalarFuncSig::LogicalAnd,
                    vec![
                        constant(ScalarValue::Int(Some(1)), &int_type),
                        constant(ScalarValue::Int(Some(2)), &int_type),
                    ],
                    &int_type,
                ),
                call(
                    ScalarFuncSig::IfNullString,
                    vec![
                        LocalExpr::InputSlot {
                            slot: 0,
                            field_type: source_type.clone(),
                        },
                        constant(ScalarValue::Bytes(None), &bytes_type),
                    ],
                    &bytes_type,
                ),
                constant(ScalarValue::Bytes(Some(vec![0xff])), &bytes_type),
            ],
            &bytes_type,
        );
        let schema = [source_type];
        let facts = ControlLineageFacts::sql_typed_row(
            &spec,
            &schema,
            vec![
                ControlProducerFact::selected_control(0, id(0), LineageCarrier::Bytes),
                ControlProducerFact::computed_boolean(1, id(1)),
                ControlProducerFact::constant(2, id(2), LineageCarrier::Int),
                ControlProducerFact::constant(3, id(3), LineageCarrier::Int),
                ControlProducerFact::selected_control(4, id(4), LineageCarrier::Bytes),
                ControlProducerFact::input_slot(5, id(5), LineageCarrier::Bytes),
                ControlProducerFact::constant(6, id(6), LineageCarrier::Bytes),
                ControlProducerFact::constant(7, id(7), LineageCarrier::Bytes),
            ],
            CompileLimits::default(),
        )
        .unwrap();
        let program =
            compile_control_with_lineage(&spec, &schema, LocalCompileContext::default(), &facts)
                .unwrap();
        assert_eq!(program.return_type(), &bytes_type);
        let mut pending = vec![&program.inner.expression];
        let mut ordinal = 0;
        while let Some(expression) = pending.pop() {
            assert_eq!(expression.len(), 1);
            assert_eq!(expression.checked_result_flow(), facts.flow(ordinal));
            ordinal += 1;
            match &expression[0] {
                RpnExpressionNode::ShortCircuitFnCall { args, .. } => {
                    pending.extend(args.iter().rev());
                }
                RpnExpressionNode::Constant { .. } | RpnExpressionNode::ColumnRef { .. } => {}
                _ => panic!("lineaged compilation emitted an ordinary or host node"),
            }
        }
        assert_eq!(ordinal, facts.node_count());

        let mut unsigned = int_type;
        unsigned.set_flag(1 << 5);
        let leaf = constant(ScalarValue::Int(Some(-1)), &unsigned);
        let facts = ControlLineageFacts::sql_typed_row(
            &leaf,
            &[],
            vec![ControlProducerFact::constant(0, id(0), LineageCarrier::Int)],
            CompileLimits::default(),
        )
        .unwrap();
        let program =
            compile_control_with_lineage(&leaf, &[], LocalCompileContext::default(), &facts)
                .unwrap();
        assert_eq!(program.return_type(), &unsigned);
        assert_eq!(
            program.inner.expression.checked_result_flow(),
            facts.flow(0)
        );
        assert!(compile_local(&leaf, &[], LocalCompileContext::default()).is_err());
    }

    fn assert_unannotated(expression: &RpnExpression) {
        let mut pending = vec![expression];
        while let Some(expression) = pending.pop() {
            assert!(expression.checked_result_flow().is_none());
            for node in expression.iter() {
                if let RpnExpressionNode::ShortCircuitFnCall { args, .. }
                | RpnExpressionNode::OrdinaryFnCall { args, .. }
                | RpnExpressionNode::HostCall { args, .. } = node
                {
                    pending.extend(args.iter());
                }
            }
        }
    }

    #[test]
    fn compile_legacy_and_profiled_modes_remain_unannotated() {
        let ft: FieldType = FieldTypeTp::LongLong.into();
        let leaf = || constant(ScalarValue::Int(Some(1)), &ft);
        for spec in [
            call(ScalarFuncSig::IfInt, vec![leaf(), leaf(), leaf()], &ft),
            call(
                ScalarFuncSig::PlusIntSignedSigned,
                vec![leaf(), leaf()],
                &ft,
            ),
        ] {
            let program = compile_local(&spec, &[], LocalCompileContext::default()).unwrap();
            assert_unannotated(&program.expression);
        }
        let spec = call(
            ScalarFuncSig::PlusInt,
            vec![
                leaf(),
                call(ScalarFuncSig::PlusInt, vec![leaf(), leaf()], &ft),
            ],
            &ft,
        );
        let facts = OrdinaryProfileSpec::new(
            &spec,
            &[],
            OrdinaryProfile::TypedRow,
            vec![
                OrdinaryCallSite::typed_row(0, OrdinarySourceId::new(9, 0)),
                OrdinaryCallSite::typed_row(2, OrdinarySourceId::new(9, 2)),
            ],
            CompileLimits::default(),
        )
        .unwrap();
        let program =
            compile_local_profiled(&spec, &[], LocalCompileContext::default(), &facts).unwrap();
        assert_unannotated(&program.expression);
    }
}

#[cfg(test)]
mod numeric_batch_compile_tests {
    use tidb_query_datatype::FieldTypeTp;

    use super::*;
    use crate::local::{
        CompileLimits, ControlProducerFact, LineageCarrier, LiteralKind, OrdinaryCallSite,
        OrdinaryProfile, OrdinarySourceId, ResultMetaId,
    };

    fn constant(value: Option<i64>, field_type: &FieldType) -> LocalExpr {
        LocalExpr::Constant {
            value: ScalarValue::Int(value),
            field_type: field_type.clone(),
            literal_kind: LiteralKind::Typed,
        }
    }

    fn input(field_type: &FieldType) -> LocalExpr {
        LocalExpr::InputSlot {
            slot: 0,
            field_type: field_type.clone(),
        }
    }

    fn plus(left: LocalExpr, right: LocalExpr, field_type: &FieldType) -> LocalExpr {
        LocalExpr::Call {
            function: FunctionRef::TiPb(ScalarFuncSig::PlusInt),
            args: vec![left, right].into_boxed_slice(),
            return_type: field_type.clone(),
            metadata: crate::CallMetadata::None,
        }
    }

    fn batch_facts(
        spec: &LocalExpr,
        schema: &[FieldType],
        ordinals: &[usize],
    ) -> NumericBatchFacts {
        NumericBatchFacts::sql_native_numeric_batch(
            spec,
            schema,
            ordinals
                .iter()
                .map(|&ordinal| {
                    OrdinaryCallSite::sql_native_numeric_batch(
                        ordinal,
                        OrdinarySourceId::new(u64::MAX, 1 << 63),
                    )
                })
                .collect(),
            CompileLimits::default(),
        )
        .unwrap()
    }

    fn assert_entry(program: &LocalProgram, expected: ProgramEntry) {
        for entry in [
            ProgramEntry::Row,
            ProgramEntry::ControlLineage,
            ProgramEntry::SqlNumericBatch,
        ] {
            let result = program.check_entry(entry);
            if entry == expected {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(LocalError::InvalidSpec(_))));
            }
        }
    }

    #[test]
    fn compile_numeric_batch_retains_every_call_ordinal_without_lineage_flow() {
        let ft: FieldType = FieldTypeTp::LongLong.into();
        let spec = plus(
            plus(input(&ft), constant(Some(1), &ft), &ft),
            plus(constant(Some(2), &ft), input(&ft), &ft),
            &ft,
        );
        let schema = [ft.clone()];
        let facts = batch_facts(&spec, &schema, &[0, 1, 4]);
        let program =
            compile_numeric_batch(&spec, &schema, LocalCompileContext::default(), &facts).unwrap();
        assert_entry(&program.inner, ProgramEntry::SqlNumericBatch);
        assert_eq!(program.return_type(), &ft);
        assert_eq!(program.inner.expression.node_count(), 7);
        assert_eq!(program.inner.expression.work_count(), 7);
        assert_eq!(program.inner.expression.column_ref_count(), 2);
        assert_eq!(program.inner.expression.referenced_column_offsets(), &[0]);
        let mut pending = vec![&program.inner.expression];
        let mut ordinal = 0;
        let mut call_ordinals = Vec::new();
        while let Some(expression) = pending.pop() {
            assert_eq!(expression.len(), 1);
            assert_eq!(expression.checked_result_flow(), None);
            match &expression[0] {
                RpnExpressionNode::OrdinaryFnCall { prepared, args } => {
                    assert_eq!(
                        prepared.function(),
                        FunctionRef::TiPb(ScalarFuncSig::PlusInt)
                    );
                    assert_eq!(prepared.site(), facts.site(ordinal).unwrap());
                    assert_eq!(
                        prepared.site().profile(),
                        OrdinaryProfile::NativeNumericBatch
                    );
                    assert_eq!(prepared.site().original_pb_signature(), None);
                    assert_eq!(args.len(), 2);
                    call_ordinals.push(ordinal);
                    pending.extend(args.iter().rev());
                }
                RpnExpressionNode::Constant { .. } | RpnExpressionNode::ColumnRef { .. } => {}
                _ => panic!("numeric-batch compiler emitted a non-ordinary executable node"),
            }
            ordinal += 1;
        }
        assert_eq!(call_ordinals, vec![0, 1, 4]);
        assert_eq!(ordinal, facts.node_count());
    }

    #[test]
    fn compile_leaf_entry_tags_distinguish_rows_lineage_and_numeric_batch() {
        let ft: FieldType = FieldTypeTp::LongLong.into();
        let schema = [ft.clone()];
        let hosts = HostCatalog::new(Vec::new()).unwrap();
        for spec in [constant(Some(7), &ft), input(&ft)] {
            let facts = batch_facts(&spec, &schema, &[]);
            let numeric =
                compile_numeric_batch(&spec, &schema, LocalCompileContext::default(), &facts)
                    .unwrap();
            assert_entry(&numeric.inner, ProgramEntry::SqlNumericBatch);
            assert_eq!(numeric.inner.expression.checked_result_flow(), None);
            let row_facts = OrdinaryProfileSpec::new(
                &spec,
                &schema,
                OrdinaryProfile::TypedRow,
                vec![],
                CompileLimits::default(),
            )
            .unwrap();
            for row in [
                compile_local(&spec, &schema, LocalCompileContext::default()).unwrap(),
                compile_local_with_hosts(&spec, &schema, LocalCompileContext::default(), &hosts)
                    .unwrap(),
                compile_local_profiled(&spec, &schema, LocalCompileContext::default(), &row_facts)
                    .unwrap(),
            ] {
                assert_entry(&row, ProgramEntry::Row);
                assert_eq!(row.expression.checked_result_flow(), None);
            }
            let id = ResultMetaId::new(701, 9);
            let producer = match &spec {
                LocalExpr::Constant { .. } => {
                    ControlProducerFact::constant(0, id, LineageCarrier::Int)
                }
                LocalExpr::InputSlot { .. } => {
                    ControlProducerFact::input_slot(0, id, LineageCarrier::Int)
                }
                _ => unreachable!(),
            };
            let lineage_facts = ControlLineageFacts::sql_typed_row(
                &spec,
                &schema,
                vec![producer],
                CompileLimits::default(),
            )
            .unwrap();
            let lineaged = compile_control_with_lineage(
                &spec,
                &schema,
                LocalCompileContext::default(),
                &lineage_facts,
            )
            .unwrap();
            assert_entry(&lineaged.inner, ProgramEntry::ControlLineage);
            assert!(lineaged.inner.expression.checked_result_flow().is_some());
        }
    }

    #[test]
    fn compile_numeric_batch_revalidates_source_schema_and_current_limits() {
        let ft: FieldType = FieldTypeTp::LongLong.into();
        let schema = [ft.clone()];
        let spec = plus(input(&ft), constant(Some(1), &ft), &ft);
        let facts = batch_facts(&spec, &schema, &[0]);
        let changed = plus(input(&ft), constant(Some(2), &ft), &ft);
        assert!(matches!(
            compile_numeric_batch(&changed, &schema, LocalCompileContext::default(), &facts),
            Err(LocalError::InvalidSpec(_))
        ));
        let mut changed_type = ft;
        changed_type.set_flen(123);
        assert!(matches!(
            compile_numeric_batch(
                &spec,
                &[changed_type],
                LocalCompileContext::default(),
                &facts
            ),
            Err(LocalError::InvalidSpec(_))
        ));
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
                compile_numeric_batch(&spec, &schema, LocalCompileContext { limits }, &facts),
                Err(LocalError::ResourceLimit(_))
            ));
        }
    }
}

#[cfg(test)]
mod evaluated_ascii_compile_tests {
    use super::*;
    use crate::local::{CompileLimits, LiteralKind, OrdinaryProfile};

    fn source_with_arg(arg: LocalExpr) -> LocalExpr {
        LocalExpr::Call {
            function: FunctionRef::TiPb(ScalarFuncSig::Ascii),
            args: vec![arg].into_boxed_slice(),
            return_type: evaluated_ascii_int_type(),
            metadata: crate::CallMetadata::None,
        }
    }

    fn source() -> LocalExpr {
        source_with_arg(LocalExpr::InputSlot {
            slot: 0,
            field_type: evaluated_ascii_bytes_type(),
        })
    }

    fn assert_closed_source_rejected(spec: &LocalExpr, schema: &[FieldType]) {
        assert!(matches!(
            compile(
                spec,
                schema,
                LocalCompileContext::default(),
                CompileMode::EvaluatedAscii,
            ),
            Err(LocalError::InvalidSpec(_))
        ));
    }

    #[test]
    fn evaluated_ascii_factory_uses_canonical_two_node_rpn_and_own_entry() {
        let program = compile_evaluated_ascii(LocalCompileContext::default()).unwrap();
        assert_eq!(
            program.schema,
            [FieldType::from(tidb_query_datatype::FieldTypeTp::Blob)]
        );
        assert_eq!(
            program.return_type(),
            &FieldType::from(tidb_query_datatype::FieldTypeTp::LongLong)
        );
        for entry in [
            ProgramEntry::Row,
            ProgramEntry::ControlLineage,
            ProgramEntry::SqlNumericBatch,
        ] {
            assert!(matches!(
                program.check_entry(entry),
                Err(LocalError::InvalidSpec(_))
            ));
        }
        assert!(program.check_entry(ProgramEntry::EvaluatedAscii).is_ok());
        assert!(program.host_catalog.is_none());
        assert_eq!(program.expression.checked_result_flow(), None);
        let [
            RpnExpressionNode::ColumnRef { offset: 0 },
            RpnExpressionNode::FnCall {
                func_meta,
                args_len,
                field_type,
                metadata,
            },
        ] = program.expression.as_ref()
        else {
            panic!("evaluated ASCII did not retain its exact eager two-node shape");
        };
        assert_eq!(func_meta.name, "ascii");
        assert_eq!(*args_len, 1);
        assert_eq!(field_type, &evaluated_ascii_int_type());
        assert!(metadata.is::<()>());
        assert_eq!(program.expression.node_count(), 2);
        assert_eq!(program.expression.work_count(), 2);
        assert_eq!(program.expression.column_ref_count(), 1);
        assert_eq!(program.expression.referenced_column_offsets(), &[0]);
    }

    #[test]
    fn evaluated_ascii_private_mode_rejects_other_sources_and_complete_types() {
        let bytes_type = evaluated_ascii_bytes_type();
        let schema = [bytes_type.clone()];
        let leaf = || LocalExpr::InputSlot {
            slot: 0,
            field_type: bytes_type.clone(),
        };
        let literal = || LocalExpr::Constant {
            value: ScalarValue::Bytes(None),
            field_type: bytes_type.clone(),
            literal_kind: LiteralKind::Typed,
        };
        for spec in [
            leaf(),
            literal(),
            source_with_arg(literal()),
            source_with_arg(source()),
            source_with_arg(LocalExpr::InputSlot {
                slot: 1,
                field_type: bytes_type.clone(),
            }),
            source_with_arg(LocalExpr::InputSlot {
                slot: 0,
                field_type: evaluated_ascii_int_type(),
            }),
        ] {
            assert_closed_source_rejected(&spec, &schema);
        }
        for args in [vec![], vec![leaf(), leaf()]] {
            let mut spec = source();
            if let LocalExpr::Call { args: target, .. } = &mut spec {
                *target = args.into_boxed_slice();
            }
            assert_closed_source_rejected(&spec, &schema);
        }
        let mutations: [fn(&mut LocalExpr); 4] = [
            |spec| {
                if let LocalExpr::Call { function, .. } = spec {
                    *function = FunctionRef::TiPb(ScalarFuncSig::BitLength);
                }
            },
            |spec| {
                if let LocalExpr::Call { metadata, .. } = spec {
                    *metadata = crate::CallMetadata::InUnion { in_union: false };
                }
            },
            |spec| {
                if let LocalExpr::Call { return_type, .. } = spec {
                    return_type.set_flen(3);
                }
            },
            |spec| {
                if let LocalExpr::Call { args, .. } = spec {
                    if let LocalExpr::InputSlot { field_type, .. } = &mut args[0] {
                        field_type.set_array(true);
                    }
                }
            },
        ];
        for mutate in mutations {
            let mut spec = source();
            mutate(&mut spec);
            assert_closed_source_rejected(&spec, &schema);
        }
        let spec = source();
        assert_closed_source_rejected(&spec, &[]);
        assert_closed_source_rejected(&spec, &[bytes_type.clone(), bytes_type.clone()]);
        assert_closed_source_rejected(&spec, &[evaluated_ascii_int_type()]);
        let mut changed_schema = bytes_type;
        changed_schema.set_flen(3);
        assert_closed_source_rejected(&spec, &[changed_schema]);
    }

    #[test]
    fn evaluated_bytes_factory_keeps_selected_operation_and_entry_closed() {
        for operation in [
            EvaluatedBytesOp::Ascii,
            EvaluatedBytesOp::Length,
            EvaluatedBytesOp::BitLength,
            EvaluatedBytesOp::LTrim,
            EvaluatedBytesOp::RTrim,
            EvaluatedBytesOp::UnHex,
            EvaluatedBytesOp::Crc32,
            EvaluatedBytesOp::Reverse,
            EvaluatedBytesOp::ReverseUtf8,
            EvaluatedBytesOp::CharLength,
            EvaluatedBytesOp::CharLengthUtf8,
            EvaluatedBytesOp::Quote,
            EvaluatedBytesOp::HexInt,
            EvaluatedBytesOp::HexStr,
            EvaluatedBytesOp::Bin,
            EvaluatedBytesOp::Left,
            EvaluatedBytesOp::LeftUtf8,
            EvaluatedBytesOp::Right,
            EvaluatedBytesOp::RightUtf8,
            EvaluatedBytesOp::Replace,
            EvaluatedBytesOp::BitCount,
            EvaluatedBytesOp::BitNeg,
            EvaluatedBytesOp::BitAnd,
            EvaluatedBytesOp::BitOr,
            EvaluatedBytesOp::BitXor,
            EvaluatedBytesOp::LeftShift,
            EvaluatedBytesOp::RightShift,
            EvaluatedBytesOp::UnaryNot,
            EvaluatedBytesOp::IsNull,
            EvaluatedBytesOp::IsTrue,
            EvaluatedBytesOp::IsFalse,
            EvaluatedBytesOp::IsTrueWithNull,
            EvaluatedBytesOp::IsNotNull,
            EvaluatedBytesOp::IsNotTrue,
            EvaluatedBytesOp::IsNotFalse,
            EvaluatedBytesOp::Md5,
            EvaluatedBytesOp::Sha1,
            EvaluatedBytesOp::LogicalAnd,
            EvaluatedBytesOp::LogicalOr,
            EvaluatedBytesOp::LogicalXor,
            EvaluatedBytesOp::InetAton,
            EvaluatedBytesOp::InetNtoa,
            EvaluatedBytesOp::Inet6Aton,
            EvaluatedBytesOp::Inet6Ntoa,
            EvaluatedBytesOp::AsinRaw,
            EvaluatedBytesOp::AcosRaw,
            EvaluatedBytesOp::SqrtRaw,
            EvaluatedBytesOp::SignRaw,
            EvaluatedBytesOp::RadiansRaw,
            EvaluatedBytesOp::DegreesRaw,
            EvaluatedBytesOp::PiRaw,
            EvaluatedBytesOp::IsIpv4Nullable,
            EvaluatedBytesOp::IsIpv6Nullable,
            EvaluatedBytesOp::IsIpv4CompatNullable,
            EvaluatedBytesOp::IsIpv4MappedNullable,
            EvaluatedBytesOp::SpaceNative,
            EvaluatedBytesOp::RepeatNative,
            EvaluatedBytesOp::ToBase64Native,
            EvaluatedBytesOp::FromBase64Native,
            EvaluatedBytesOp::FromBase64ValueNative,
            EvaluatedBytesOp::Lower,
            EvaluatedBytesOp::Upper,
            EvaluatedBytesOp::LowerUtf8Ready,
            EvaluatedBytesOp::UpperUtf8Ready,
            EvaluatedBytesOp::Sha2Native,
            EvaluatedBytesOp::OrdNative,
        ] {
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            let expected = if operation == EvaluatedBytesOp::Ascii {
                ProgramEntry::EvaluatedAscii
            } else {
                ProgramEntry::EvaluatedBytes
            };
            for entry in [
                ProgramEntry::Row,
                ProgramEntry::ControlLineage,
                ProgramEntry::SqlNumericBatch,
                ProgramEntry::EvaluatedAscii,
                ProgramEntry::EvaluatedBytes,
            ] {
                assert_eq!(program.check_entry(entry).is_ok(), entry == expected);
            }
            let arity = operation.input_types().len();
            assert_eq!(program.schema.len(), arity);
            for (slot, field_type) in program.schema.iter().enumerate() {
                assert_eq!(Some(field_type), operation.input_field_type(slot).as_ref());
                assert!(matches!(program.expression[slot],
                    RpnExpressionNode::ColumnRef { offset } if offset == slot));
            }
            assert_eq!(program.return_type(), &operation.return_type());
            let calls = operation.call_count();
            assert_eq!(program.expression.len(), arity + calls);
            for index in 0..calls {
                check_evaluated_bytes_kernel(operation, index, &program.expression[arity + index])
                    .unwrap();
            }
            assert!(operation.call_operation(calls).is_none());
            if operation == EvaluatedBytesOp::PiRaw {
                assert_eq!(arity, 0);
                assert_eq!(calls, 1);
                assert!(program.schema.is_empty());
                assert!(matches!(
                    program.expression.as_ref(),
                    [RpnExpressionNode::FnCall { args_len: 0, .. }]
                ));
            }

            let first = operation.call_operation(0).unwrap();
            let mut spec = LocalExpr::Call {
                function: first.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: first.return_type(),
                metadata: crate::CallMetadata::None,
            };
            for index in 1..calls {
                let primitive = operation.call_operation(index).unwrap();
                spec = LocalExpr::Call {
                    function: primitive.function_ref(),
                    args: vec![spec].into_boxed_slice(),
                    return_type: primitive.return_type(),
                    metadata: crate::CallMetadata::None,
                };
            }
            if operation != EvaluatedBytesOp::Ascii {
                assert_closed_source_rejected(&spec, &program.schema);
            }
            if let EvaluatedKernelKind::ClosedPrivate(id) = operation.kernel_kind() {
                assert_eq!(operation.function_ref(), FunctionRef::Local(id));
                let mut raw_call = CallBuild::local(
                    CallShape::new(
                        operation.function_ref(),
                        operation.return_type(),
                        program
                            .schema
                            .iter()
                            .cloned()
                            .map(CallArg::dynamic)
                            .collect(),
                    ),
                    crate::CallMetadata::None,
                );
                assert!(prepare_call(&mut raw_call).is_err());
            }
            if matches!(
                operation.function_ref(),
                FunctionRef::TiPb(ScalarFuncSig::LogicalAnd | ScalarFuncSig::LogicalOr)
            ) {
                let row =
                    compile_local(&spec, &program.schema, LocalCompileContext::default()).unwrap();
                assert!(row.check_entry(ProgramEntry::Row).is_ok());
                assert!(matches!(
                    row.expression.as_ref(),
                    [RpnExpressionNode::ShortCircuitFnCall { .. }]
                ));
            }
            let other = if operation == EvaluatedBytesOp::Length {
                EvaluatedBytesOp::BitLength
            } else {
                EvaluatedBytesOp::Length
            };
            assert!(matches!(
                compile(
                    &spec,
                    &program.schema,
                    LocalCompileContext::default(),
                    CompileMode::EvaluatedBytes(other),
                ),
                Err(LocalError::InvalidSpec(_))
            ));
            if arity > 1 {
                if let LocalExpr::Call { args, .. } = &mut spec {
                    args.swap(0, 1);
                }
                assert!(matches!(
                    compile(
                        &spec,
                        &program.schema,
                        LocalCompileContext::default(),
                        CompileMode::EvaluatedBytes(operation)
                    ),
                    Err(LocalError::InvalidSpec(_))
                ));
            }
        }
    }

    #[test]
    fn evaluated_ascii_factory_obeys_current_two_node_depth_limits() {
        for limits in [
            CompileLimits {
                max_nodes: 0,
                max_depth: 2,
            },
            CompileLimits {
                max_nodes: 1,
                max_depth: 2,
            },
            CompileLimits {
                max_nodes: 2,
                max_depth: 0,
            },
            CompileLimits {
                max_nodes: 2,
                max_depth: 1,
            },
        ] {
            assert!(matches!(
                compile_evaluated_ascii(LocalCompileContext { limits }),
                Err(LocalError::ResourceLimit(_))
            ));
        }
        assert!(
            compile_evaluated_ascii(LocalCompileContext {
                limits: CompileLimits {
                    max_nodes: 2,
                    max_depth: 2
                },
            })
            .is_ok()
        );
    }

    #[test]
    fn evaluated_ascii_does_not_open_existing_local_domains() {
        let spec = source();
        let schema = [evaluated_ascii_bytes_type()];
        let cx = LocalCompileContext::default();
        assert!(matches!(
            compile_local(&spec, &schema, cx),
            Err(LocalError::InvalidSpec(_))
        ));
        let hosts = HostCatalog::new(Vec::new()).unwrap();
        assert!(matches!(
            compile_local_with_hosts(&spec, &schema, cx, &hosts),
            Err(LocalError::InvalidSpec(_))
        ));
        assert!(matches!(
            OrdinaryProfileSpec::new(&spec, &schema, OrdinaryProfile::TypedRow, vec![], cx.limits,),
            Err(LocalError::InvalidSpec(_))
        ));
        assert!(matches!(
            NumericBatchFacts::sql_native_numeric_batch(&spec, &schema, vec![], cx.limits),
            Err(LocalError::InvalidSpec(_))
        ));
        assert!(matches!(
            ControlLineageFacts::sql_typed_row(&spec, &schema, vec![], cx.limits),
            Err(LocalError::InvalidSpec(_))
        ));
        let row = compile_local(
            &LocalExpr::Constant {
                value: ScalarValue::Int(Some(7)),
                field_type: evaluated_ascii_int_type(),
                literal_kind: LiteralKind::Typed,
            },
            &[],
            cx,
        )
        .unwrap();
        assert!(matches!(
            row.check_entry(ProgramEntry::EvaluatedAscii),
            Err(LocalError::InvalidSpec(_))
        ));
    }
}
