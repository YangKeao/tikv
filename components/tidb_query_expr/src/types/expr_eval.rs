// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use tidb_query_common::Result;
pub use tidb_query_datatype::codec::data_type::{
    BATCH_MAX_SIZE, IDENTICAL_LOGICAL_ROWS, LogicalRows,
};
use tidb_query_datatype::{
    codec::{batch::LazyBatchColumnVec, data_type::*},
    expr::EvalContext,
};
use tipb::FieldType;

use super::{
    RpnFnCallExtra,
    expr::{RpnExpression, RpnExpressionNode},
    function::{ControlKind, RpnFnMeta},
};
use crate::{
    impl_op::LogicalAccumulator,
    local::{
        ArgMode, CheckedResultFlow, EvaluatedBytesOp, FailureRecorder, HostArgReply,
        HostArgRequest, HostCatalogKey, HostInvocation, HostStart, HostStep, HostTaskId, InputRow,
        LineageCarrier, LocalError, LocalHostServices, LocalResult, LocalRuntimeServices,
        OrdinaryProfile, PreparedHostCall, PreparedOrdinaryCall, ResultMetaId,
        runtime::{
            EvalBudget, StorageMode, bytes_min_storage_bytes, int_min_storage_bytes,
            int_storage_bytes, int_vector_storage_bytes, vector_storage_bytes,
        },
    },
};

/// Represents a vector value node in the RPN stack.
///
/// It can be either an owned node or a reference node.
///
/// When node comes from a column reference, it is a reference node (both value
/// and field_type are references).
///
/// When nodes comes from an evaluated result, it is an owned node.
#[derive(Debug)]
pub enum RpnStackNodeVectorValue<'a> {
    Generated {
        // TODO: Maybe box it can be faster.
        physical_value: VectorValue,
    },
    Ref {
        physical_value: &'a VectorValue,
        logical_rows: &'a [usize],
    },
}

impl RpnStackNodeVectorValue<'_> {
    /// Gets a reference to the inner physical vector value.
    pub fn as_ref(&self) -> &VectorValue {
        match self {
            RpnStackNodeVectorValue::Generated { physical_value, .. } => physical_value,
            RpnStackNodeVectorValue::Ref { physical_value, .. } => physical_value,
        }
    }

    /// Gets the actual vector value.
    pub fn take_vector_value(self) -> Result<VectorValue> {
        match self {
            RpnStackNodeVectorValue::Generated { physical_value } => Ok(physical_value),
            RpnStackNodeVectorValue::Ref {
                physical_value,
                logical_rows,
                ..
            } => {
                // TODO: extract a common util function to do this
                let mut result_vec = physical_value.clone_empty(logical_rows.len());
                match_template::match_template! {
                    TT = [
                        Int,
                        Real,
                        Duration,
                        Decimal,
                        DateTime,
                        Bytes => BytesRef,
                        Json => JsonRef,
                        Enum => EnumRef,
                        Set => SetRef,
                        VectorFloat32 => VectorFloat32Ref,
                    ],
                    match &mut result_vec {
                        VectorValue::TT(dest_column) => {
                            let src_ref = TT::borrow_vector_value(physical_value);
                            for index in logical_rows {
                                dest_column.push(src_ref.get_option_ref(*index).map(|x| x.into_owned_value()));
                            }
                        },
                    }
                }

                Ok(result_vec)
            }
        }
    }

    /// Gets a reference to the logical rows.
    pub fn logical_rows_struct(&self) -> LogicalRows<'_> {
        match self {
            RpnStackNodeVectorValue::Generated { physical_value } => LogicalRows::Ref {
                logical_rows: &IDENTICAL_LOGICAL_ROWS[0..physical_value.len()],
            },

            RpnStackNodeVectorValue::Ref { logical_rows, .. } => LogicalRows::Ref { logical_rows },
        }
    }

    /// Gets a reference to the logical rows.
    pub fn logical_rows(&self) -> &[usize] {
        self.logical_rows_struct().as_slice()
    }
}

/// A type for each node in the RPN evaluation stack. It can be one of a scalar
/// value node or a vector value node. The vector value node can be either an
/// owned vector value or a reference.
#[derive(Debug)]
pub enum RpnStackNode<'a> {
    /// Represents a scalar value. Comes from a constant node in expression
    /// list.
    Scalar {
        value: &'a ScalarValue,
        field_type: &'a FieldType,
    },

    /// Represents a vector value. Comes from a column reference or evaluated
    /// result.
    Vector {
        value: RpnStackNodeVectorValue<'a>,
        field_type: &'a FieldType,
    },
}

impl RpnStackNode<'_> {
    /// Gets the field type.
    #[inline]
    pub fn field_type(&self) -> &FieldType {
        match self {
            RpnStackNode::Scalar { field_type, .. } => field_type,
            RpnStackNode::Vector { field_type, .. } => field_type,
        }
    }

    /// Borrows the inner scalar value for `Scalar` variant.
    #[inline]
    pub fn scalar_value(&self) -> Option<&ScalarValue> {
        match self {
            RpnStackNode::Scalar { value, .. } => Some(*value),
            RpnStackNode::Vector { .. } => None,
        }
    }

    /// Borrows the inner vector value for `Vector` variant.
    #[inline]
    pub fn vector_value(&self) -> Option<&RpnStackNodeVectorValue<'_>> {
        match self {
            RpnStackNode::Scalar { .. } => None,
            RpnStackNode::Vector { value, .. } => Some(value),
        }
    }

    /// Whether this is a `Scalar` variant.
    #[inline]
    pub fn is_scalar(&self) -> bool {
        matches!(self, RpnStackNode::Scalar { .. })
    }

    /// Whether this is a `Vector` variant.
    #[inline]
    pub fn is_vector(&self) -> bool {
        matches!(self, RpnStackNode::Vector { .. })
    }

    /// Gets the actual vector value.
    pub fn take_vector_value(self) -> Result<VectorValue> {
        match self {
            RpnStackNode::Scalar { .. } => Err(other_err!("take_vector_value on Scalar variant")),
            RpnStackNode::Vector { value, .. } => value.take_vector_value(),
        }
    }

    /// Gets a reference of the element by logical index.
    ///
    /// If this is a `Scalar` variant, the returned reference will be the same
    /// for any index.
    ///
    /// # Panics
    ///
    /// Panics if index is out of range and this is a `Vector` variant.
    #[inline]
    pub fn get_logical_scalar_ref(&self, logical_index: usize) -> ScalarValueRef<'_> {
        match self {
            RpnStackNode::Vector { value, .. } => {
                let physical_vector = value.as_ref();
                let logical_rows = value.logical_rows_struct();
                let idx = logical_rows.get_idx(logical_index);
                physical_vector.get_scalar_ref(idx)
            }
            RpnStackNode::Scalar { value, .. } => value.as_scalar_value_ref(),
        }
    }
}

/// Private value channel for the same driver. IDs name materialization records,
/// not value equality or a second expression graph. Kernel operand ABI stays
/// `RpnStackNode`; only the checked singleton control path carries a tag.
#[derive(Debug)]
pub(crate) struct FrameResult<'a> {
    pub(crate) node: RpnStackNode<'a>,
    pub(crate) meta: Option<ResultMetaId>,
}

impl<'a> FrameResult<'a> {
    fn unannotated(node: RpnStackNode<'a>) -> Self {
        Self { node, meta: None }
    }

    fn into_unannotated(self) -> LocalResult<RpnStackNode<'a>> {
        if self.meta.is_some() {
            return Err(LocalError::InvalidSpec(
                "result lineage cannot escape through an unannotated path".into(),
            ));
        }
        Ok(self.node)
    }

    pub(crate) fn retained_heap_bytes(&self, mode: StorageMode) -> usize {
        node_storage(&self.node, mode)
    }

    fn with_field_type(self, field_type: &'a FieldType) -> Self {
        let node = match self.node {
            RpnStackNode::Scalar { value, .. } => RpnStackNode::Scalar { value, field_type },
            RpnStackNode::Vector { value, .. } => RpnStackNode::Vector { value, field_type },
        };
        Self {
            node,
            meta: self.meta,
        }
    }
}

fn scalar_carrier(value: ScalarValueRef<'_>) -> Option<LineageCarrier> {
    match value {
        ScalarValueRef::Int(_) => Some(LineageCarrier::Int),
        ScalarValueRef::Bytes(_) => Some(LineageCarrier::Bytes),
        _ => None,
    }
}

fn scalar_is_null(value: ScalarValueRef<'_>) -> bool {
    match value {
        ScalarValueRef::Int(value) => value.is_none(),
        ScalarValueRef::Bytes(value) => value.is_none(),
        _ => unreachable!("lineaged carrier was checked"),
    }
}

fn carrier_min_storage(carrier: LineageCarrier, rows: usize) -> usize {
    match carrier {
        LineageCarrier::Int => int_min_storage_bytes(rows),
        LineageCarrier::Bytes => bytes_min_storage_bytes(rows, 0),
    }
    .unwrap_or(usize::MAX)
}

/// The one input seam used by the official RPN driver. Binding mode never
/// decodes/imports a column until its ColumnRef is actually demanded.
pub(crate) enum EvalInput<'data, 'services> {
    Decoded(&'data LazyBatchColumnVec),
    Bindings(&'services mut dyn LocalRuntimeServices),
    // A closed ready value, not a caller-supplied evaluator/provider. The owner
    // remains in the invocation facade until both it and the result are dropped.
    ReadyBytes {
        value: &'data ScalarValue,
        witness: &'services mut EvaluatedAsciiWitness,
    },
    ReadyArgs {
        values: &'data [ScalarValue],
        witness: &'services mut EvaluatedAsciiWitness,
    },
}

impl EvalInput<'_, '_> {
    fn retained_input_bytes(&self) -> Option<usize> {
        match self {
            Self::ReadyBytes {
                value: ScalarValue::Bytes(Some(value)),
                ..
            } => Some(value.capacity()),
            Self::ReadyArgs { values, .. } => values.iter().try_fold(0usize, |total, value| {
                total.checked_add(match value {
                    ScalarValue::Bytes(Some(bytes)) => bytes.capacity(),
                    _ => 0,
                })
            }),
            _ => Some(0),
        }
    }
}

fn evaluated_ready_args_match(operation: EvaluatedBytesOp, values: &[ScalarValue]) -> bool {
    let types = operation.input_types();
    (1..=3).contains(&types.len())
        && values.len() == types.len()
        && values.iter().zip(types).all(|(value, eval_type)| {
            matches!(
                (value, *eval_type),
                (ScalarValue::Int(_), tidb_query_datatype::EvalType::Int)
                    | (ScalarValue::Bytes(_), tidb_query_datatype::EvalType::Bytes)
            )
        })
}

/// Per-worker evidence at the actual generated-wrapper dispatch, including
/// NULL. This is not an ASCII-body counter, an input identity, or a native SQL
/// site.
#[derive(Default)]
pub(crate) struct EvaluatedAsciiWitness {
    invocations: u64,
}

impl EvaluatedAsciiWitness {
    pub(crate) fn invocations(&self) -> u64 {
        self.invocations
    }

    fn record_dispatch(&mut self) -> LocalResult<()> {
        self.invocations = self.invocations.checked_add(1).ok_or_else(|| {
            LocalError::ResourceLimit("evaluated ASCII invocation counter overflow".into())
        })?;
        Ok(())
    }
}

/// Execution admission is independent of retained-buffer measurement. Only
/// fixed private entry wrappers choose a domain; no budget flag selects a
/// scheduler or grants lineage/Bytes admission.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EvalExecution {
    Unannotated,
    SqlControlLineage,
    SqlNumericBatch,
    EvaluatedAscii,
    EvaluatedBytes(EvaluatedBytesOp),
}

impl EvalExecution {
    fn evaluated_bytes_operation(self) -> Option<EvaluatedBytesOp> {
        match self {
            Self::EvaluatedAscii => Some(EvaluatedBytesOp::Ascii),
            Self::EvaluatedBytes(operation) => Some(operation),
            _ => None,
        }
    }

    fn check_budget(self, budget: &EvalBudget) -> LocalResult<()> {
        let valid = match self {
            Self::Unannotated => budget.mode() == StorageMode::ConservativeInt,
            Self::SqlControlLineage
            | Self::SqlNumericBatch
            | Self::EvaluatedAscii
            | Self::EvaluatedBytes(_) => {
                budget.is_checked() && budget.mode() == StorageMode::ExactRetained
            }
        };
        if !valid {
            return Err(LocalError::InvalidSpec(
                "execution domain and accounting policy differ".into(),
            ));
        }
        Ok(())
    }

    fn check_input(self, input: &EvalInput<'_, '_>) -> LocalResult<()> {
        let valid = match (self.evaluated_bytes_operation(), input) {
            (None, EvalInput::Decoded(_) | EvalInput::Bindings(_)) => true,
            (Some(operation), EvalInput::ReadyBytes { value, .. }) => {
                operation.input_types() == [tidb_query_datatype::EvalType::Bytes]
                    && matches!(value, ScalarValue::Bytes(_))
            }
            (Some(operation), EvalInput::ReadyArgs { values, .. }) => {
                evaluated_ready_args_match(operation, values)
            }
            _ => false,
        };
        if !valid {
            return Err(LocalError::InvalidSpec(
                "ready Bytes and evaluated ASCII must share their closed execution domain".into(),
            ));
        }
        if input.retained_input_bytes().is_none() {
            return Err(LocalError::ResourceLimit(
                "evaluated ready-argument storage overflow".into(),
            ));
        }
        Ok(())
    }

    fn check_ordinary(self, prepared: &PreparedOrdinaryCall, rows: usize) -> LocalResult<()> {
        let site = prepared.site();
        let valid = match (self, site.profile(), site.original_pb_signature()) {
            (Self::Unannotated, OrdinaryProfile::TypedRow, None) => rows == 1,
            (Self::Unannotated, OrdinaryProfile::PbRow, Some(signature)) => {
                rows == 1 && signature == tipb::ScalarFuncSig::PlusInt as i32
            }
            (Self::SqlNumericBatch, OrdinaryProfile::NativeNumericBatch, None) => {
                rows > 0 && rows <= BATCH_MAX_SIZE
            }
            _ => false,
        };
        if !valid || prepared.function() != crate::FunctionRef::TiPb(tipb::ScalarFuncSig::PlusInt) {
            return Err(LocalError::InvalidSpec(
                "ordinary source profile and execution domain differ".into(),
            ));
        }
        Ok(())
    }
}

// Defensive check for the fixed numeric entry. Immutable compilation facts
// apply the same batch-only raw-type policy before copying any descriptor.
fn numeric_batch_int_type(field_type: &FieldType) -> bool {
    let excluded = (1 << 5) | (1 << 8) | (1 << 11) | (1 << 18) | (1 << 21);
    let documented = (1 << 25) - 1;
    field_type.get_tp() == tidb_query_datatype::FieldTypeTp::LongLong as i32
        && !field_type.get_array()
        && field_type.get_flag() & (excluded | !documented) == 0
}

// Fresh scalar-only canonical protobuf values have no heap owners. Comparing
// these fixed ABI records does not clone a descriptor or select/prepare a call.
pub(crate) fn evaluated_bytes_shape(
    operation: EvaluatedBytesOp,
    nodes: &[RpnExpressionNode],
    schema: &[FieldType],
) -> bool {
    let arity = operation.input_types().len();
    let calls = operation.call_count();
    if !(1..=3).contains(&arity)
        || !(1..=2).contains(&calls)
        || schema.len() != arity
        || nodes.len() != arity + calls
        || schema.iter().enumerate().any(|(slot, field_type)| {
            operation.input_field_type(slot).as_ref() != Some(field_type)
        })
        || nodes[..arity].iter().enumerate().any(|(slot, node)| {
            !matches!(node, RpnExpressionNode::ColumnRef { offset } if *offset == slot)
        })
    {
        return false;
    }
    nodes[arity..].iter().enumerate().all(|(index, node)| {
        let Some(primitive) = operation.call_operation(index) else {
            return false;
        };
        let expected_arity = if index == 0 { arity } else { 1 };
        if primitive.input_types().len() != expected_arity {
            return false;
        }
        let input_types_match = if index == 0 {
            primitive.input_types() == operation.input_types()
        } else {
            matches!(&nodes[arity + index - 1], RpnExpressionNode::FnCall { field_type, .. }
                if primitive.input_field_type(0).as_ref() == Some(field_type))
        };
        match node {
            RpnExpressionNode::FnCall {
                func_meta,
                args_len,
                field_type,
                metadata,
            } => {
                let official = primitive.fn_meta();
                input_types_match
                    && *args_len == expected_arity
                    && func_meta.name == official.name
                    && std::ptr::fn_addr_eq(func_meta.fn_ptr, official.fn_ptr)
                    && *field_type == primitive.return_type()
                    && (index + 1 != calls || *field_type == operation.return_type())
                    && metadata.is::<()>()
            }
            _ => false,
        }
    })
}

#[derive(Clone)]
enum FrameRows<'a> {
    Borrowed(&'a [usize], usize),
    Owned(Vec<usize>, usize),
}

impl FrameRows<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Borrowed(_, len) | Self::Owned(_, len) => *len,
        }
    }
    fn physical(&self) -> &[usize] {
        match self {
            Self::Borrowed(rows, _) => rows,
            Self::Owned(rows, _) => rows,
        }
    }
    fn storage(&self) -> usize {
        match self {
            Self::Owned(rows, _) => rows.capacity().saturating_mul(std::mem::size_of::<usize>()),
            _ => 0,
        }
    }
    fn select(&self, positions: &[usize]) -> Self {
        // Legacy constant-only expressions can have output rows but no physical
        // row map. Do not invent physical rows for that low-level contract.
        let rows = if self.physical().is_empty() {
            Vec::new()
        } else {
            positions
                .iter()
                .map(|&position| self.physical()[position])
                .collect()
        };
        Self::Owned(rows, positions.len())
    }
}

struct ProgramFrame<'a> {
    nodes: &'a [RpnExpressionNode],
    pc: usize,
    rows: FrameRows<'a>,
    stack: Vec<RpnStackNode<'a>>,
    flow: Option<CheckedResultFlow>,
    // Only an annotated ONE-node structured subprogram may use this tag slot.
    // Eager operand stacks and kernel argument slices remain unchanged.
    result_meta: Option<ResultMetaId>,
}

impl<'a> ProgramFrame<'a> {
    fn new(program: &'a RpnExpression, rows: FrameRows<'a>) -> Self {
        Self {
            nodes: program.as_ref(),
            pc: 0,
            rows,
            stack: Vec::new(),
            flow: program.checked_result_flow(),
            result_meta: None,
        }
    }

    fn check_flow(&self) -> LocalResult<()> {
        let Some(flow) = self.flow else {
            return Ok(());
        };
        if self.nodes.len() != 1 || self.rows.len() != 1 {
            return Err(LocalError::InvalidSpec(
                "lineage requires an annotated singleton subprogram".into(),
            ));
        }
        let valid = match (&self.nodes[0], flow) {
            (
                RpnExpressionNode::Constant { .. } | RpnExpressionNode::ColumnRef { .. },
                CheckedResultFlow::Leaf { .. },
            ) => true,
            (
                RpnExpressionNode::ShortCircuitFnCall { func_meta, .. },
                CheckedResultFlow::PreserveSelected { .. },
            ) => !func_meta.kind.is_logical(),
            (
                RpnExpressionNode::ShortCircuitFnCall { func_meta, .. },
                CheckedResultFlow::OwnResult { .. },
            ) => func_meta.kind.is_logical(),
            _ => false,
        };
        if !valid {
            return Err(LocalError::InvalidSpec(
                "lineage annotation does not match its structured node".into(),
            ));
        }
        Ok(())
    }

    fn check_execution(&self, execution: EvalExecution, schema: &[FieldType]) -> LocalResult<()> {
        self.check_flow()?;
        let valid = match execution {
            EvalExecution::Unannotated => self.flow.is_none(),
            EvalExecution::SqlControlLineage => self.flow.is_some() && self.rows.len() == 1,
            EvalExecution::EvaluatedAscii | EvalExecution::EvaluatedBytes(_) => {
                let operation = execution.evaluated_bytes_operation().unwrap();
                self.flow.is_none()
                    && self.rows.len() == 1
                    && self.rows.physical() == [0]
                    && evaluated_bytes_shape(operation, self.nodes, schema)
            }
            EvalExecution::SqlNumericBatch => {
                if self.flow.is_some()
                    || self.nodes.len() != 1
                    || self.rows.len() == 0
                    || self.rows.len() > BATCH_MAX_SIZE
                    || self.rows.physical().len() != self.rows.len()
                {
                    return Err(LocalError::InvalidSpec(
                        "numeric batch requires its untagged singleton-structured phase".into(),
                    ));
                }
                match &self.nodes[0] {
                    RpnExpressionNode::Constant {
                        value: ScalarValue::Int(_),
                        field_type,
                    } => numeric_batch_int_type(field_type),
                    RpnExpressionNode::ColumnRef { offset } => {
                        schema.get(*offset).is_some_and(numeric_batch_int_type)
                    }
                    RpnExpressionNode::OrdinaryFnCall { prepared, args } => {
                        execution.check_ordinary(prepared, self.rows.len())?;
                        args.len() == 2 && numeric_batch_int_type(prepared.return_type())
                    }
                    _ => false,
                }
            }
        };
        if !valid {
            return Err(LocalError::InvalidSpec(
                "program annotation/node differs from its execution domain".into(),
            ));
        }
        Ok(())
    }

    fn leaf_result(&self, node: RpnStackNode<'a>) -> LocalResult<FrameResult<'a>> {
        match self.flow {
            None => Ok(FrameResult::unannotated(node)),
            Some(CheckedResultFlow::Leaf { id, carrier }) => {
                validate_frame_result(&node, 1)?;
                if scalar_carrier(node.get_logical_scalar_ref(0)) != Some(carrier) {
                    return Err(LocalError::InvalidSpec(
                        "lineage leaf returned the wrong carrier".into(),
                    ));
                }
                Ok(FrameResult {
                    node,
                    meta: Some(id),
                })
            }
            _ => Err(LocalError::InvalidSpec(
                "lineage structured result was treated as a leaf".into(),
            )),
        }
    }

    fn result_precharge(&self, node: &RpnExpressionNode, execution: EvalExecution) -> usize {
        if execution == EvalExecution::Unannotated {
            int_storage_bytes(self.rows.len())
        } else if matches!(node, RpnExpressionNode::Constant { .. }) {
            0 // borrowed compiled scalar; do not materialize an unneeded value.
        } else if execution == EvalExecution::SqlNumericBatch {
            int_min_storage_bytes(self.rows.len()).unwrap_or(usize::MAX)
        } else if let Some(operation) = execution.evaluated_bytes_operation() {
            match node {
                RpnExpressionNode::ColumnRef { offset }
                    if *offset < operation.input_types().len() =>
                {
                    0
                }
                RpnExpressionNode::FnCall { .. } => match operation.eval_type() {
                    tidb_query_datatype::EvalType::Int => {
                        int_min_storage_bytes(1).unwrap_or(usize::MAX)
                    }
                    tidb_query_datatype::EvalType::Bytes => {
                        bytes_min_storage_bytes(1, 0).unwrap_or(usize::MAX)
                    }
                    _ => unreachable!("the closed ready-Bytes result is Int or Bytes"),
                },
                _ => usize::MAX, // the complete fixed shape was checked first.
            }
        } else {
            self.flow.map_or(usize::MAX, |flow| {
                carrier_min_storage(flow.carrier(), self.rows.len())
            })
        }
    }
}

fn reserve_stack(
    frame: &mut ProgramFrame<'_>,
    needed: usize,
    retained: &mut usize,
    budget: &EvalBudget,
) -> LocalResult<()> {
    if needed <= frame.stack.capacity() {
        return Ok(());
    }
    let old = frame.stack.capacity();
    // Legacy keeps its once-per-program allocation. Checked local growth is
    // geometric and charged before allocation or a subsequent effect.
    let target = if budget.is_checked() {
        old.saturating_mul(2)
            .max(4)
            .min(frame.nodes.len())
            .max(needed)
    } else {
        frame.nodes.len().max(needed)
    };
    let bytes = target
        .saturating_sub(old)
        .saturating_mul(std::mem::size_of::<RpnStackNode<'_>>());
    budget.storage(retained.saturating_add(bytes))?;
    let additional = target - frame.stack.len();
    reserve(&mut frame.stack, additional)?;
    *retained = retained.saturating_add(
        frame
            .stack
            .capacity()
            .saturating_sub(old)
            .saturating_mul(std::mem::size_of::<RpnStackNode<'_>>()),
    );
    if budget.mode() == StorageMode::ExactRetained && frame.stack.capacity() > old {
        // Vec may retain more than requested. Check actual replacement plus a
        // conservative old/new overlap before any subsequent semantic effect.
        budget.storage(
            retained.saturating_add(old.saturating_mul(std::mem::size_of::<RpnStackNode<'_>>())),
        )?;
    }
    budget.storage(*retained)
}

struct ControlFrame<'a> {
    kind: ControlKind,
    args: &'a [RpnExpression],
    field_type: &'a FieldType,
    rows: FrameRows<'a>,
    next: Option<usize>,
    awaiting: Option<usize>,
    value: Option<Int>,
    flow: Option<CheckedResultFlow>,
    selected: Option<FrameResult<'a>>,
    logical: Option<LogicalAccumulator>,
    pending: Option<Vec<usize>>,
}

impl<'a> ControlFrame<'a> {
    fn new(
        kind: ControlKind,
        args: &'a [RpnExpression],
        field_type: &'a FieldType,
        rows: FrameRows<'a>,
        flow: Option<CheckedResultFlow>,
    ) -> LocalResult<Self> {
        if let Some(flow) = flow {
            let valid = match flow {
                CheckedResultFlow::OwnResult { .. } => kind.is_logical(),
                CheckedResultFlow::PreserveSelected { .. } => !kind.is_logical(),
                CheckedResultFlow::Leaf { .. } => false,
            };
            if !valid || rows.len() != 1 {
                return Err(LocalError::InvalidSpec(
                    "lineage control has an incompatible result flow/row shape".into(),
                ));
            }
        }
        if kind.is_logical() {
            assert!(args.len() >= 2);
            assert!(rows.physical().is_empty() || rows.physical().len() == rows.len());
        } else if rows.len() != 1 {
            return Err(LocalError::InvalidSpec(
                "non-logical controls require the admitted singleton profile".into(),
            ));
        }
        Ok(Self {
            kind,
            args,
            field_type,
            rows,
            next: (!args.is_empty()).then_some(0),
            awaiting: None,
            value: None,
            flow,
            selected: None,
            logical: kind.is_logical().then(|| LogicalAccumulator::new(kind)),
            pending: None,
        })
    }

    fn accept_logical(
        &mut self,
        child: RpnStackNode<'a>,
        index: usize,
        count: usize,
    ) -> LocalResult<()> {
        let logical = self
            .logical
            .as_mut()
            .expect("logical control has an accumulator");
        let resolved = logical
            .merge(child, self.pending.as_deref(), count, self.rows.len())
            .map_err(LocalError::Evaluation)?;
        self.next = if resolved == count || index + 1 == self.args.len() {
            None
        } else {
            Some(index + 1)
        };
        if self.next.is_some() && resolved != 0 {
            if let Some(pending) = &mut self.pending {
                pending.retain(|&position| !logical.is_resolved(position));
            } else {
                self.pending = Some(
                    (0..self.rows.len())
                        .filter(|&position| !logical.is_resolved(position))
                        .collect(),
                );
            }
        }
        Ok(())
    }

    fn accept(&mut self, child: FrameResult<'a>, schema: &'a [FieldType]) -> LocalResult<()> {
        let index = self.awaiting.take().expect("control awaited a child");
        let count = self.pending.as_ref().map_or(self.rows.len(), Vec::len);
        validate_frame_result(&child.node, count)?;
        if let Some(flow) = self.flow {
            return self.accept_lineaged(child, index, schema, flow);
        }
        let child = child.into_unannotated()?;
        if self.logical.is_some() {
            return self.accept_logical(child, index, count);
        }
        let value = match child.get_logical_scalar_ref(0) {
            ScalarValueRef::Int(value) => value.copied(),
            _ => {
                return Err(LocalError::InvalidSpec(
                    "control child is outside signed Int admission".into(),
                ));
            }
        };
        self.next = None;
        match self.kind {
            ControlKind::If if index == 0 => {
                self.next = Some(if value.unwrap_or(0) != 0 { 1 } else { 2 })
            }
            ControlKind::IfNull if index == 0 && value.is_none() => self.next = Some(1),
            ControlKind::Coalesce if value.is_none() && index + 1 < self.args.len() => {
                self.next = Some(index + 1)
            }
            ControlKind::CaseWhen if index % 2 == 0 && index + 1 < self.args.len() => {
                if value.unwrap_or(0) != 0 {
                    self.next = Some(index + 1);
                } else if index + 2 < self.args.len() {
                    self.next = Some(index + 2);
                }
            }
            _ => self.value = value,
        }
        Ok(())
    }

    fn accept_lineaged(
        &mut self,
        child: FrameResult<'a>,
        index: usize,
        schema: &'a [FieldType],
        flow: CheckedResultFlow,
    ) -> LocalResult<()> {
        let child_flow = self.args[index].checked_result_flow().ok_or_else(|| {
            LocalError::InvalidSpec("lineage child is missing its producer annotation".into())
        })?;
        let id = child.meta.ok_or_else(|| {
            LocalError::InvalidSpec("lineage child returned no materialization ID".into())
        })?;
        if id.unit() != flow.own_id().unit()
            || child.node.field_type() != self.args[index].ret_field_type(schema)
            || scalar_carrier(child.node.get_logical_scalar_ref(0)) != Some(child_flow.carrier())
        {
            return Err(LocalError::InvalidSpec(
                "lineage child type/carrier/namespace differs from its producer".into(),
            ));
        }
        if self.logical.is_some() {
            if child_flow.carrier() != LineageCarrier::Int {
                return Err(LocalError::InvalidSpec(
                    "logical lineage operand is not Int".into(),
                ));
            }
            return self.accept_logical(child.node, index, 1);
        }
        self.next = None;
        let condition = (self.kind == ControlKind::If && index == 0)
            || (self.kind == ControlKind::CaseWhen
                && index % 2 == 0
                && index + 1 < self.args.len());
        if condition {
            let ScalarValueRef::Int(value) = child.node.get_logical_scalar_ref(0) else {
                return Err(LocalError::InvalidSpec(
                    "lineage predicate is not signed Int".into(),
                ));
            };
            let truth = value.is_some_and(|value| *value != 0);
            if self.kind == ControlKind::If {
                self.next = Some(if truth { 1 } else { 2 });
            } else if truth {
                self.next = Some(index + 1);
            } else if index + 2 < self.args.len() {
                self.next = Some(index + 2);
            }
            return Ok(());
        }
        if child_flow.carrier() != flow.carrier() {
            return Err(LocalError::InvalidSpec(
                "selected result has a different carrier family".into(),
            ));
        }
        let is_null = scalar_is_null(child.node.get_logical_scalar_ref(0));
        if self.kind == ControlKind::IfNull && index == 0 && is_null {
            self.next = Some(1);
        } else if self.kind == ControlKind::Coalesce && is_null {
            if index + 1 < self.args.len() {
                self.next = Some(index + 1);
            }
            // Exhausted COALESCE generates its OWN NULL, not its last child's
            // ID.
        } else {
            self.selected = Some(child); // move the actual payload and CURRENT ID.
        }
        Ok(())
    }

    fn storage(&self, mode: StorageMode) -> usize {
        self.rows
            .storage()
            .saturating_add(self.pending.as_ref().map_or(0, |rows| {
                rows.capacity().saturating_mul(std::mem::size_of::<usize>())
            }))
            .saturating_add(self.logical.as_ref().map_or(0, |logical| match mode {
                StorageMode::ConservativeInt => logical.retained_bytes(),
                StorageMode::ExactRetained => logical.retained_bytes_exact().unwrap_or(usize::MAX),
            }))
            .saturating_add(
                self.selected
                    .as_ref()
                    .map_or(0, |value| value.retained_heap_bytes(mode)),
            )
    }

    fn finish_precharge(&self) -> usize {
        match self.flow {
            None => int_storage_bytes(self.rows.len()),
            Some(_) if self.logical.is_some() || self.selected.is_some() => 0,
            Some(flow) => carrier_min_storage(flow.carrier(), 1),
        }
    }

    fn request(&mut self) -> Option<(&'a RpnExpression, FrameRows<'a>)> {
        let index = self.next?;
        self.awaiting = Some(index);
        let rows = self.pending.as_ref().map_or_else(
            || self.rows.clone(),
            |positions| self.rows.select(positions),
        );
        Some((&self.args[index], rows))
    }

    fn finish(self) -> LocalResult<FrameResult<'a>> {
        if let Some(flow) = self.flow {
            if let Some(logical) = self.logical {
                return Ok(FrameResult {
                    node: RpnStackNode::Vector {
                        value: RpnStackNodeVectorValue::Generated {
                            physical_value: logical.into_vector(),
                        },
                        field_type: self.field_type,
                    },
                    meta: Some(flow.own_id()),
                });
            }
            if let Some(selected) = self.selected {
                return Ok(selected.with_field_type(self.field_type));
            }
            let physical_value = match flow.carrier() {
                LineageCarrier::Int => VectorValue::from_scalar(&ScalarValue::Int(None), 1),
                LineageCarrier::Bytes => {
                    let mut bytes = ChunkedVecBytes::try_with_capacities(1, 0)
                        .map_err(|error| LocalError::ResourceLimit(error.to_string()))?;
                    bytes.push_ref(None);
                    VectorValue::Bytes(bytes)
                }
            };
            return Ok(FrameResult {
                node: RpnStackNode::Vector {
                    value: RpnStackNodeVectorValue::Generated { physical_value },
                    field_type: self.field_type,
                },
                meta: Some(flow.own_id()),
            });
        }
        let value = self.logical.map_or_else(
            || VectorValue::from_scalar(&ScalarValue::Int(self.value), 1),
            LogicalAccumulator::into_vector,
        );
        Ok(FrameResult::unannotated(RpnStackNode::Vector {
            value: RpnStackNodeVectorValue::Generated {
                physical_value: value,
            },
            field_type: self.field_type,
        }))
    }
}

/// Owns the only mutable input/service seam for one occurrence. The host view
/// is reborrowed for one callback, never across evaluation of an argument.
struct TaskGuard<'input, 'data, 'services> {
    input: &'input mut EvalInput<'data, 'services>,
    catalog: Option<HostCatalogKey>,
    live: Vec<HostTaskId>,
}

impl TaskGuard<'_, '_, '_> {
    fn storage(&self) -> usize {
        self.live
            .capacity()
            .saturating_mul(std::mem::size_of::<HostTaskId>())
            // check_input rejects overflow before any frame or kernel runs.
            .saturating_add((&*self.input).retained_input_bytes().unwrap_or(usize::MAX))
    }

    fn host(&mut self) -> LocalResult<&mut dyn LocalHostServices> {
        let expected = self.catalog.ok_or_else(|| {
            LocalError::HostContract("host invocation has no compiled catalog".into())
        })?;
        let provider = match self.input {
            EvalInput::Bindings(services) => services.host_services(),
            EvalInput::Decoded(_) | EvalInput::ReadyBytes { .. } | EvalInput::ReadyArgs { .. } => {
                None
            }
        }
        .ok_or_else(|| LocalError::HostContract("host provider is unavailable".into()))?;
        if provider.catalog_key() != &expected {
            return Err(LocalError::HostContract(
                "host provider changed catalog identity".into(),
            ));
        }
        Ok(provider)
    }

    fn reserve_start(&mut self, budget: &EvalBudget, retained: &mut usize) -> LocalResult<()> {
        // Its storage includes a standing ready-value owner, not a host ledger.
        // Do not let that base enter the task-buffer-only reservation delta.
        if matches!(
            &*self.input,
            EvalInput::ReadyBytes { .. } | EvalInput::ReadyArgs { .. }
        ) {
            return Err(LocalError::InvalidSpec(
                "ready Bytes cannot start a host task".into(),
            ));
        }
        // A start may yield Pending. Reserve its potential task/ledger slot
        // before the callback, even if it will actually return immediate Ready.
        budget.task_count(self.live.len().saturating_add(1))?;
        let old = self.storage();
        let target = self.live.capacity().max(self.live.len().saturating_add(1));
        let additional = target
            .saturating_mul(std::mem::size_of::<HostTaskId>())
            .saturating_sub(old);
        budget.storage(retained.saturating_add(additional))?;
        reserve(&mut self.live, 1)?;
        *retained = retained.saturating_add(self.storage().saturating_sub(old));
        budget.storage(*retained)
    }

    fn arm(&mut self, task: HostTaskId) -> LocalResult<()> {
        if self.live.contains(&task) {
            // The one known identity is cancelled once during guard teardown.
            // A broken provider that aliases two resources behind the same ID
            // cannot be repaired by inventing a second cancellation identity.
            return Err(LocalError::HostContract(
                "duplicate live host task token".into(),
            ));
        }
        debug_assert!(self.live.len() < self.live.capacity());
        self.live.push(task); // Capacity was reserved before start.
        task.validate_generation()
    }

    fn complete(&mut self, task: HostTaskId) -> LocalResult<()> {
        let index = self
            .live
            .iter()
            .rposition(|known| *known == task)
            .ok_or_else(|| LocalError::HostContract("host task is not live".into()))?;
        self.live.remove(index);
        Ok(())
    }
}

impl Drop for TaskGuard<'_, '_, '_> {
    fn drop(&mut self) {
        let expected = self.catalog;
        while let Some(task) = self.live.pop() {
            // Cancellation must be infallible, non-panicking and diagnostic-free.
            // If a contract-breaking provider vanished/changed, the original
            // owner is no longer reachable: do not cancel an unrelated catalog,
            // replace the primary error, or claim that its resources were freed.
            // Do not allocate an error string on this cleanup-only path.
            let provider = match self.input {
                EvalInput::Bindings(services) => services.host_services(),
                EvalInput::Decoded(_)
                | EvalInput::ReadyBytes { .. }
                | EvalInput::ReadyArgs { .. } => None,
            };
            if let (Some(expected), Some(provider)) = (expected, provider) {
                if provider.catalog_key() == &expected {
                    provider.cancel(&task);
                }
            }
        }
    }
}

struct HostFrame<'a> {
    prepared: &'a PreparedHostCall,
    args: &'a [RpnExpression],
    rows: FrameRows<'a>,
    task: Option<HostTaskId>,
    request: Option<HostArgRequest>,
    awaiting: Option<usize>,
    ready: Option<usize>,
    cache: Vec<Option<VectorValue>>,
}

impl<'a> HostFrame<'a> {
    fn initial_storage(args: usize, rows: &FrameRows<'_>) -> usize {
        rows.storage()
            .saturating_add(args.saturating_mul(std::mem::size_of::<Option<VectorValue>>()))
    }

    fn new(
        prepared: &'a PreparedHostCall,
        args: &'a [RpnExpression],
        rows: FrameRows<'a>,
    ) -> LocalResult<Self> {
        if rows.len() != 1 || rows.physical().len() != 1 || args.len() != prepared.arg_types().len()
        {
            return Err(LocalError::HostContract(
                "host call requires its checked singleton shape".into(),
            ));
        }
        let mut cache = Vec::new();
        reserve(&mut cache, args.len())?;
        cache.resize_with(args.len(), || None);
        Ok(Self {
            prepared,
            args,
            rows,
            task: None,
            request: None,
            awaiting: None,
            ready: None,
            cache,
        })
    }

    fn storage(&self, _mode: StorageMode) -> usize {
        self.cache.iter().flatten().fold(
            Self::initial_storage(self.cache.capacity(), &self.rows),
            |total, value| total.saturating_add(int_storage_bytes(value.capacity())),
        )
    }

    fn invocation(&self, occurrence: usize) -> HostInvocation<'_> {
        HostInvocation {
            slot: self.prepared.slot(),
            row: InputRow {
                occurrence,
                input_row: self.rows.physical()[0],
            },
            arg_types: self.prepared.arg_types(),
            return_type: self.prepared.return_type(),
        }
    }

    fn request(&mut self, request: HostArgRequest) -> LocalResult<()> {
        if request.index >= self.args.len() {
            return Err(LocalError::HostContract(
                "host requested an absent argument".into(),
            ));
        }
        self.request = Some(request);
        Ok(())
    }

    fn accept(&mut self, value: RpnStackNode<'a>) -> LocalResult<()> {
        let index = self.awaiting.take().expect("host awaited an argument");
        validate_frame_result(&value, 1)?;
        if value.field_type() != &self.prepared.arg_types()[index] {
            return Err(LocalError::HostContract(
                "argument complete field type differs from registration".into(),
            ));
        }
        let scalar = value.get_logical_scalar_ref(0);
        if !matches!(scalar, ScalarValueRef::Int(_)) {
            return Err(LocalError::HostContract("host argument is not Int".into()));
        }
        // Cache only successful, owned singleton results for this invocation.
        self.cache[index] = Some(VectorValue::from_scalar(&scalar.to_owned(), 1));
        self.ready = Some(index);
        Ok(())
    }

    fn reply(&self, index: usize) -> HostArgReply<'_> {
        HostArgReply {
            index,
            field_type: &self.prepared.arg_types()[index],
            values: self.cache[index]
                .as_ref()
                .expect("successful cached argument"),
        }
    }

    fn finish(self, value: VectorValue) -> LocalResult<RpnStackNode<'a>> {
        if value.eval_type() != tidb_query_datatype::EvalType::Int || value.len() != 1 {
            return Err(LocalError::HostContract(
                "host Ready must contain one Int value".into(),
            ));
        }
        // The provider's transient allocations are outside this retained-scratch
        // budget. Do not keep arbitrary capacity or a borrow from the callback.
        let physical_value = VectorValue::from_scalar(&value.get_scalar_ref(0).to_owned(), 1);
        Ok(RpnStackNode::Vector {
            value: RpnStackNodeVectorValue::Generated { physical_value },
            field_type: self.prepared.return_type(),
        })
    }
}

/// A profile-checked ordinary call, not a different kernel implementation.
/// Its prepared record retains the exact source/site assertion. No error-site
/// wrapper or native source-shaped diagnostic adaptation is implied here.
struct OrdinaryFrame<'a> {
    prepared: &'a PreparedOrdinaryCall,
    args: &'a [RpnExpression],
    rows: FrameRows<'a>,
    next: usize,
    awaiting: Option<usize>,
    stopped: bool,
    values: Vec<RpnStackNode<'a>>,
}

impl<'a> OrdinaryFrame<'a> {
    fn initial_storage(rows: &FrameRows<'_>) -> usize {
        rows.storage()
            .saturating_add(2usize.saturating_mul(std::mem::size_of::<RpnStackNode<'_>>()))
    }

    fn new(
        prepared: &'a PreparedOrdinaryCall,
        args: &'a [RpnExpression],
        rows: FrameRows<'a>,
        execution: EvalExecution,
    ) -> LocalResult<Self> {
        execution.check_ordinary(prepared, rows.len())?;
        if args.len() != 2 {
            return Err(LocalError::InvalidSpec(
                "ordinary demand requires its checked singleton binary shape".into(),
            ));
        }
        let mut values = Vec::new();
        reserve(&mut values, 2)?;
        Ok(Self {
            prepared,
            args,
            rows,
            next: 0,
            awaiting: None,
            stopped: false,
            values,
        })
    }

    fn storage(&self, mode: StorageMode) -> usize {
        self.values.iter().fold(
            self.rows.storage().saturating_add(
                self.values
                    .capacity()
                    .saturating_mul(std::mem::size_of::<RpnStackNode<'_>>()),
            ),
            |total, value| total.saturating_add(node_storage(value, mode)),
        )
    }

    fn accept(
        &mut self,
        child: RpnStackNode<'a>,
        schema: &'a [FieldType],
        execution: EvalExecution,
    ) -> LocalResult<()> {
        execution.check_ordinary(self.prepared, self.rows.len())?;
        let index = self
            .awaiting
            .take()
            .expect("ordinary call awaited an operand");
        validate_frame_result(&child, self.rows.len())?;
        if child.field_type() != self.args[index].ret_field_type(schema) {
            return Err(LocalError::InvalidSpec(
                "ordinary operand complete field type changed".into(),
            ));
        }
        let ScalarValueRef::Int(value) = child.get_logical_scalar_ref(0) else {
            return Err(LocalError::InvalidSpec(
                "ordinary operand is outside Int admission".into(),
            ));
        };
        // Identity is the only admitted operand conversion. Row demand stops
        // on left NULL; numeric batch MUST complete both operand phases, even
        // at width one or when every left lane is NULL.
        self.stopped = execution == EvalExecution::Unannotated && index == 0 && value.is_none();
        self.next = index + 1;
        self.values.push(child); // two slots were reserved before either child.
        Ok(())
    }

    fn finish_numeric_batch(
        &self,
        ctx: &mut EvalContext,
        budget: &mut EvalBudget,
        other_live_bytes: usize,
        mut recorder: Option<&mut FailureRecorder>,
    ) -> LocalResult<RpnStackNode<'a>> {
        EvalExecution::SqlNumericBatch.check_ordinary(self.prepared, self.rows.len())?;
        if self.stopped || self.values.len() != 2 || self.next != 2 {
            return Err(LocalError::InvalidSpec(
                "numeric kernel phase requires both complete operands".into(),
            ));
        }
        let count = self.rows.len();
        budget.storage(
            other_live_bytes.saturating_add(int_min_storage_bytes(count).unwrap_or(usize::MAX)),
        )?;
        let mut output = ChunkedVecSized::<Int>::with_capacity(count);
        let mut output_bytes = int_vector_storage_bytes(&output).unwrap_or(usize::MAX);
        budget.storage(other_live_bytes.saturating_add(output_bytes))?;
        // This is ONLY this node's kernel phase. Both whole operand programs
        // have already completed; neither children nor a failed lane are replayed.
        for lane in 0..count {
            budget.charge()?;
            budget.storage(
                other_live_bytes
                    .saturating_add(output_bytes)
                    .saturating_add(int_min_storage_bytes(1).unwrap_or(usize::MAX)),
            )?;
            let position = [lane];
            let args = [
                numeric_lane_view(&self.values[0], &position)?,
                numeric_lane_view(&self.values[1], &position)?,
            ];
            let value = eval_ordinary_kernel(
                ctx,
                self.prepared,
                &args,
                Some(InputRow {
                    occurrence: lane,
                    input_row: self.rows.physical()[lane],
                }),
                recorder.as_deref_mut(),
            )?;
            validate_frame_result(&value, 1)?;
            let ScalarValueRef::Int(item) = value.get_logical_scalar_ref(0) else {
                return Err(LocalError::InvalidSpec(
                    "numeric kernel result is not Int".into(),
                ));
            };
            let incoming = node_storage(&value, budget.mode());
            budget.storage(
                other_live_bytes
                    .saturating_add(output_bytes)
                    .saturating_add(incoming),
            )?;
            output.push(item.copied());
            output_bytes = int_vector_storage_bytes(&output).unwrap_or(usize::MAX);
            budget.storage(
                other_live_bytes
                    .saturating_add(output_bytes)
                    .saturating_add(incoming),
            )?;
            // The singleton result is not retained across the next kernel lane.
            drop(value);
        }
        Ok(RpnStackNode::Vector {
            value: RpnStackNodeVectorValue::Generated {
                physical_value: VectorValue::Int(output),
            },
            field_type: self.prepared.return_type(),
        })
    }
}

fn numeric_lane_view<'b>(
    node: &'b RpnStackNode<'_>,
    position: &'b [usize; 1],
) -> LocalResult<RpnStackNode<'b>> {
    match node {
        RpnStackNode::Scalar { value, field_type } => {
            Ok(RpnStackNode::Scalar { value, field_type })
        }
        RpnStackNode::Vector {
            value: RpnStackNodeVectorValue::Generated { physical_value },
            field_type,
        } => Ok(RpnStackNode::Vector {
            value: RpnStackNodeVectorValue::Ref {
                physical_value,
                logical_rows: position,
            },
            field_type,
        }),
        _ => Err(LocalError::InvalidSpec(
            "numeric operands require owned selection-order vectors or immutable broadcasts".into(),
        )),
    }
}

/// The only ordinary-kernel error capture. A batch supplies the actual lane
/// being invoked, never an inferred row from an unlocalized vector failure.
fn eval_ordinary_kernel<'a>(
    ctx: &mut EvalContext,
    prepared: &'a PreparedOrdinaryCall,
    args: &[RpnStackNode<'_>],
    row: Option<InputRow>,
    recorder: Option<&mut FailureRecorder>,
) -> LocalResult<RpnStackNode<'a>> {
    let (func_meta, metadata) = prepared.kernel();
    eval_prepared_kernel(
        ctx,
        1,
        args,
        func_meta,
        prepared.return_type(),
        metadata,
        None,
    )
    .map_err(|error| match (recorder, row) {
        (Some(recorder), Some(row)) => recorder.capture_kernel(prepared.site(), row, error),
        _ => error,
    })
}

/// The only input error capture. Successful shape/storage checks remain
/// unsited; all domains preserve the exact moved callback error.
fn read_binding_value(
    services: &mut dyn LocalRuntimeServices,
    ctx: &mut EvalContext,
    slot: usize,
    row: InputRow,
    field_type: &FieldType,
    recorder: Option<&mut FailureRecorder>,
) -> LocalResult<VectorValue> {
    services
        .read_input(ctx, slot, row, field_type)
        .map_err(|error| match recorder {
            Some(recorder) => recorder.capture_input(slot, row, error),
            None => error,
        })
}

/// Import one demanded numeric leaf over the complete occurrence map. The
/// explicit base includes suspended siblings/frames; this is not a per-lane
/// budget reset or a second root-expression loop.
fn read_numeric_binding_phase(
    services: &mut dyn LocalRuntimeServices,
    ctx: &mut EvalContext,
    slot: usize,
    rows: &FrameRows<'_>,
    field_type: &FieldType,
    budget: &mut EvalBudget,
    other_live_bytes: usize,
    mut recorder: Option<&mut FailureRecorder>,
) -> LocalResult<VectorValue> {
    let count = rows.len();
    if count == 0 || count > BATCH_MAX_SIZE || rows.physical().len() != count {
        return Err(LocalError::InvalidSpec(
            "numeric binding phase has an invalid occurrence map".into(),
        ));
    }
    budget.storage(
        other_live_bytes.saturating_add(int_min_storage_bytes(count).unwrap_or(usize::MAX)),
    )?;
    let mut output = ChunkedVecSized::<Int>::with_capacity(count);
    let mut output_bytes = int_vector_storage_bytes(&output).unwrap_or(usize::MAX);
    budget.storage(other_live_bytes.saturating_add(output_bytes))?;
    for (occurrence, &input_row) in rows.physical().iter().enumerate() {
        budget.storage(
            other_live_bytes
                .saturating_add(output_bytes)
                .saturating_add(int_min_storage_bytes(1).unwrap_or(usize::MAX)),
        )?;
        budget.charge()?;
        let value = read_binding_value(
            services,
            ctx,
            slot,
            InputRow {
                occurrence,
                input_row,
            },
            field_type,
            recorder.as_deref_mut(),
        )?;
        if value.eval_type() != tidb_query_datatype::EvalType::Int || value.len() != 1 {
            return Err(LocalError::BindingContract(
                "numeric read_input must return one Int value".into(),
            ));
        }
        let incoming = vector_storage_bytes(&value, budget.mode());
        budget.storage(
            other_live_bytes
                .saturating_add(output_bytes)
                .saturating_add(incoming),
        )?;
        let ScalarValueRef::Int(item) = value.get_scalar_ref(0) else {
            unreachable!("checked Int carrier");
        };
        output.push(item.copied());
        output_bytes = int_vector_storage_bytes(&output).unwrap_or(usize::MAX);
        budget.storage(
            other_live_bytes
                .saturating_add(output_bytes)
                .saturating_add(incoming),
        )?;
        drop(value);
    }
    Ok(VectorValue::Int(output))
}

/// Both eager FnCall and staged ordinary demand enter this one prepared kernel
/// seam. Metadata was constructed once by the canonical preparation helper.
fn eval_prepared_kernel<'a>(
    ctx: &mut EvalContext,
    output_rows: usize,
    args: &[RpnStackNode<'_>],
    func_meta: RpnFnMeta,
    ret_field_type: &'a FieldType,
    metadata: &(dyn std::any::Any + Send),
    witness: Option<&mut EvaluatedAsciiWitness>,
) -> LocalResult<RpnStackNode<'a>> {
    let mut extra = RpnFnCallExtra { ret_field_type };
    if let Some(witness) = witness {
        // Record only at the real generated-wrapper dispatch. The checked
        // counter may refuse before dispatch; no fallible work intervenes.
        witness.record_dispatch()?;
    }
    let physical_value = (func_meta.fn_ptr)(ctx, output_rows, args, &mut extra, metadata)
        .map_err(LocalError::Evaluation)?;
    Ok(RpnStackNode::Vector {
        value: RpnStackNodeVectorValue::Generated { physical_value },
        field_type: ret_field_type,
    })
}

enum EvalFrame<'a> {
    Program(ProgramFrame<'a>),
    Control(ControlFrame<'a>),
    Host(HostFrame<'a>),
    Ordinary(OrdinaryFrame<'a>),
}

fn node_storage(node: &RpnStackNode<'_>, mode: StorageMode) -> usize {
    match node {
        RpnStackNode::Vector {
            value: RpnStackNodeVectorValue::Generated { physical_value },
            ..
        } => vector_storage_bytes(physical_value, mode),
        _ => 0,
    }
}

impl EvalFrame<'_> {
    fn storage(&self, mode: StorageMode) -> usize {
        match self {
            Self::Program(frame) => frame.stack.iter().fold(
                frame.rows.storage().saturating_add(
                    frame
                        .stack
                        .capacity()
                        .saturating_mul(std::mem::size_of::<RpnStackNode<'_>>()),
                ),
                |total, node| total.saturating_add(node_storage(node, mode)),
            ),
            Self::Control(frame) => frame.storage(mode),
            Self::Host(frame) => frame.storage(mode),
            Self::Ordinary(frame) => frame.storage(mode),
        }
    }
}

fn frames_storage(frames: &[EvalFrame<'_>], capacity: usize, mode: StorageMode) -> usize {
    frames.iter().fold(
        capacity.saturating_mul(std::mem::size_of::<EvalFrame<'_>>()),
        |total, frame| total.saturating_add(frame.storage(mode)),
    )
}

fn reserve<T>(values: &mut Vec<T>, additional: usize) -> LocalResult<()> {
    values
        .try_reserve_exact(additional)
        .map_err(|_| LocalError::ResourceLimit("cannot reserve evaluation storage".into()))
}

fn push_frame<'a>(
    frames: &mut Vec<EvalFrame<'a>>,
    frame: EvalFrame<'a>,
    budget: &EvalBudget,
    task_storage: usize,
) -> LocalResult<()> {
    budget.depth(frames.len().saturating_add(1))?;
    if budget.is_checked() {
        budget.storage(
            frames_storage(
                frames,
                frames.capacity().max(frames.len().saturating_add(1)),
                budget.mode(),
            )
            .saturating_add(frame.storage(budget.mode()))
            .saturating_add(task_storage),
        )?;
    }
    let old_capacity = frames.capacity();
    reserve(frames, 1)?;
    frames.push(frame);
    if budget.is_checked() {
        let retained =
            frames_storage(frames, frames.capacity(), budget.mode()).saturating_add(task_storage);
        if budget.mode() == StorageMode::ExactRetained && frames.capacity() > old_capacity {
            budget.storage(retained.saturating_add(
                old_capacity.saturating_mul(std::mem::size_of::<EvalFrame<'_>>()),
            ))?;
        }
        budget.storage(retained)?;
    }
    Ok(())
}

fn validate_frame_result(value: &RpnStackNode<'_>, count: usize) -> LocalResult<()> {
    let valid = match value {
        RpnStackNode::Scalar { .. } => true,
        RpnStackNode::Vector {
            value: RpnStackNodeVectorValue::Generated { physical_value },
            ..
        } => physical_value.len() == count,
        RpnStackNode::Vector {
            value: RpnStackNodeVectorValue::Ref { logical_rows, .. },
            ..
        } => logical_rows.len() == count,
    };
    if !valid {
        return Err(LocalError::InvalidSpec(
            "RPN child returned an invalid row shape".into(),
        ));
    }
    Ok(())
}

fn legacy_error(error: LocalError) -> tidb_query_common::Error {
    match error {
        LocalError::Evaluation(error) => error,
        error => other_err!("{}", error),
    }
}

/// One official RPN frame loop for legacy wire, controls, staged hosts and
/// profile-checked ordinary demand. No callback recursively invokes eval.
fn eval_frames<'a, 'data: 'a>(
    first: EvalFrame<'a>,
    ctx: &mut EvalContext,
    schema: &'a [FieldType],
    input: &mut EvalInput<'data, '_>,
    occurrence: usize,
    host_catalog: Option<HostCatalogKey>,
    budget: &mut EvalBudget,
    mut recorder: Option<&mut FailureRecorder>,
    execution: EvalExecution,
) -> LocalResult<FrameResult<'a>> {
    execution.check_budget(budget)?;
    execution.check_input(input)?;
    if execution.evaluated_bytes_operation().is_some() && host_catalog.is_some() {
        return Err(LocalError::InvalidSpec(
            if execution == EvalExecution::EvaluatedAscii {
                "evaluated ASCII has no host catalog"
            } else {
                "evaluated Bytes has no host catalog"
            }
            .into(),
        ));
    }
    // Preserve the allocation-free legacy leaf path, using the exact same
    // primitive helper and checked ColumnRef seam as the frame loop.
    if let EvalFrame::Program(frame) = &first {
        frame.check_execution(execution, schema)?;
        if frame.nodes.len() == 1
            && !matches!(
                frame.nodes[0],
                RpnExpressionNode::ShortCircuitFnCall { .. }
                    | RpnExpressionNode::HostCall { .. }
                    | RpnExpressionNode::OrdinaryFnCall { .. }
            )
        {
            budget.depth(1)?;
            budget.charge()?;
            budget.storage(frame.result_precharge(&frame.nodes[0], execution))?;
            let (_, value) = RpnExpression::eval_one_node(
                ctx,
                schema,
                input,
                &frame.rows,
                occurrence,
                budget,
                &frame.nodes[0],
                &[],
                recorder.as_deref_mut(),
                frame.flow,
                execution,
                0,
            )?;
            budget.storage(node_storage(&value, budget.mode()))?;
            return frame.leaf_result(value);
        }
    }
    // The guard is declared before frames so its tokens remain armed while
    // frame values unwind/drop. It holds no provider view across a child.
    let mut tasks = TaskGuard {
        input,
        catalog: host_catalog,
        live: Vec::new(),
    };
    let mut frames = Vec::new();
    push_frame(&mut frames, first, budget, tasks.storage())?;
    let mut returned: Option<FrameResult<'a>> = None;
    while !frames.is_empty() {
        let mut retained = if budget.is_checked() {
            let bytes = frames_storage(&frames, frames.capacity(), budget.mode())
                .saturating_add(
                    returned
                        .as_ref()
                        .map_or(0, |value| value.retained_heap_bytes(budget.mode())),
                )
                .saturating_add(tasks.storage());
            budget.storage(bytes)?;
            bytes
        } else {
            0
        };
        match frames.pop().unwrap() {
            EvalFrame::Program(mut frame) => {
                frame.check_execution(execution, schema)?;
                if let Some(value) = returned.take() {
                    let needed = frame.stack.len().saturating_add(1);
                    reserve_stack(&mut frame, needed, &mut retained, budget)?;
                    if let Some(flow) = frame.flow {
                        let id = value.meta.ok_or_else(|| {
                            LocalError::InvalidSpec(
                                "annotated program received no result ID".into(),
                            )
                        })?;
                        if id.unit() != flow.own_id().unit() {
                            return Err(LocalError::InvalidSpec(
                                "result ID namespace changed across a program boundary".into(),
                            ));
                        }
                        frame.result_meta = Some(id);
                        frame.stack.push(value.node);
                    } else {
                        frame.stack.push(value.into_unannotated()?);
                    }
                }
                if frame.pc == frame.nodes.len() {
                    assert_eq!(frame.stack.len(), 1);
                    returned = Some(FrameResult {
                        node: frame.stack.pop().unwrap(),
                        meta: frame.result_meta,
                    });
                    continue;
                }
                let node = &frame.nodes[frame.pc];
                frame.pc += 1;
                budget.charge()?;
                if let RpnExpressionNode::ShortCircuitFnCall {
                    func_meta,
                    args,
                    field_type,
                } = node
                {
                    let control = ControlFrame::new(
                        func_meta.kind,
                        args,
                        field_type,
                        frame.rows.clone(),
                        frame.flow,
                    )?;
                    push_frame(
                        &mut frames,
                        EvalFrame::Program(frame),
                        budget,
                        tasks.storage(),
                    )?;
                    push_frame(
                        &mut frames,
                        EvalFrame::Control(control),
                        budget,
                        tasks.storage(),
                    )?;
                } else if let RpnExpressionNode::HostCall { prepared, args } = node {
                    if tasks.catalog != Some(*prepared.catalog_key()) {
                        return Err(LocalError::HostContract(
                            "host node differs from invocation catalog".into(),
                        ));
                    }
                    budget.storage(
                        retained
                            .saturating_add(HostFrame::initial_storage(args.len(), &frame.rows)),
                    )?;
                    let host = HostFrame::new(prepared, args, frame.rows.clone())?;
                    push_frame(
                        &mut frames,
                        EvalFrame::Program(frame),
                        budget,
                        tasks.storage(),
                    )?;
                    push_frame(&mut frames, EvalFrame::Host(host), budget, tasks.storage())?;
                } else if let RpnExpressionNode::OrdinaryFnCall { prepared, args } = node {
                    budget.storage(
                        retained.saturating_add(OrdinaryFrame::initial_storage(&frame.rows)),
                    )?;
                    let ordinary =
                        OrdinaryFrame::new(prepared, args, frame.rows.clone(), execution)?;
                    push_frame(
                        &mut frames,
                        EvalFrame::Program(frame),
                        budget,
                        tasks.storage(),
                    )?;
                    push_frame(
                        &mut frames,
                        EvalFrame::Ordinary(ordinary),
                        budget,
                        tasks.storage(),
                    )?;
                } else {
                    let leaf = frame.nodes.len() == 1;
                    if !leaf {
                        let used = match node {
                            RpnExpressionNode::FnCall { args_len, .. } => *args_len,
                            _ => 0,
                        };
                        let needed = frame.stack.len().saturating_sub(used).saturating_add(1);
                        reserve_stack(&mut frame, needed, &mut retained, budget)?;
                    }
                    // Charge space for the result while all argument values are
                    // still live, before invoking a kernel or input service.
                    budget.storage(
                        retained.saturating_add(frame.result_precharge(node, execution)),
                    )?;
                    let (used, value) = RpnExpression::eval_one_node(
                        ctx,
                        schema,
                        tasks.input,
                        &frame.rows,
                        occurrence,
                        budget,
                        node,
                        &frame.stack,
                        recorder.as_deref_mut(),
                        frame.flow,
                        execution,
                        retained,
                    )?;
                    budget.storage(retained.saturating_add(node_storage(&value, budget.mode())))?;
                    if leaf {
                        returned = Some(frame.leaf_result(value)?);
                    } else {
                        frame.stack.truncate(frame.stack.len() - used);
                        frame.stack.push(value);
                        push_frame(
                            &mut frames,
                            EvalFrame::Program(frame),
                            budget,
                            tasks.storage(),
                        )?;
                    }
                }
            }
            EvalFrame::Control(mut frame) => {
                if let Some(value) = returned.take() {
                    budget.charge()?;
                    let old_logical = if budget.mode() == StorageMode::ExactRetained {
                        frame.logical.as_ref().map_or(0, |logical| {
                            logical.retained_bytes_exact().unwrap_or(usize::MAX)
                        })
                    } else {
                        0
                    };
                    let extra = if budget.mode() == StorageMode::ConservativeInt {
                        int_storage_bytes(frame.rows.len())
                    } else if frame.logical.is_some() {
                        int_min_storage_bytes(frame.rows.len()).unwrap_or(usize::MAX)
                    } else {
                        0
                    };
                    budget.storage(retained.saturating_add(extra))?;
                    frame.accept(value, schema)?;
                    if budget.mode() == StorageMode::ExactRetained {
                        retained = frames_storage(&frames, frames.capacity(), budget.mode())
                            .saturating_add(frame.storage(budget.mode()))
                            .saturating_add(tasks.storage());
                        let new_logical = frame.logical.as_ref().map_or(0, |logical| {
                            logical.retained_bytes_exact().unwrap_or(usize::MAX)
                        });
                        if new_logical > old_logical {
                            budget.storage(retained.saturating_add(old_logical))?;
                        }
                        budget.storage(retained)?;
                    }
                }
                if let Some((child, rows)) = frame.request() {
                    budget.charge()?;
                    push_frame(
                        &mut frames,
                        EvalFrame::Control(frame),
                        budget,
                        tasks.storage(),
                    )?;
                    push_frame(
                        &mut frames,
                        EvalFrame::Program(ProgramFrame::new(child, rows)),
                        budget,
                        tasks.storage(),
                    )?;
                } else {
                    budget.storage(retained.saturating_add(frame.finish_precharge()))?;
                    let value = frame.finish()?;
                    budget.storage(
                        frames_storage(&frames, frames.capacity(), budget.mode())
                            .saturating_add(tasks.storage())
                            .saturating_add(value.retained_heap_bytes(budget.mode())),
                    )?;
                    returned = Some(value);
                }
            }
            EvalFrame::Ordinary(mut frame) => {
                if let Some(value) = returned.take() {
                    budget.charge()?;
                    frame.accept(value.into_unannotated()?, schema, execution)?;
                    retained = frames_storage(&frames, frames.capacity(), budget.mode())
                        .saturating_add(frame.storage(budget.mode()))
                        .saturating_add(tasks.storage());
                    budget.storage(retained)?;
                }
                if (execution == EvalExecution::SqlNumericBatch || !frame.stopped)
                    && frame.next < frame.args.len()
                {
                    budget.charge()?;
                    let index = frame.next;
                    frame.awaiting = Some(index);
                    let child = &frame.args[index];
                    budget.storage(retained.saturating_add(frame.rows.storage()))?;
                    let rows = frame.rows.clone();
                    push_frame(
                        &mut frames,
                        EvalFrame::Ordinary(frame),
                        budget,
                        tasks.storage(),
                    )?;
                    push_frame(
                        &mut frames,
                        EvalFrame::Program(ProgramFrame::new(child, rows)),
                        budget,
                        tasks.storage(),
                    )?;
                } else if execution == EvalExecution::SqlNumericBatch {
                    let value = frame.finish_numeric_batch(
                        ctx,
                        budget,
                        retained,
                        recorder.as_deref_mut(),
                    )?;
                    budget.storage(retained.saturating_add(node_storage(&value, budget.mode())))?;
                    returned = Some(FrameResult::unannotated(value));
                } else {
                    budget.charge()?;
                    // Both completed operand values are live during kernel work.
                    budget.storage(retained.saturating_add(int_storage_bytes(1)))?;
                    let value = if frame.stopped {
                        RpnStackNode::Vector {
                            value: RpnStackNodeVectorValue::Generated {
                                physical_value: VectorValue::from_scalar(
                                    &ScalarValue::Int(None),
                                    1,
                                ),
                            },
                            field_type: frame.prepared.return_type(),
                        }
                    } else {
                        // Raw legacy calls may have no physical coordinate;
                        // do not fabricate row0 for their failures.
                        let row = frame.rows.physical().first().map(|&input_row| InputRow {
                            occurrence,
                            input_row,
                        });
                        eval_ordinary_kernel(
                            ctx,
                            frame.prepared,
                            &frame.values,
                            row,
                            recorder.as_deref_mut(),
                        )?
                    };
                    validate_frame_result(&value, 1)?;
                    if !matches!(value.get_logical_scalar_ref(0), ScalarValueRef::Int(_)) {
                        return Err(LocalError::InvalidSpec(
                            "ordinary kernel result is not Int".into(),
                        ));
                    }
                    budget.storage(retained.saturating_add(node_storage(&value, budget.mode())))?;
                    returned = Some(FrameResult::unannotated(value));
                }
            }
            EvalFrame::Host(mut frame) => {
                if let Some(value) = returned.take() {
                    budget.charge()?;
                    budget.storage(retained.saturating_add(int_storage_bytes(1)))?;
                    frame.accept(value.into_unannotated()?)?;
                    retained = frames_storage(&frames, frames.capacity(), budget.mode())
                        .saturating_add(frame.storage(budget.mode()))
                        .saturating_add(tasks.storage());
                    budget.storage(retained)?;
                }

                if frame.task.is_none() {
                    budget.charge()?;
                    tasks.reserve_start(budget, &mut retained)?;
                    budget.storage(retained.saturating_add(int_storage_bytes(1)))?;
                    let start = tasks.host()?.start(ctx, frame.invocation(occurrence))?;
                    match start {
                        HostStart::Ready(value) => {
                            let value = frame.finish(value)?;
                            budget.storage(
                                frames_storage(&frames, frames.capacity(), budget.mode())
                                    .saturating_add(tasks.storage())
                                    .saturating_add(node_storage(&value, budget.mode())),
                            )?;
                            returned = Some(FrameResult::unannotated(value));
                            continue;
                        }
                        HostStart::Pending { task, request } => {
                            // Register before validating either the token or request;
                            // every reachable identity is now owned by cleanup.
                            tasks.arm(task)?;
                            frame.task = Some(task);
                            frame.request(request)?;
                        }
                    }
                } else if let Some(index) = frame.ready.take() {
                    budget.charge()?;
                    budget.storage(retained.saturating_add(int_storage_bytes(1)))?;
                    let task = frame.task.expect("resuming a registered task");
                    let step = tasks.host()?.resume(ctx, &task, frame.reply(index))?;
                    match step {
                        HostStep::NeedArg(request) => frame.request(request)?,
                        HostStep::Ready(value) => {
                            // Keep cleanup armed through validation/normalization,
                            // even when the provider has already completed its task.
                            let value = frame.finish(value)?;
                            budget.storage(
                                frames_storage(&frames, frames.capacity(), budget.mode())
                                    .saturating_add(tasks.storage())
                                    .saturating_add(node_storage(&value, budget.mode())),
                            )?;
                            tasks.complete(task)?;
                            returned = Some(FrameResult::unannotated(value));
                            continue;
                        }
                    }
                }

                let request = frame.request.take().expect("host requested an argument");
                budget.charge()?; // Reuse hits are not a free/infinite host loop.
                if request.mode == ArgMode::Fresh {
                    // Discard the old value before restarting; a failure cannot
                    // accidentally fall back to a previous successful reply.
                    frame.cache[request.index] = None;
                }
                if frame.cache[request.index].is_some() {
                    frame.ready = Some(request.index);
                    push_frame(&mut frames, EvalFrame::Host(frame), budget, tasks.storage())?;
                } else {
                    frame.awaiting = Some(request.index);
                    let child = &frame.args[request.index];
                    budget.storage(retained.saturating_add(frame.rows.storage()))?;
                    let rows = frame.rows.clone();
                    push_frame(&mut frames, EvalFrame::Host(frame), budget, tasks.storage())?;
                    push_frame(
                        &mut frames,
                        EvalFrame::Program(ProgramFrame::new(child, rows)),
                        budget,
                        tasks.storage(),
                    )?;
                }
            }
        }
    }
    Ok(returned.expect("root RPN frame returned a value"))
}

pub(crate) fn eval_logical_entry(
    kind: ControlKind,
    ctx: &mut EvalContext,
    schema: &[FieldType],
    columns: &LazyBatchColumnVec,
    logical_rows: &[usize],
    output_rows: usize,
    args: &[RpnExpression],
) -> Result<VectorValue> {
    assert!(args.len() >= 2);
    assert!(output_rows <= BATCH_MAX_SIZE);
    assert!(logical_rows.is_empty() || logical_rows.len() == output_rows);
    if output_rows == 0 {
        return Ok(VectorValue::with_capacity(
            0,
            tidb_query_datatype::EvalType::Int,
        ));
    }
    let field_type: FieldType = tidb_query_datatype::FieldTypeTp::LongLong.into();
    let frame = ControlFrame::new(
        kind,
        args,
        &field_type,
        FrameRows::Borrowed(logical_rows, output_rows),
        None,
    )
    .map_err(legacy_error)?;
    let result = eval_frames(
        EvalFrame::Control(frame),
        ctx,
        schema,
        &mut EvalInput::Decoded(columns),
        0,
        None,
        &mut EvalBudget::legacy(),
        None,
        EvalExecution::Unannotated,
    )
    .and_then(FrameResult::into_unannotated)
    .map_err(legacy_error)?;
    match result {
        RpnStackNode::Vector { value, .. } => value.take_vector_value(),
        RpnStackNode::Scalar { value, .. } => Ok(VectorValue::from_scalar(value, output_rows)),
    }
}

impl RpnExpression {
    /// Evaluates the expression into a vector.
    ///
    /// If referred columns are not decoded, they will be decoded according to
    /// the given schema.
    ///
    /// # Panics
    ///
    /// Panics if the expression is not valid.
    ///
    /// Panics when referenced column does not have equal length as specified in
    /// `rows`.
    pub fn eval<'a>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input_physical_columns: &'a mut LazyBatchColumnVec,
        input_logical_rows: &'a [usize],
        output_rows: usize,
    ) -> Result<RpnStackNode<'a>> {
        // We iterate two times. The first time we decode all referred columns. The
        // second time we evaluate. This is to make Rust's borrow checker happy
        // because there will be mutable reference during the first iteration
        // and we can't keep these references.
        self.ensure_columns_decoded(ctx, schema, input_physical_columns, input_logical_rows)?;
        self.eval_decoded(
            ctx,
            schema,
            input_physical_columns,
            input_logical_rows,
            output_rows,
        )
    }

    /// Decodes all referred columns which are not decoded. Then we ensure
    /// all referred columns are decoded.
    pub fn ensure_columns_decoded<'a>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input_physical_columns: &'a mut LazyBatchColumnVec,
        input_logical_rows: &[usize],
    ) -> Result<()> {
        let logical_rows = LogicalRows::from_slice(input_logical_rows);
        for &offset in self.referenced_column_offsets() {
            input_physical_columns[offset].ensure_decoded(ctx, &schema[offset], logical_rows)?;
        }
        Ok(())
    }

    /// Evaluates the expression into a stack node. The input columns must be
    /// already decoded.
    ///
    /// It differs from `eval` in that `eval_decoded` needn't receive a mutable
    /// reference to `LazyBatchColumnVec`. However, since `eval_decoded`
    /// doesn't decode columns, it will panic if referred columns are not
    /// decoded.
    ///
    /// # Panics
    ///
    /// Panics if the expression is not valid.
    ///
    /// Panics if referred columns are not decoded.
    ///
    /// Panics when referenced column does not have equal length as specified in
    /// `rows`.
    pub fn eval_decoded<'a>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input_physical_columns: &'a LazyBatchColumnVec,
        input_logical_rows: &'a [usize],
        output_rows: usize,
    ) -> Result<RpnStackNode<'a>> {
        let mut input = EvalInput::Decoded(input_physical_columns);
        self.eval_with_input(
            ctx,
            schema,
            &mut input,
            input_logical_rows,
            output_rows,
            0,
            None,
            &mut EvalBudget::legacy(),
        )
        .map_err(legacy_error)
    }

    pub(crate) fn eval_with_input<'a, 'data: 'a>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input: &mut EvalInput<'data, '_>,
        input_logical_rows: &'a [usize],
        output_rows: usize,
        occurrence: usize,
        host_catalog: Option<HostCatalogKey>,
        budget: &mut EvalBudget,
    ) -> LocalResult<RpnStackNode<'a>> {
        self.eval_with_input_recording(
            ctx,
            schema,
            input,
            input_logical_rows,
            output_rows,
            occurrence,
            host_catalog,
            budget,
            None,
        )
    }

    /// Optional failure-only observation. The recorder's borrow cannot escape
    /// in a returned RPN value; legacy callers always use the None wrapper.
    pub(crate) fn eval_with_input_recording<'a, 'data: 'a>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input: &mut EvalInput<'data, '_>,
        input_logical_rows: &'a [usize],
        output_rows: usize,
        occurrence: usize,
        host_catalog: Option<HostCatalogKey>,
        budget: &mut EvalBudget,
        recorder: Option<&mut FailureRecorder>,
    ) -> LocalResult<RpnStackNode<'a>> {
        assert!(output_rows > 0 && output_rows <= BATCH_MAX_SIZE);
        if self.checked_result_flow().is_some() || budget.mode() != StorageMode::ConservativeInt {
            return Err(LocalError::InvalidSpec(
                "lineaged expression requires its tagged entrypoint".into(),
            ));
        }
        eval_frames(
            EvalFrame::Program(ProgramFrame::new(
                self,
                FrameRows::Borrowed(input_logical_rows, output_rows),
            )),
            ctx,
            schema,
            input,
            occurrence,
            host_catalog,
            budget,
            recorder,
            EvalExecution::Unannotated,
        )
        .and_then(FrameResult::into_unannotated)
    }

    pub(crate) fn eval_with_input_lineaged<'a, 'data: 'a>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input: &mut EvalInput<'data, '_>,
        input_logical_rows: &'a [usize],
        output_rows: usize,
        occurrence: usize,
        host_catalog: Option<HostCatalogKey>,
        budget: &mut EvalBudget,
        recorder: Option<&mut FailureRecorder>,
    ) -> LocalResult<FrameResult<'a>> {
        if self.checked_result_flow().is_none()
            || budget.mode() != StorageMode::ExactRetained
            || host_catalog.is_some()
            || output_rows != 1
            || input_logical_rows.len() != 1
        {
            return Err(LocalError::InvalidSpec(
                "lineaged entry requires its checked singleton/annotation/budget domain".into(),
            ));
        }
        let result = eval_frames(
            EvalFrame::Program(ProgramFrame::new(
                self,
                FrameRows::Borrowed(input_logical_rows, output_rows),
            )),
            ctx,
            schema,
            input,
            occurrence,
            None,
            budget,
            recorder,
            EvalExecution::SqlControlLineage,
        )?;
        if result.meta.is_none() {
            return Err(LocalError::InvalidSpec(
                "lineaged expression returned no materialization ID".into(),
            ));
        }
        Ok(result)
    }

    /// Fixed whole-selection numeric entry. Its opaque local facade has already
    /// validated the stored compiled-entry tag, complete schema and row bounds.
    /// Empty selections are published by that facade without entering a frame.
    pub(crate) fn eval_with_input_numeric_batch<'a, 'data: 'a>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input: &mut EvalInput<'data, '_>,
        input_logical_rows: &'a [usize],
        budget: &mut EvalBudget,
        recorder: Option<&mut FailureRecorder>,
    ) -> LocalResult<RpnStackNode<'a>> {
        let output_rows = input_logical_rows.len();
        if self.checked_result_flow().is_some()
            || output_rows == 0
            || output_rows > BATCH_MAX_SIZE
            || !matches!(input, EvalInput::Bindings(_))
        {
            return Err(LocalError::InvalidSpec(
                "numeric batch requires its untagged bounded binding domain".into(),
            ));
        }
        eval_frames(
            EvalFrame::Program(ProgramFrame::new(
                self,
                FrameRows::Borrowed(input_logical_rows, output_rows),
            )),
            ctx,
            schema,
            input,
            0,
            None,
            budget,
            recorder,
            EvalExecution::SqlNumericBatch,
        )
        .and_then(FrameResult::into_unannotated)
    }

    /// Compatibility entry for the original ASCII-only ready-value worker.
    pub(crate) fn eval_with_ready_ascii<'a, 'data: 'a>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        ready: &'data ScalarValue,
        input_logical_rows: &'a [usize],
        witness: &mut EvaluatedAsciiWitness,
        budget: &mut EvalBudget,
    ) -> LocalResult<RpnStackNode<'a>> {
        self.eval_with_ready_bytes(
            EvaluatedBytesOp::Ascii,
            ctx,
            schema,
            ready,
            input_logical_rows,
            witness,
            budget,
        )
    }

    /// Closed value-boundary entry: the facade owns the ready nullable Bytes
    /// and has checked its private compiled tag. This always executes the fixed
    /// two-node program, including the selected official nullable wrapper for
    /// NULL.
    pub(crate) fn eval_with_ready_bytes<'a, 'data: 'a>(
        &'a self,
        operation: EvaluatedBytesOp,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        ready: &'data ScalarValue,
        input_logical_rows: &'a [usize],
        witness: &mut EvaluatedAsciiWitness,
        budget: &mut EvalBudget,
    ) -> LocalResult<RpnStackNode<'a>> {
        if operation.input_types() != [tidb_query_datatype::EvalType::Bytes] {
            return Err(LocalError::InvalidSpec(
                "evaluated Bytes compatibility entry requires one Bytes operand".into(),
            ));
        }
        self.eval_with_ready_args(
            operation,
            ctx,
            schema,
            std::slice::from_ref(ready),
            input_logical_rows,
            witness,
            budget,
        )
    }

    /// Fixed one-to-three ready operands, borrowed from the facade until its
    /// result extraction completes. Only the selected closed recipe is
    /// admitted.
    pub(crate) fn eval_with_ready_args<'a, 'data: 'a>(
        &'a self,
        operation: EvaluatedBytesOp,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        ready: &'data [ScalarValue],
        input_logical_rows: &'a [usize],
        witness: &mut EvaluatedAsciiWitness,
        budget: &mut EvalBudget,
    ) -> LocalResult<RpnStackNode<'a>> {
        if self.checked_result_flow().is_some()
            || input_logical_rows != [0]
            || !evaluated_bytes_shape(operation, self.as_ref(), schema)
        {
            return Err(LocalError::InvalidSpec(
                if operation == EvaluatedBytesOp::Ascii {
                    "evaluated ASCII requires its exact untagged two-node singleton recipe"
                } else if operation.input_types() == [tidb_query_datatype::EvalType::Bytes] {
                    "evaluated Bytes requires its exact selected untagged two-node singleton recipe"
                } else {
                    "evaluated arguments require their exact selected untagged singleton recipe"
                }
                .into(),
            ));
        }
        let mut input = EvalInput::ReadyArgs {
            values: ready,
            witness,
        };
        let execution = match operation {
            EvaluatedBytesOp::Ascii => EvalExecution::EvaluatedAscii,
            _ => EvalExecution::EvaluatedBytes(operation),
        };
        eval_frames(
            EvalFrame::Program(ProgramFrame::new(
                self,
                FrameRows::Borrowed(input_logical_rows, 1),
            )),
            ctx,
            schema,
            &mut input,
            0,
            None,
            budget,
            None,
            execution,
        )
        .and_then(FrameResult::into_unannotated)
    }

    #[inline]
    fn eval_one_node<'a, 'data: 'a>(
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input: &mut EvalInput<'data, '_>,
        rows: &FrameRows<'a>,
        occurrence: usize,
        budget: &mut EvalBudget,
        node: &'a RpnExpressionNode,
        stack: &[RpnStackNode<'a>],
        recorder: Option<&mut FailureRecorder>,
        flow: Option<CheckedResultFlow>,
        execution: EvalExecution,
        other_live_bytes: usize,
    ) -> LocalResult<(usize, RpnStackNode<'a>)> {
        let output_rows = rows.len();
        match node {
            RpnExpressionNode::Constant { value, field_type } => {
                Ok((0, RpnStackNode::Scalar { value, field_type }))
            }
            RpnExpressionNode::ColumnRef { offset } => {
                if matches!(
                    input,
                    EvalInput::ReadyBytes { .. } | EvalInput::ReadyArgs { .. }
                ) {
                    execution.check_input(input)?;
                    let operation = execution.evaluated_bytes_operation().ok_or_else(|| {
                        LocalError::InvalidSpec(
                            "ready Bytes cannot enter another input domain".into(),
                        )
                    })?;
                    let canonical = operation.input_field_type(*offset);
                    if output_rows != 1
                        || rows.physical() != [0]
                        || schema.len() != operation.input_types().len()
                        || canonical.is_none()
                        || schema.get(*offset) != canonical.as_ref()
                    {
                        return Err(LocalError::InvalidSpec(
                            "ready Bytes cannot enter another input domain".into(),
                        ));
                    }
                }
                let field_type = &schema[*offset];
                assert_eq!(rows.physical().len(), output_rows);
                let value = match input {
                    EvalInput::ReadyBytes { value, .. } => {
                        if execution.evaluated_bytes_operation().is_none()
                            || *offset != 0
                            || output_rows != 1
                            || !matches!(value, ScalarValue::Bytes(_))
                        {
                            return Err(LocalError::InvalidSpec(
                                "ready Bytes cannot enter another input domain".into(),
                            ));
                        }
                        let value: &'data ScalarValue = *value;
                        return Ok((0, RpnStackNode::Scalar { value, field_type }));
                    }
                    EvalInput::ReadyArgs { values, .. } => {
                        let values: &'data [ScalarValue] = *values;
                        let value = &values[*offset];
                        return Ok((0, RpnStackNode::Scalar { value, field_type }));
                    }
                    EvalInput::Decoded(columns) => {
                        let columns: &'data LazyBatchColumnVec = *columns;
                        let physical_value = columns[*offset].decoded();
                        match rows {
                            // Only caller-owned selections can escape as Ref.
                            FrameRows::Borrowed(logical_rows, _) => RpnStackNodeVectorValue::Ref {
                                physical_value,
                                logical_rows,
                            },
                            FrameRows::Owned(logical_rows, _) => {
                                let selected = RpnStackNodeVectorValue::Ref {
                                    physical_value,
                                    logical_rows,
                                }
                                .take_vector_value()
                                .map_err(LocalError::Evaluation)?;
                                RpnStackNodeVectorValue::Generated {
                                    physical_value: selected,
                                }
                            }
                        }
                    }
                    EvalInput::Bindings(services)
                        if execution == EvalExecution::SqlNumericBatch =>
                    {
                        let physical_value = read_numeric_binding_phase(
                            *services,
                            ctx,
                            *offset,
                            rows,
                            field_type,
                            budget,
                            other_live_bytes,
                            recorder,
                        )?;
                        RpnStackNodeVectorValue::Generated { physical_value }
                    }
                    EvalInput::Bindings(services) => {
                        if output_rows != 1 {
                            return Err(LocalError::BindingContract(
                                "binding import requires one occurrence".into(),
                            ));
                        }
                        budget.charge()?;
                        let row = InputRow {
                            occurrence,
                            input_row: rows.physical()[0],
                        };
                        let value =
                            read_binding_value(*services, ctx, *offset, row, field_type, recorder)?;
                        // A successful callback did not record anything. Reply
                        // validation/normalization failures must remain unsited.
                        let expected = match flow.map(CheckedResultFlow::carrier) {
                            None | Some(LineageCarrier::Int) => tidb_query_datatype::EvalType::Int,
                            Some(LineageCarrier::Bytes) => tidb_query_datatype::EvalType::Bytes,
                        };
                        if value.eval_type() != expected || value.len() != 1 {
                            return Err(LocalError::BindingContract(if flow.is_none() {
                                "read_input must return one Int value".into()
                            } else {
                                "lineage read_input must return its declared singleton carrier"
                                    .into()
                            }));
                        }
                        // Legacy Int keeps its bounded normalization contract.
                        // New lineaged owners are moved without a Bytes scalar
                        // roundtrip and their ACTUAL capacity is checked by the
                        // shared driver before retention/next semantic effect.
                        let value = if execution == EvalExecution::SqlControlLineage {
                            value
                        } else {
                            VectorValue::from_scalar(&value.get_scalar_ref(0).to_owned(), 1)
                        };
                        RpnStackNodeVectorValue::Generated {
                            physical_value: value,
                        }
                    }
                };
                Ok((0, RpnStackNode::Vector { value, field_type }))
            }
            RpnExpressionNode::ShortCircuitFnCall { .. }
            | RpnExpressionNode::HostCall { .. }
            | RpnExpressionNode::OrdinaryFnCall { .. } => {
                unreachable!("control/host/ordinary demand is scheduled by the frame driver")
            }
            RpnExpressionNode::FnCall {
                func_meta,
                args_len,
                field_type: ret_field_type,
                metadata,
            } => {
                // Suppose that we have a function call `Foo(A, B, C)`, the RPN nodes look like
                // `[A, B, C, Foo]`. The last N stack elements are its arguments.
                assert!(stack.len() >= *args_len);
                let stack_slice_begin = stack.len() - *args_len;
                let stack_slice = &stack[stack_slice_begin..];
                let value = eval_prepared_kernel(
                    ctx,
                    output_rows,
                    stack_slice,
                    *func_meta,
                    ret_field_type,
                    &**metadata,
                    match input {
                        EvalInput::ReadyBytes { witness, .. }
                        | EvalInput::ReadyArgs { witness, .. } => Some(&mut **witness),
                        _ => None,
                    },
                )?;
                Ok((*args_len, value))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use test::{Bencher, black_box};
    use tidb_query_codegen::rpn_fn;
    use tidb_query_common::Result;
    use tidb_query_datatype::{
        EvalType, FieldTypeAccessor, FieldTypeTp,
        codec::{
            batch::LazyBatchColumn,
            data_type::*,
            datum::{Datum, DatumEncoder},
        },
        expr::{EvalConfig, EvalContext, Flag},
    };
    use tipb::{FieldType, ScalarFuncSig};
    use tipb_helper::ExprDefBuilder;

    use super::*;
    use crate::{RpnExpressionBuilder, RpnFnMeta, impl_arithmetic::*, impl_compare::*};

    fn evaluated_ascii_test_recipe() -> (RpnExpression, Vec<FieldType>) {
        (
            RpnExpression::from(vec![
                RpnExpressionNode::ColumnRef { offset: 0 },
                RpnExpressionNode::FnCall {
                    func_meta: crate::impl_string::ascii_fn_meta(),
                    args_len: 1,
                    field_type: FieldTypeTp::LongLong.into(),
                    metadata: Box::new(()),
                },
            ]),
            vec![FieldTypeTp::Blob.into()],
        )
    }

    #[test]
    fn test_evaluated_ascii_input_domain_rejects_old_entries_and_leaf_shortcuts() {
        use crate::local::ExecutionLimits;
        let ready = ScalarValue::Bytes(None);
        let mut witness = EvaluatedAsciiWitness::default();
        let mut input = EvalInput::ReadyBytes {
            value: &ready,
            witness: &mut witness,
        };
        assert!(EvalExecution::EvaluatedAscii.check_input(&input).is_ok());
        for domain in [
            EvalExecution::Unannotated,
            EvalExecution::SqlControlLineage,
            EvalExecution::SqlNumericBatch,
        ] {
            assert!(domain.check_input(&input).is_err());
        }
        let columns = LazyBatchColumnVec::empty();
        assert!(
            EvalExecution::EvaluatedAscii
                .check_input(&EvalInput::Decoded(&columns))
                .is_err()
        );
        let leaf = RpnExpression::from(vec![RpnExpressionNode::Constant {
            value: ScalarValue::Int(None),
            field_type: FieldTypeTp::LongLong.into(),
        }]);
        for domain in [EvalExecution::Unannotated, EvalExecution::EvaluatedAscii] {
            let mut budget = if domain == EvalExecution::Unannotated {
                EvalBudget::local(ExecutionLimits::default(), 1).unwrap()
            } else {
                EvalBudget::exact(ExecutionLimits::default()).unwrap()
            };
            let result = eval_frames(
                EvalFrame::Program(ProgramFrame::new(&leaf, FrameRows::Borrowed(&[0], 1))),
                &mut EvalContext::default(),
                &[],
                &mut input,
                0,
                None,
                &mut budget,
                None,
                domain,
            );
            assert!(matches!(result, Err(LocalError::InvalidSpec(_))));
        }
        assert_eq!(witness.invocations(), 0);
    }

    #[test]
    fn test_evaluated_ascii_ready_capacity_stands_outside_scalar_and_host_ledger() {
        use crate::local::ExecutionLimits;
        let ready = ScalarValue::Bytes(Some(Vec::with_capacity(4096)));
        let capacity = match &ready {
            ScalarValue::Bytes(Some(bytes)) => bytes.capacity(),
            _ => unreachable!(),
        };
        let mut witness = EvaluatedAsciiWitness::default();
        let field_type = FieldTypeTp::Blob.into();
        assert_eq!(
            node_storage(
                &RpnStackNode::Scalar {
                    value: &ready,
                    field_type: &field_type,
                },
                StorageMode::ExactRetained,
            ),
            0
        );
        {
            let mut input = EvalInput::ReadyBytes {
                value: &ready,
                witness: &mut witness,
            };
            let mut guard = TaskGuard {
                input: &mut input,
                catalog: None,
                live: Vec::new(),
            };
            assert_eq!(guard.storage(), capacity);
            assert!(guard.host().is_err());
            let budget = EvalBudget::exact(ExecutionLimits::default()).unwrap();
            let mut retained = capacity;
            assert!(matches!(
                guard.reserve_start(&budget, &mut retained),
                Err(LocalError::InvalidSpec(_))
            ));
            assert_eq!(retained, capacity);
            assert_eq!(guard.storage(), capacity);
        }
        let (program, schema) = evaluated_ascii_test_recipe();
        let mut budget = EvalBudget::exact(ExecutionLimits {
            max_retained_bytes: capacity - 1,
            ..ExecutionLimits::default()
        })
        .unwrap();
        assert!(matches!(
            program.eval_with_ready_ascii(
                &mut EvalContext::default(),
                &schema,
                &ready,
                &[0],
                &mut witness,
                &mut budget,
            ),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(witness.invocations(), 0);
    }

    #[test]
    fn test_evaluated_ascii_wrapper_dispatches_null_empty_and_raw_ready_values() {
        use crate::local::ExecutionLimits;
        let (program, schema) = evaluated_ascii_test_recipe();
        let mut witness = EvaluatedAsciiWitness::default();
        let mut ctx = EvalContext::default();
        for (bytes, expected) in [
            (None, None),
            (Some(vec![]), Some(0)),
            (Some(vec![255, 0]), Some(255)),
        ] {
            let ready = ScalarValue::Bytes(bytes);
            let before = witness.invocations();
            let mut budget = EvalBudget::exact(ExecutionLimits::default()).unwrap();
            let output = program
                .eval_with_ready_ascii(&mut ctx, &schema, &ready, &[0], &mut witness, &mut budget)
                .unwrap();
            assert_eq!(witness.invocations(), before + 1);
            match output {
                RpnStackNode::Vector {
                    value:
                        RpnStackNodeVectorValue::Generated {
                            physical_value: VectorValue::Int(values),
                        },
                    field_type,
                } => {
                    assert_eq!(values.len(), 1);
                    assert_eq!(ChunkRef::get_option_ref(&values, 0).copied(), expected);
                    assert_eq!(*field_type, FieldType::from(FieldTypeTp::LongLong));
                }
                _ => panic!("ready ASCII did not return its owned singleton Int"),
            }
        }
        assert_eq!(ctx.warnings.warning_cnt, 0);
        assert!(ctx.warnings.warnings.is_empty());
    }

    #[test]
    fn test_evaluated_ascii_requires_official_recipe_without_restricting_legacy_rpn() {
        use crate::local::ExecutionLimits;
        let (mut program, schema) = evaluated_ascii_test_recipe();
        let ready = ScalarValue::Bytes(Some(vec![255]));
        let columns = LazyBatchColumnVec::from(vec![VectorValue::from_scalar(&ready, 1)]);
        // Existing raw/wire RPN ASCII remains legal in its original domain.
        let mut input = EvalInput::Decoded(&columns);
        let mut budget = EvalBudget::local(ExecutionLimits::default(), 1).unwrap();
        let result = eval_frames(
            EvalFrame::Program(ProgramFrame::new(&program, FrameRows::Borrowed(&[0], 1))),
            &mut EvalContext::default(),
            &schema,
            &mut input,
            0,
            None,
            &mut budget,
            None,
            EvalExecution::Unannotated,
        )
        .unwrap();
        match result.node.get_logical_scalar_ref(0) {
            ScalarValueRef::Int(Some(value)) => assert_eq!(*value, 255),
            _ => panic!("legacy ASCII changed"),
        }
        drop(result);
        if let RpnExpressionNode::FnCall { func_meta, .. } = &mut program[1] {
            // A forged display name alone cannot authorize a different kernel.
            func_meta.fn_ptr = |_, _, _, _, _| panic!("wrong kernel entered");
        } else {
            unreachable!();
        }
        let mut witness = EvaluatedAsciiWitness::default();
        let mut budget = EvalBudget::exact(ExecutionLimits::default()).unwrap();
        assert!(matches!(
            program.eval_with_ready_ascii(
                &mut EvalContext::default(),
                &schema,
                &ready,
                &[0],
                &mut witness,
                &mut budget,
            ),
            Err(LocalError::InvalidSpec(_))
        ));
        assert_eq!(witness.invocations(), 0);
    }

    #[test]
    fn test_evaluated_bytes_rejects_same_carrier_operation_and_kernel_drift() {
        use crate::local::ExecutionLimits;
        let ready = ScalarValue::Bytes(None);
        let schema = [FieldType::from(FieldTypeTp::Blob)];
        let mut witness = EvaluatedAsciiWitness::default();
        for (operation, other) in [
            (EvaluatedBytesOp::Length, EvaluatedBytesOp::BitLength),
            (EvaluatedBytesOp::LTrim, EvaluatedBytesOp::RTrim),
            (EvaluatedBytesOp::Crc32, EvaluatedBytesOp::Length),
            (EvaluatedBytesOp::Reverse, EvaluatedBytesOp::ReverseUtf8),
            (
                EvaluatedBytesOp::CharLength,
                EvaluatedBytesOp::CharLengthUtf8,
            ),
            (EvaluatedBytesOp::Quote, EvaluatedBytesOp::UnHex),
            (EvaluatedBytesOp::Md5, EvaluatedBytesOp::Sha1),
        ] {
            // Isolate each guard: neither a matching carrier nor a matching
            // display name grants admission for a different kernel or metadata.
            for mismatch in 0..5 {
                let mut selected = operation;
                let mut func_meta = operation.fn_meta();
                let mut field_type = operation.return_type();
                let mut metadata: Box<dyn std::any::Any + Send> = Box::new(());
                match mismatch {
                    0 => selected = other,
                    1 => func_meta.name = other.fn_meta().name,
                    2 => func_meta.fn_ptr = other.fn_meta().fn_ptr,
                    3 => metadata = Box::new(false),
                    _ => field_type.set_flen(1),
                }
                let program = RpnExpression::from(vec![
                    RpnExpressionNode::ColumnRef { offset: 0 },
                    RpnExpressionNode::FnCall {
                        func_meta,
                        args_len: 1,
                        field_type,
                        metadata,
                    },
                ]);
                let mut budget = EvalBudget::exact(ExecutionLimits::default()).unwrap();
                assert!(matches!(
                    program.eval_with_ready_bytes(
                        selected,
                        &mut EvalContext::default(),
                        &schema,
                        &ready,
                        &[0],
                        &mut witness,
                        &mut budget,
                    ),
                    Err(LocalError::InvalidSpec(_))
                ));
            }
        }
        assert_eq!(witness.invocations(), 0);
    }

    #[test]
    fn test_evaluated_composite_requires_exact_call_chain() {
        let call = |primitive: EvaluatedBytesOp| RpnExpressionNode::FnCall {
            func_meta: primitive.fn_meta(),
            args_len: 1,
            field_type: primitive.return_type(),
            metadata: Box::new(()),
        };
        let schema = [FieldType::from(FieldTypeTp::LongLong)];
        let operation = EvaluatedBytesOp::IsNotNull;
        let mut nodes = vec![
            RpnExpressionNode::ColumnRef { offset: 0 },
            call(EvaluatedBytesOp::IsNull),
            call(EvaluatedBytesOp::UnaryNot),
        ];
        assert!(evaluated_bytes_shape(operation, &nodes, &schema));
        assert!(!evaluated_bytes_shape(
            EvaluatedBytesOp::IsNull,
            &nodes,
            &schema
        ));
        assert!(!evaluated_bytes_shape(operation, &nodes[..2], &schema));
        nodes.push(call(EvaluatedBytesOp::UnaryNot));
        assert!(!evaluated_bytes_shape(operation, &nodes, &schema));
        let _ = nodes.pop();
        nodes.swap(1, 2);
        assert!(!evaluated_bytes_shape(operation, &nodes, &schema));
        nodes.swap(1, 2);
        nodes[1] = call(EvaluatedBytesOp::IsTrue);
        assert!(!evaluated_bytes_shape(operation, &nodes, &schema));
        nodes[1] = call(EvaluatedBytesOp::IsNull);
        if let RpnExpressionNode::FnCall { func_meta, .. } = &mut nodes[2] {
            func_meta.name = EvaluatedBytesOp::IsNull.fn_meta().name;
        }
        assert!(!evaluated_bytes_shape(operation, &nodes, &schema));
        if let RpnExpressionNode::FnCall { func_meta, .. } = &mut nodes[2] {
            *func_meta = EvaluatedBytesOp::UnaryNot.fn_meta();
            func_meta.fn_ptr = EvaluatedBytesOp::IsNull.fn_meta().fn_ptr;
        }
        assert!(!evaluated_bytes_shape(operation, &nodes, &schema));
    }

    #[test]
    fn test_evaluated_int2_rejects_kernel_identity_and_wrong_kind() {
        use crate::local::ExecutionLimits;
        let schema = [
            FieldType::from(FieldTypeTp::LongLong),
            FieldType::from(FieldTypeTp::LongLong),
        ];
        let mut witness = EvaluatedAsciiWitness::default();
        for (operation, other) in [
            (EvaluatedBytesOp::BitAnd, EvaluatedBytesOp::BitOr),
            (EvaluatedBytesOp::LogicalAnd, EvaluatedBytesOp::LogicalOr),
        ] {
            let other = other.fn_meta();
            for mismatch in 0..3 {
                let mut func_meta = operation.fn_meta();
                let mut ready = [ScalarValue::Int(Some(6)), ScalarValue::Int(Some(3))];
                match mismatch {
                    0 => func_meta.name = other.name,
                    1 => func_meta.fn_ptr = other.fn_ptr,
                    _ => ready[1] = ScalarValue::Bytes(None),
                }
                let program = RpnExpression::from(vec![
                    RpnExpressionNode::ColumnRef { offset: 0 },
                    RpnExpressionNode::ColumnRef { offset: 1 },
                    RpnExpressionNode::FnCall {
                        func_meta,
                        args_len: 2,
                        field_type: operation.return_type(),
                        metadata: Box::new(()),
                    },
                ]);
                let mut budget = EvalBudget::exact(ExecutionLimits::default()).unwrap();
                assert!(matches!(
                    program.eval_with_ready_args(
                        operation,
                        &mut EvalContext::default(),
                        &schema,
                        &ready,
                        &[0],
                        &mut witness,
                        &mut budget,
                    ),
                    Err(LocalError::InvalidSpec(_))
                ));
            }
        }
        assert_eq!(witness.invocations(), 0);
    }

    #[test]
    fn test_evaluated_ascii_witness_overflow_and_actual_wrapper_error() {
        use crate::local::ExecutionLimits;
        let (program, schema) = evaluated_ascii_test_recipe();
        let ready = ScalarValue::Bytes(None);
        let mut witness = EvaluatedAsciiWitness {
            invocations: u64::MAX,
        };
        let mut budget = EvalBudget::exact(ExecutionLimits::default()).unwrap();
        assert!(matches!(
            program.eval_with_ready_ascii(
                &mut EvalContext::default(),
                &schema,
                &ready,
                &[0],
                &mut witness,
                &mut budget,
            ),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(witness.invocations(), u64::MAX);

        // A shared-helper fixture, not an admitted C4 recipe: an actual Err at
        // dispatch is preserved and counted once, without retries or a fake site.
        let mut meta = crate::impl_string::ascii_fn_meta();
        meta.fn_ptr = |_, _, _, _, _| {
            Err(tidb_query_common::Error::from(
                tidb_query_common::error::EvaluateError::Other("owned wrapper sentinel".into()),
            ))
        };
        let field_type = FieldTypeTp::LongLong.into();
        let mut witness = EvaluatedAsciiWitness::default();
        let error = eval_prepared_kernel(
            &mut EvalContext::default(),
            1,
            &[],
            meta,
            &field_type,
            &(),
            Some(&mut witness),
        )
        .unwrap_err();
        match error {
            LocalError::Evaluation(error) => match *error.0 {
                tidb_query_common::error::ErrorInner::Evaluate(
                    tidb_query_common::error::EvaluateError::Other(message),
                ) => assert_eq!(message, "owned wrapper sentinel"),
                _ => panic!("original wrapper error kind was replaced"),
            },
            _ => panic!("actual wrapper error was replaced"),
        }
        assert_eq!(witness.invocations(), 1);
    }

    static SHORT_CIRCUIT_RHS_EVAL_COUNT: AtomicUsize = AtomicUsize::new(0);

    fn short_circuit_context() -> EvalContext {
        EvalContext::new(Arc::new(EvalConfig::from_flag(
            Flag::ENABLE_SHORT_CIRCUIT_EXPRESSION,
        )))
    }

    #[rpn_fn(nullable)]
    fn short_circuit_counted_identity(v: Option<&Int>) -> Result<Option<Int>> {
        SHORT_CIRCUIT_RHS_EVAL_COUNT.fetch_add(1, Ordering::SeqCst);
        Ok(v.copied())
    }

    #[rpn_fn(nullable)]
    fn short_circuit_unreachable(_v: Option<&Int>) -> Result<Option<Int>> {
        unreachable!("short-circuited argument must not be evaluated")
    }

    /// Single constant node
    #[test]
    fn test_eval_single_constant_node() {
        let exp = RpnExpressionBuilder::new_for_test()
            .push_constant_for_test(1.5f64)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        let result = exp.eval(&mut ctx, &[], &mut columns, &[], 10);
        let val = result.unwrap();
        assert!(val.is_scalar());
        assert_eq!(
            val.scalar_value().unwrap().as_real(),
            Real::new(1.5).ok().as_ref()
        );
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::Double);
    }

    #[test]
    fn test_lineage_frame_layout_storage_is_actual() {
        let frame = std::mem::size_of::<EvalFrame<'_>>();
        println!(
            "lineage frame layout: EvalFrame={} ProgramFrame={} ControlFrame={} FrameResult={} RpnStackNode={}",
            frame,
            std::mem::size_of::<ProgramFrame<'_>>(),
            std::mem::size_of::<ControlFrame<'_>>(),
            std::mem::size_of::<FrameResult<'_>>(),
            std::mem::size_of::<RpnStackNode<'_>>()
        );
        for mode in [StorageMode::ConservativeInt, StorageMode::ExactRetained] {
            assert_eq!(frames_storage(&[], 3, mode), 3 * frame);
        }
        // Today's real allocation unit, NOT version-stable absolute budget
        // cutoffs. Old size_of-based fixtures cannot prove identical prefixes.
    }

    #[test]
    fn test_lineage_selected_owner_is_counted_through_control_and_program() {
        let mode = StorageMode::ExactRetained;
        let field_type: FieldType = FieldTypeTp::VarString.into();
        let mut bytes = ChunkedVecBytes::try_with_capacities(1, 8192).unwrap();
        bytes.push_ref(Some(b"x"));
        let heap = bytes.retained_heap_bytes().unwrap();
        let id = ResultMetaId::new(7, 3);
        let node = RpnStackNode::Vector {
            value: RpnStackNodeVectorValue::Generated {
                physical_value: VectorValue::Bytes(bytes),
            },
            field_type: &field_type,
        };
        // Storage-only private fixture: it is never admitted/evaluated as an
        // empty-argument control. The moved payload includes unused capacity.
        let mut control = ControlFrame {
            kind: ControlKind::IfNull,
            args: &[],
            field_type: &field_type,
            rows: FrameRows::Borrowed(&[0], 1),
            next: None,
            awaiting: None,
            value: None,
            flow: Some(CheckedResultFlow::PreserveSelected {
                generated_null: ResultMetaId::new(7, 1),
                carrier: LineageCarrier::Bytes,
            }),
            selected: Some(FrameResult {
                node,
                meta: Some(id),
            }),
            logical: None,
            pending: None,
        };
        assert_eq!(control.storage(mode), heap);
        let selected = control.selected.take().unwrap();
        assert_eq!(selected.retained_heap_bytes(mode), heap);
        assert_eq!(control.storage(mode), 0);
        let expression = RpnExpression::from(vec![]);
        let mut program = ProgramFrame::new(&expression, FrameRows::Borrowed(&[0], 1));
        program.stack.push(selected.node);
        program.result_meta = selected.meta;
        let stack_bytes = program.stack.capacity() * std::mem::size_of::<RpnStackNode<'_>>();
        let frame = EvalFrame::Program(program);
        assert_eq!(frame.storage(mode), heap + stack_bytes);
        assert_eq!(
            frames_storage(&[frame], 1, mode),
            std::mem::size_of::<EvalFrame<'_>>() + heap + stack_bytes
        );
    }

    #[test]
    fn test_numeric_execution_and_lineage_are_not_accounting_flags() {
        use crate::local::ExecutionLimits;
        let exact = EvalBudget::exact(ExecutionLimits::default()).unwrap();
        let row = EvalBudget::local(ExecutionLimits::default(), 1).unwrap();
        assert!(EvalExecution::Unannotated.check_budget(&row).is_ok());
        assert!(EvalExecution::Unannotated.check_budget(&exact).is_err());
        for domain in [
            EvalExecution::SqlControlLineage,
            EvalExecution::SqlNumericBatch,
        ] {
            assert!(domain.check_budget(&exact).is_ok());
            assert!(domain.check_budget(&row).is_err());
        }
        let make = || {
            RpnExpression::from(vec![RpnExpressionNode::Constant {
                value: ScalarValue::Int(None),
                field_type: FieldTypeTp::LongLong.into(),
            }])
        };
        let raw = make();
        let frame = ProgramFrame::new(&raw, FrameRows::Borrowed(&[0], 1));
        assert!(
            frame
                .check_execution(EvalExecution::SqlNumericBatch, &[])
                .is_ok()
        );
        assert!(
            frame
                .check_execution(EvalExecution::SqlControlLineage, &[])
                .is_err()
        );
        // An untagged raw leaf has no producer-domain identity. The opaque
        // LocalProgram entry tag, not this raw node, prevents row-facade escape.
        assert!(
            frame
                .check_execution(EvalExecution::Unannotated, &[])
                .is_ok()
        );
        let tagged = make()
            .with_result_flow(CheckedResultFlow::Leaf {
                id: ResultMetaId::new(81, 7),
                carrier: LineageCarrier::Int,
            })
            .unwrap();
        let frame = ProgramFrame::new(&tagged, FrameRows::Borrowed(&[0], 1));
        assert!(
            frame
                .check_execution(EvalExecution::SqlControlLineage, &[])
                .is_ok()
        );
        assert!(
            frame
                .check_execution(EvalExecution::SqlNumericBatch, &[])
                .is_err()
        );
        assert!(
            frame
                .check_execution(EvalExecution::Unannotated, &[])
                .is_err()
        );
        let frame = ProgramFrame::new(&tagged, FrameRows::Borrowed(&[0, 0], 2));
        assert!(
            frame
                .check_execution(EvalExecution::SqlControlLineage, &[])
                .is_err()
        );
    }

    #[test]
    fn test_numeric_import_charges_suspended_base_and_actual_reply() {
        use crate::local::ExecutionLimits;
        struct Services {
            schema: Vec<FieldType>,
            rows: Vec<InputRow>,
            capacity: usize,
        }
        impl LocalRuntimeServices for Services {
            fn binding_schema(&self) -> &[FieldType] {
                &self.schema
            }
            fn read_input(
                &mut self,
                _: &mut EvalContext,
                _: usize,
                row: InputRow,
                _: &FieldType,
            ) -> LocalResult<VectorValue> {
                self.rows.push(row);
                let mut values = ChunkedVecSized::<Int>::with_capacity(self.capacity);
                values.push(Some(row.input_row as i64));
                Ok(VectorValue::Int(values))
            }
            fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
                panic!("numeric imports must not request host services")
            }
        }
        let field_type: FieldType = FieldTypeTp::LongLong.into();
        let rows = FrameRows::Borrowed(&[2, 0, 2], 3);
        let base = 4096;
        let output_bytes =
            int_vector_storage_bytes(&ChunkedVecSized::<Int>::with_capacity(3)).unwrap();
        for (limit, capacity, expected_reads) in [
            (base - 1, 1, 0),
            (
                base + output_bytes + int_min_storage_bytes(1).unwrap(),
                256,
                1,
            ),
        ] {
            let mut services = Services {
                schema: vec![field_type.clone()],
                rows: Vec::new(),
                capacity,
            };
            let mut budget = EvalBudget::exact(ExecutionLimits {
                max_retained_bytes: limit,
                ..ExecutionLimits::default()
            })
            .unwrap();
            let mut recorder = FailureRecorder::default();
            let error = read_numeric_binding_phase(
                &mut services,
                &mut EvalContext::default(),
                0,
                &rows,
                &field_type,
                &mut budget,
                base,
                Some(&mut recorder),
            )
            .unwrap_err();
            assert!(matches!(error, LocalError::ResourceLimit(_)));
            assert_eq!(services.rows.len(), expected_reads);
            assert!(recorder.into_failure(error).site().is_none());
        }
        let mut services = Services {
            schema: vec![field_type.clone()],
            rows: Vec::new(),
            capacity: 1,
        };
        let mut budget = EvalBudget::exact(ExecutionLimits::default()).unwrap();
        let value = read_numeric_binding_phase(
            &mut services,
            &mut EvalContext::default(),
            0,
            &rows,
            &field_type,
            &mut budget,
            base,
            None,
        )
        .unwrap();
        let mut expected = ChunkedVecSized::<Int>::with_capacity(3);
        for item in [2i64, 0, 2] {
            expected.push(Some(item));
        }
        assert_eq!(value, VectorValue::Int(expected));
        assert_eq!(
            services.rows,
            vec![
                InputRow {
                    occurrence: 0,
                    input_row: 2
                },
                InputRow {
                    occurrence: 1,
                    input_row: 0
                },
                InputRow {
                    occurrence: 2,
                    input_row: 2
                }
            ]
        );
    }

    #[test]
    fn test_ordinary_operands_stay_charged_before_right_and_kernel() {
        use crate::local::*;
        struct Services {
            schema: Vec<FieldType>,
            reads: usize,
        }
        impl LocalRuntimeServices for Services {
            fn binding_schema(&self) -> &[FieldType] {
                &self.schema
            }
            fn read_input(
                &mut self,
                _: &mut EvalContext,
                slot: usize,
                _: InputRow,
                _: &FieldType,
            ) -> LocalResult<VectorValue> {
                self.reads += 1;
                Ok(VectorValue::from_scalar(
                    &ScalarValue::Int(Some(if slot == 0 { i64::MAX } else { 1 })),
                    1,
                ))
            }
            fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
                panic!("ordinary profile cannot request hosts")
            }
        }
        let field_type: FieldType = FieldTypeTp::LongLong.into();
        let schema = vec![field_type.clone(); 2];
        let spec = LocalExpr::Call {
            function: FunctionRef::TiPb(tipb::ScalarFuncSig::PlusInt),
            args: (0..2)
                .map(|slot| LocalExpr::InputSlot {
                    slot,
                    field_type: field_type.clone(),
                })
                .collect(),
            return_type: field_type,
            metadata: CallMetadata::None,
        };
        let facts = OrdinaryProfileSpec::new(
            &spec,
            &schema,
            OrdinaryProfile::TypedRow,
            vec![OrdinaryCallSite::typed_row(0, OrdinarySourceId::new(1, 7))],
            CompileLimits::default(),
        )
        .unwrap();
        let mut program =
            compile_local_profiled(&spec, &schema, LocalCompileContext::default(), &facts).unwrap();
        let base =
            3 * std::mem::size_of::<EvalFrame<'_>>() + 2 * std::mem::size_of::<RpnStackNode<'_>>();
        let int = int_storage_bytes(1);
        // Output + left + right + kernel result overlap. The final case must
        // actually reach overflow; smaller caps stop at the declared prefix.
        for (copies, reads, reaches_kernel) in
            [(1, 0, false), (2, 1, false), (3, 2, false), (4, 2, true)]
        {
            let mut services = Services {
                schema: schema.clone(),
                reads: 0,
            };
            let result = program.eval_with_bindings(
                &mut LocalEvalState::with_limits(ExecutionLimits {
                    max_retained_bytes: base + copies * int,
                    ..ExecutionLimits::default()
                }),
                &mut EvalContext::default(),
                1,
                &[0],
                &mut services,
            );
            if reaches_kernel {
                assert!(matches!(result, Err(LocalError::Evaluation(_))));
            } else {
                assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
            }
            assert_eq!(services.reads, reads);
        }
    }

    #[test]
    fn test_host_ledger_and_reply_storage_precede_later_effects() {
        use crate::local::*;
        struct Services {
            catalog: HostCatalog,
            schema: Vec<FieldType>,
            starts: usize,
            reads: usize,
            resumes: usize,
            cancels: usize,
            active: bool,
        }
        impl LocalRuntimeServices for Services {
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
                Ok(VectorValue::from_scalar(&ScalarValue::Int(Some(9)), 1))
            }
            fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
                Some(self)
            }
        }
        impl LocalHostServices for Services {
            fn catalog_key(&self) -> &HostCatalogKey {
                self.catalog.key()
            }
            fn start(
                &mut self,
                _: &mut EvalContext,
                _: HostInvocation<'_>,
            ) -> LocalResult<HostStart> {
                self.starts += 1;
                self.active = true;
                Ok(HostStart::Pending {
                    task: HostTaskId {
                        slot: 1,
                        generation: 1,
                    },
                    request: HostArgRequest {
                        index: 0,
                        mode: ArgMode::Fresh,
                    },
                })
            }
            fn resume(
                &mut self,
                _: &mut EvalContext,
                _: &HostTaskId,
                _: HostArgReply<'_>,
            ) -> LocalResult<HostStep> {
                self.resumes += 1;
                self.active = false;
                Ok(HostStep::Ready(VectorValue::from_scalar(
                    &ScalarValue::Int(Some(9)),
                    1,
                )))
            }
            fn cancel(&mut self, _: &HostTaskId) {
                self.cancels += 1;
                self.active = false;
            }
        }
        let field_type: FieldType = FieldTypeTp::LongLong.into();
        let catalog = HostCatalog::new(vec![HostSignature {
            arg_types: vec![field_type.clone()].into_boxed_slice(),
            return_type: field_type.clone(),
        }])
        .unwrap();
        let spec = LocalExpr::HostCall {
            slot: catalog.slot(0).unwrap(),
            args: vec![LocalExpr::InputSlot {
                slot: 0,
                field_type: field_type.clone(),
            }]
            .into_boxed_slice(),
            return_type: field_type.clone(),
        };
        let mut program = compile_local_with_hosts(
            &spec,
            &[field_type.clone()],
            LocalCompileContext::default(),
            &catalog,
        )
        .unwrap();
        let frame = std::mem::size_of::<EvalFrame<'_>>();
        let cache = std::mem::size_of::<Option<VectorValue>>();
        let task = std::mem::size_of::<HostTaskId>();
        let int = int_storage_bytes(1);
        // Each cap omits required storage: ledger before start, ledger while
        // pushing/executing the child, then owned cached reply while
        // the generated child is still live. Existing diagnostics/effects stop
        // at those boundaries and known tasks are cleaned up exactly once.
        for (bytes, starts, reads, cancels) in [
            (2 * frame + cache + 2 * int, 0, 0, 0),
            (3 * frame + cache + int, 1, 0, 1),
            (3 * frame + cache + 2 * int, 1, 0, 1),
            (3 * frame + cache + task + 2 * int, 1, 1, 1),
        ] {
            let mut services = Services {
                catalog: catalog.clone(),
                schema: vec![field_type.clone()],
                starts: 0,
                reads: 0,
                resumes: 0,
                cancels: 0,
                active: false,
            };
            let result = program.eval_with_bindings(
                &mut LocalEvalState::with_limits(ExecutionLimits {
                    max_retained_bytes: bytes,
                    ..ExecutionLimits::default()
                }),
                &mut EvalContext::default(),
                1,
                &[0],
                &mut services,
            );
            assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
            assert_eq!(
                (
                    services.starts,
                    services.reads,
                    services.resumes,
                    services.cancels
                ),
                (starts, reads, 0, cancels)
            );
            assert!(!services.active);
        }
    }

    #[test]
    fn test_control_return_stack_storage_is_charged_before_success() {
        use crate::local::*;
        let field_type: FieldType = FieldTypeTp::LongLong.into();
        let spec = LocalExpr::Call {
            function: FunctionRef::TiPb(tipb::ScalarFuncSig::CaseWhenInt),
            args: Box::new([]),
            return_type: field_type,
            metadata: CallMetadata::None,
        };
        let mut program = compile_local(&spec, &[], LocalCompileContext::default()).unwrap();
        let peak = int_storage_bytes(1) * 2
            + std::mem::size_of::<EvalFrame<'_>>() * 2
            + std::mem::size_of::<RpnStackNode<'_>>();
        let columns = LazyBatchColumnVec::empty();
        let evaluate = |program: &mut LocalProgram, bytes| {
            program.eval(
                &mut LocalEvalState::with_limits(ExecutionLimits {
                    max_retained_bytes: bytes,
                    ..ExecutionLimits::default()
                }),
                &mut EvalContext::default(),
                LocalBatch {
                    columns: &columns,
                    physical_rows: 1,
                    selection: &[0],
                },
            )
        };
        assert!(matches!(
            evaluate(&mut program, peak - 1),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(
            evaluate(&mut program, peak).unwrap().to_int_vec(),
            vec![None]
        );
    }

    #[test]
    fn test_control_return_refreshes_storage_before_next_input_effect() {
        use crate::local::*;
        struct Services {
            schema: Vec<FieldType>,
            reads: usize,
        }
        impl LocalRuntimeServices for Services {
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
                Ok(VectorValue::from_scalar(&ScalarValue::Int(Some(5)), 1))
            }
        }
        let field_type: FieldType = FieldTypeTp::LongLong.into();
        let empty = LocalExpr::Call {
            function: FunctionRef::TiPb(tipb::ScalarFuncSig::CaseWhenInt),
            args: Box::new([]),
            return_type: field_type.clone(),
            metadata: CallMetadata::None,
        };
        let spec = LocalExpr::Call {
            function: FunctionRef::TiPb(tipb::ScalarFuncSig::PlusIntSignedSigned),
            args: vec![
                empty,
                LocalExpr::InputSlot {
                    slot: 0,
                    field_type: field_type.clone(),
                },
            ]
            .into_boxed_slice(),
            return_type: field_type.clone(),
            metadata: CallMetadata::None,
        };
        let mut services = Services {
            schema: vec![field_type],
            reads: 0,
        };
        let mut program =
            compile_local(&spec, &services.schema, LocalCompileContext::default()).unwrap();
        let before_input = int_storage_bytes(1) * 2
            + std::mem::size_of::<EvalFrame<'_>>() * 2
            + std::mem::size_of::<RpnStackNode<'_>>() * 3;
        let result = program.eval_with_bindings(
            &mut LocalEvalState::with_limits(ExecutionLimits {
                max_retained_bytes: before_input,
                ..ExecutionLimits::default()
            }),
            &mut EvalContext::default(),
            1,
            &[0],
            &mut services,
        );
        assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
        assert_eq!(services.reads, 0);
    }

    #[test]
    fn test_logical_short_circuit_skips_rhs_rows() {
        use tipb::{Expr, ScalarFuncSig};

        fn fn_mapper(expr: &Expr) -> Result<crate::types::function::SelectedCall> {
            if expr.get_sig() == ScalarFuncSig::CastIntAsInt {
                return Ok(short_circuit_counted_identity_fn_meta().into());
            }
            crate::select_expr_node(expr)
        }

        fn int_column(values: &[Option<Int>]) -> LazyBatchColumn {
            let mut col =
                LazyBatchColumn::decoded_with_capacity_and_tp(values.len(), EvalType::Int);
            for value in values {
                col.mut_decoded().push_int(*value);
            }
            col
        }

        fn run_case(
            sig: ScalarFuncSig,
            lhs: &[Option<Int>],
            rhs: &[Option<Int>],
            logical_rows: &[usize],
            expected: &[Option<Int>],
            expected_rhs_eval_count: usize,
        ) {
            let node = ExprDefBuilder::scalar_func(sig, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
                .push_child(
                    ExprDefBuilder::scalar_func(ScalarFuncSig::CastIntAsInt, FieldTypeTp::LongLong)
                        .push_child(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong)),
                )
                .build();
            let mut ctx = short_circuit_context();
            let exp = RpnExpressionBuilder::build_from_expr_tree_with_fn_mapper_and_ctx(
                node, &mut ctx, fn_mapper, 2,
            )
            .unwrap();
            let mut columns = LazyBatchColumnVec::from(vec![int_column(lhs), int_column(rhs)]);
            let schema = &[FieldTypeTp::LongLong.into(), FieldTypeTp::LongLong.into()];

            SHORT_CIRCUIT_RHS_EVAL_COUNT.store(0, Ordering::SeqCst);
            let result = exp
                .eval(
                    &mut ctx,
                    schema,
                    &mut columns,
                    logical_rows,
                    logical_rows.len(),
                )
                .unwrap();

            assert_eq!(
                result.vector_value().unwrap().as_ref().to_int_vec(),
                expected
            );
            assert_eq!(
                SHORT_CIRCUIT_RHS_EVAL_COUNT.load(Ordering::SeqCst),
                expected_rhs_eval_count
            );
        }

        run_case(
            ScalarFuncSig::LogicalAnd,
            &[
                Some(0),
                Some(0),
                Some(0),
                Some(2),
                Some(2),
                Some(2),
                None,
                None,
                None,
            ],
            &[
                Some(0),
                Some(3),
                None,
                Some(0),
                Some(3),
                None,
                Some(0),
                Some(3),
                None,
            ],
            &[0, 1, 2, 3, 4, 5, 6, 7, 8],
            &[
                Some(0),
                Some(0),
                Some(0),
                Some(0),
                Some(1),
                None,
                Some(0),
                None,
                None,
            ],
            6,
        );
        run_case(
            ScalarFuncSig::LogicalOr,
            &[
                Some(0),
                Some(0),
                Some(0),
                Some(2),
                Some(2),
                Some(2),
                None,
                None,
                None,
            ],
            &[
                Some(0),
                Some(3),
                None,
                Some(0),
                Some(3),
                None,
                Some(0),
                Some(3),
                None,
            ],
            &[0, 1, 2, 3, 4, 5, 6, 7, 8],
            &[
                Some(0),
                Some(1),
                None,
                Some(1),
                Some(1),
                Some(1),
                None,
                Some(1),
                None,
            ],
            6,
        );
        run_case(
            ScalarFuncSig::LogicalOr,
            &[Some(1), Some(0), None, Some(2), Some(0)],
            &[Some(0), Some(0), Some(1), None, None],
            &[4, 0, 2, 3, 1],
            &[None, Some(1), Some(1), Some(1), Some(0)],
            3,
        );
    }

    #[test]
    fn test_constant_short_circuit_skips_all_rhs_rows() {
        use tipb::{Expr, ScalarFuncSig};

        fn fn_mapper(expr: &Expr) -> Result<crate::types::function::SelectedCall> {
            if expr.get_sig() == ScalarFuncSig::CastIntAsInt {
                return Ok(short_circuit_unreachable_fn_meta().into());
            }
            crate::select_expr_node(expr)
        }

        let node = ExprDefBuilder::scalar_func(ScalarFuncSig::LogicalOr, FieldTypeTp::LongLong)
            .push_child(ExprDefBuilder::constant_int(1))
            .push_child(
                ExprDefBuilder::scalar_func(ScalarFuncSig::CastIntAsInt, FieldTypeTp::LongLong)
                    .push_child(ExprDefBuilder::constant_int(0)),
            )
            .build();
        let mut ctx = short_circuit_context();
        let exp = RpnExpressionBuilder::build_from_expr_tree_with_fn_mapper_and_ctx(
            node, &mut ctx, fn_mapper, 0,
        )
        .unwrap();
        let mut columns = LazyBatchColumnVec::empty();

        let result = exp.eval(&mut ctx, &[], &mut columns, &[], 10).unwrap();

        assert_eq!(
            result.vector_value().unwrap().as_ref().to_int_vec(),
            vec![Some(1); 10]
        );
    }

    #[test]
    fn test_constant_short_circuit_without_logical_rows() {
        use tipb::ScalarFuncSig;

        fn constant(value: Option<Int>) -> ExprDefBuilder {
            match value {
                Some(value) => ExprDefBuilder::constant_int(value),
                None => ExprDefBuilder::constant_null(FieldTypeTp::LongLong),
            }
        }

        fn run_case(sig: ScalarFuncSig, lhs: Option<Int>, rhs: Option<Int>, expected: Option<Int>) {
            let node = ExprDefBuilder::scalar_func(sig, FieldTypeTp::LongLong)
                .push_child(constant(lhs))
                .push_child(constant(rhs))
                .build();
            let mut ctx = short_circuit_context();
            let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, 0).unwrap();
            let mut columns = LazyBatchColumnVec::empty();

            let result = exp.eval(&mut ctx, &[], &mut columns, &[], 3).unwrap();

            assert_eq!(
                result.vector_value().unwrap().as_ref().to_int_vec(),
                vec![expected; 3]
            );
        }

        run_case(ScalarFuncSig::LogicalOr, Some(0), Some(1), Some(1));
        run_case(ScalarFuncSig::LogicalAnd, Some(1), Some(0), Some(0));
        run_case(ScalarFuncSig::LogicalOr, None, Some(1), Some(1));
        run_case(ScalarFuncSig::LogicalAnd, None, Some(0), Some(0));
        run_case(ScalarFuncSig::LogicalOr, None, Some(0), None);
        run_case(ScalarFuncSig::LogicalAnd, None, Some(1), None);
    }

    #[test]
    fn test_short_circuit_suppresses_cast_warning() {
        fn run_case(flag: Flag) -> usize {
            let node = ExprDefBuilder::scalar_func(ScalarFuncSig::LogicalOr, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::constant_int(1))
                .push_child(
                    ExprDefBuilder::scalar_func(
                        ScalarFuncSig::CastStringAsInt,
                        FieldTypeTp::LongLong,
                    )
                    .push_child(ExprDefBuilder::constant_bytes(b"invalid-int".to_vec())),
                )
                .build();
            let mut ctx = EvalContext::new(Arc::new(EvalConfig::from_flag(flag)));
            let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, 0).unwrap();
            let mut columns = LazyBatchColumnVec::empty();

            let result = exp.eval(&mut ctx, &[], &mut columns, &[], 3).unwrap();
            assert_eq!(
                result.vector_value().unwrap().as_ref().to_int_vec(),
                vec![Some(1); 3]
            );
            ctx.warnings.warning_cnt
        }

        assert_eq!(
            run_case(Flag::ENABLE_SHORT_CIRCUIT_EXPRESSION | Flag::TRUNCATE_AS_WARNING),
            0
        );
        assert!(run_case(Flag::TRUNCATE_AS_WARNING) > 0);
    }

    #[test]
    fn test_normal_parent_consumes_short_circuit_result() {
        use tipb::ScalarFuncSig;

        let node = ExprDefBuilder::scalar_func(ScalarFuncSig::PlusInt, FieldTypeTp::LongLong)
            .push_child(
                ExprDefBuilder::scalar_func(ScalarFuncSig::LogicalOr, FieldTypeTp::LongLong)
                    .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
                    .push_child(
                        ExprDefBuilder::scalar_func(
                            ScalarFuncSig::CastIntAsInt,
                            FieldTypeTp::LongLong,
                        )
                        .push_child(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong)),
                    ),
            )
            .push_child(ExprDefBuilder::constant_int(10))
            .build();
        let mut ctx = short_circuit_context();
        let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, 2).unwrap();
        let mut columns = LazyBatchColumnVec::from(vec![
            VectorValue::Int(vec![Some(0), Some(1), None].into()),
            VectorValue::Int(vec![Some(0), None, Some(0)].into()),
        ]);
        let schema = &[FieldTypeTp::LongLong.into(), FieldTypeTp::LongLong.into()];
        let result = exp
            .eval(&mut ctx, schema, &mut columns, &[2, 0, 1], 3)
            .unwrap();

        assert_eq!(
            result.vector_value().unwrap().as_ref().to_int_vec(),
            &[None, Some(10), Some(11)]
        );
    }

    #[test]
    fn test_nested_mixed_short_circuit_calls() {
        use tipb::ScalarFuncSig;

        let node = ExprDefBuilder::scalar_func(ScalarFuncSig::LogicalAnd, FieldTypeTp::LongLong)
            .push_child(
                ExprDefBuilder::scalar_func(ScalarFuncSig::LogicalOr, FieldTypeTp::LongLong)
                    .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
                    .push_child(
                        ExprDefBuilder::scalar_func(
                            ScalarFuncSig::CastIntAsInt,
                            FieldTypeTp::LongLong,
                        )
                        .push_child(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong)),
                    ),
            )
            .push_child(ExprDefBuilder::column_ref(2, FieldTypeTp::LongLong))
            .build();
        let mut ctx = short_circuit_context();
        let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, 3).unwrap();
        let mut columns = LazyBatchColumnVec::from(vec![
            VectorValue::Int(vec![Some(1), Some(0), None, Some(0)].into()),
            VectorValue::Int(vec![Some(0), Some(0), Some(0), None].into()),
            VectorValue::Int(vec![Some(1), Some(1), Some(0), Some(1)].into()),
        ]);
        let schema = &[
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
        ];
        let result = exp
            .eval(&mut ctx, schema, &mut columns, &[3, 1, 2, 0], 4)
            .unwrap();

        assert_eq!(
            result.vector_value().unwrap().as_ref().to_int_vec(),
            &[None, Some(0), Some(0), Some(1)]
        );
    }

    #[test]
    fn test_flattened_short_circuit_call() {
        use tipb::ScalarFuncSig;

        let node = ExprDefBuilder::scalar_func(ScalarFuncSig::LogicalOr, FieldTypeTp::LongLong)
            .push_child(
                ExprDefBuilder::scalar_func(ScalarFuncSig::LogicalOr, FieldTypeTp::LongLong)
                    .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
                    .push_child(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong)),
            )
            .push_child(ExprDefBuilder::column_ref(2, FieldTypeTp::LongLong))
            .build();
        let mut ctx = short_circuit_context();
        let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, 3).unwrap();
        let mut columns = LazyBatchColumnVec::from(vec![
            VectorValue::Int(vec![Some(1), Some(0), None, Some(0)].into()),
            VectorValue::Int(vec![Some(0), Some(0), Some(0), None].into()),
            VectorValue::Int(vec![Some(0), Some(1), Some(1), Some(0)].into()),
        ]);
        let schema = &[
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
        ];
        let result = exp
            .eval(&mut ctx, schema, &mut columns, &[3, 1, 2, 0], 4)
            .unwrap();

        assert_eq!(
            result.vector_value().unwrap().as_ref().to_int_vec(),
            &[None, Some(1), Some(1), Some(1)]
        );
    }

    #[test]
    fn test_flattened_short_circuit_multiple_partial_compactions() {
        use tipb::ScalarFuncSig;

        let node = ExprDefBuilder::scalar_func(ScalarFuncSig::LogicalOr, FieldTypeTp::LongLong)
            .push_child(
                ExprDefBuilder::scalar_func(ScalarFuncSig::LogicalOr, FieldTypeTp::LongLong)
                    .push_child(
                        ExprDefBuilder::scalar_func(
                            ScalarFuncSig::LogicalOr,
                            FieldTypeTp::LongLong,
                        )
                        .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
                        .push_child(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong)),
                    )
                    .push_child(ExprDefBuilder::column_ref(2, FieldTypeTp::LongLong)),
            )
            .push_child(ExprDefBuilder::column_ref(3, FieldTypeTp::LongLong))
            .build();
        let mut ctx = short_circuit_context();
        let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, 4).unwrap();
        let mut columns = LazyBatchColumnVec::from(vec![
            VectorValue::Int(vec![Some(1), Some(0), None, Some(0), Some(0), Some(1)].into()),
            VectorValue::Int(vec![Some(0); 6].into()),
            VectorValue::Int(vec![Some(0), Some(1), Some(0), Some(0), Some(1), Some(0)].into()),
            VectorValue::Int(vec![Some(0); 6].into()),
        ]);
        let schema = &[
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
        ];
        let result = exp
            .eval(&mut ctx, schema, &mut columns, &[5, 2, 0, 4, 1, 3], 6)
            .unwrap();

        assert_eq!(
            result.vector_value().unwrap().as_ref().to_int_vec(),
            &[Some(1), None, Some(1), Some(1), Some(1), Some(0)]
        );
    }

    /// Creates fixture to be used in `test_eval_single_column_node_xxx`.
    fn new_single_column_node_fixture() -> (LazyBatchColumnVec, Vec<usize>, [FieldType; 2]) {
        let physical_columns = LazyBatchColumnVec::from(vec![
            {
                // this column is not referenced
                let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(5, EvalType::Real);
                col.mut_decoded().push_real(Real::new(1.0).ok());
                col.mut_decoded().push_real(None);
                col.mut_decoded().push_real(Real::new(7.5).ok());
                col.mut_decoded().push_real(None);
                col.mut_decoded().push_real(None);
                col
            },
            {
                let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(5, EvalType::Int);
                col.mut_decoded().push_int(Some(1));
                col.mut_decoded().push_int(Some(5));
                col.mut_decoded().push_int(None);
                col.mut_decoded().push_int(None);
                col.mut_decoded().push_int(Some(42));
                col
            },
        ]);
        let schema = [FieldTypeTp::Double.into(), FieldTypeTp::LongLong.into()];
        let logical_rows = (0..5).collect();
        (physical_columns, logical_rows, schema)
    }

    /// Single column node
    #[test]
    fn test_eval_single_column_node_normal() {
        let (columns, logical_rows, schema) = new_single_column_node_fixture();

        let mut c = columns.clone();
        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(1)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let result = exp.eval(&mut ctx, &schema, &mut c, &logical_rows, 5);
        let val = result.unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_int_vec(),
            [Some(1), Some(5), None, None, Some(42)]
        );
        assert_eq!(
            val.vector_value().unwrap().logical_rows(),
            logical_rows.as_slice()
        );
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::LongLong);

        let mut c = columns.clone();
        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(1)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let result = exp.eval(&mut ctx, &schema, &mut c, &[2, 0, 1], 3);
        let val = result.unwrap();
        assert!(val.is_vector());
        // Physical column is unchanged
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_int_vec(),
            [Some(1), Some(5), None, None, Some(42)]
        );
        assert_eq!(val.vector_value().unwrap().logical_rows(), &[2, 0, 1]);
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::LongLong);

        let mut c = columns;
        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(0)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let result = exp.eval(&mut ctx, &schema, &mut c, &logical_rows, 5);
        let val = result.unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_real_vec(),
            [Real::new(1.0).ok(), None, Real::new(7.5).ok(), None, None]
        );
        assert_eq!(
            val.vector_value().unwrap().logical_rows(),
            logical_rows.as_slice()
        );
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::Double);
    }

    /// Single column node but row numbers in `eval()` does not match column
    /// length, should panic.
    #[test]
    fn test_eval_single_column_node_mismatch_rows() {
        let (columns, logical_rows, schema) = new_single_column_node_fixture();

        let mut c = columns.clone();
        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(1)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let hooked_eval = panic_hook::recover_safe(|| {
            // smaller row number
            let _ = exp.eval(&mut ctx, &schema, &mut c, &logical_rows, 4);
        });
        hooked_eval.unwrap_err();

        let mut c = columns;
        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(1)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let hooked_eval = panic_hook::recover_safe(|| {
            // larger row number
            let _ = exp.eval(&mut ctx, &schema, &mut c, &logical_rows, 6);
        });
        hooked_eval.unwrap_err();
    }

    /// Single function call node (i.e. nullary function)
    #[test]
    fn test_eval_single_fn_call_node() {
        #[rpn_fn(nullable)]
        fn foo() -> Result<Option<i64>> {
            Ok(Some(42))
        }

        let exp = RpnExpressionBuilder::new_for_test()
            .push_fn_call_for_test(foo_fn_meta(), 0, FieldTypeTp::LongLong)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        let result = exp.eval(&mut ctx, &[], &mut columns, &[], 4);
        let val = result.unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_int_vec(),
            [Some(42), Some(42), Some(42), Some(42)]
        );
        assert_eq!(val.vector_value().unwrap().logical_rows(), &[0, 1, 2, 3]);
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::LongLong);
    }

    /// Unary function (argument is scalar)
    #[test]
    fn test_eval_unary_function_scalar() {
        /// foo(v) performs v * 2.
        #[rpn_fn(nullable)]
        fn foo(v: Option<&Real>) -> Result<Option<Real>> {
            Ok(v.map(|v| *v * 2.0))
        }

        let exp = RpnExpressionBuilder::new_for_test()
            .push_constant_for_test(1.5f64)
            .push_fn_call_for_test(foo_fn_meta(), 1, FieldTypeTp::Double)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        let result = exp.eval(&mut ctx, &[], &mut columns, &[], 3);
        let val = result.unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_real_vec(),
            [
                Real::new(3.0).ok(),
                Real::new(3.0).ok(),
                Real::new(3.0).ok()
            ]
        );
        assert_eq!(val.vector_value().unwrap().logical_rows(), &[0, 1, 2]);
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::Double);
    }

    /// Unary function (argument is vector)
    #[test]
    fn test_eval_unary_function_vector() {
        /// foo(v) performs v + 5.
        #[rpn_fn(nullable)]
        fn foo(v: Option<&i64>) -> Result<Option<i64>> {
            Ok(v.map(|v| v + 5))
        }

        let mut columns = LazyBatchColumnVec::from(vec![{
            let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(3, EvalType::Int);
            col.mut_decoded().push_int(Some(1));
            col.mut_decoded().push_int(Some(5));
            col.mut_decoded().push_int(None);
            col
        }]);
        let schema = &[FieldTypeTp::LongLong.into()];

        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(0)
            .push_fn_call_for_test(foo_fn_meta(), 1, FieldTypeTp::LongLong)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let result = exp.eval(&mut ctx, schema, &mut columns, &[2, 0], 2);
        let val = result.unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_int_vec(),
            [None, Some(6)]
        );
        assert_eq!(val.vector_value().unwrap().logical_rows(), &[0, 1]);
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::LongLong);
    }

    /// Unary function (argument is raw column). The column should be decoded.
    #[test]
    fn test_eval_unary_function_raw_column() {
        /// foo(v) performs v + 5.
        #[rpn_fn(nullable)]
        fn foo(v: Option<&i64>) -> Result<Option<i64>> {
            Ok(Some(v.unwrap() + 5))
        }

        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::from(vec![{
            let mut col = LazyBatchColumn::raw_with_capacity(3);

            let mut datum_raw = Vec::new();
            datum_raw
                .write_datum(&mut ctx, &[Datum::I64(-5)], false)
                .unwrap();
            col.mut_raw().push(&datum_raw);

            let mut datum_raw = Vec::new();
            datum_raw
                .write_datum(&mut ctx, &[Datum::I64(-7)], false)
                .unwrap();
            col.mut_raw().push(&datum_raw);

            let mut datum_raw = Vec::new();
            datum_raw
                .write_datum(&mut ctx, &[Datum::I64(3)], false)
                .unwrap();
            col.mut_raw().push(&datum_raw);

            col
        }]);
        let schema = &[FieldTypeTp::LongLong.into()];

        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(0)
            .push_fn_call_for_test(foo_fn_meta(), 1, FieldTypeTp::LongLong)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let result = exp.eval(&mut ctx, schema, &mut columns, &[2, 0, 1], 3);
        let val = result.unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_int_vec(),
            [Some(8), Some(0), Some(-2)]
        );
        assert_eq!(val.vector_value().unwrap().logical_rows(), &[0, 1, 2]);
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::LongLong);
    }

    /// Binary function (arguments are scalar, scalar)
    #[test]
    fn test_eval_binary_function_scalar_scalar() {
        /// foo(v) performs v1 + float(v2) - 1.
        #[rpn_fn(nullable)]
        fn foo(v1: Option<&Real>, v2: Option<&i64>) -> Result<Option<Real>> {
            Ok(Some(*v1.unwrap() + *v2.unwrap() as f64 - 1.0))
        }

        let exp = RpnExpressionBuilder::new_for_test()
            .push_constant_for_test(1.5f64)
            .push_constant_for_test(3i64)
            .push_fn_call_for_test(foo_fn_meta(), 2, FieldTypeTp::Double)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        let result = exp.eval(&mut ctx, &[], &mut columns, &[], 3);
        let val = result.unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_real_vec(),
            [
                Real::new(3.5).ok(),
                Real::new(3.5).ok(),
                Real::new(3.5).ok()
            ]
        );
        assert_eq!(val.vector_value().unwrap().logical_rows(), &[0, 1, 2]);
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::Double);
    }

    /// Binary function (arguments are vector, scalar)
    #[test]
    fn test_eval_binary_function_vector_scalar() {
        /// foo(v) performs v1 - v2.
        #[rpn_fn(nullable)]
        fn foo(v1: Option<&Real>, v2: Option<&Real>) -> Result<Option<Real>> {
            Ok(Some(*v1.unwrap() - *v2.unwrap()))
        }

        let mut columns = LazyBatchColumnVec::from(vec![{
            let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(3, EvalType::Real);
            col.mut_decoded().push_real(Real::new(1.0).ok());
            col.mut_decoded().push_real(Real::new(5.5).ok());
            col.mut_decoded().push_real(Real::new(-4.3).ok());
            col
        }]);
        let schema = &[FieldTypeTp::Double.into()];

        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(0)
            .push_constant_for_test(1.5f64)
            .push_fn_call_for_test(foo_fn_meta(), 2, FieldTypeTp::Double)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let result = exp.eval(&mut ctx, schema, &mut columns, &[2, 0], 2);
        let val = result.unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_real_vec(),
            [
                Real::new(-5.8).ok(), // original row 2
                Real::new(-0.5).ok(), // original row 0
            ]
        );
        assert_eq!(val.vector_value().unwrap().logical_rows(), &[0, 1]);
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::Double);
    }

    /// Binary function (arguments are scalar, vector)
    #[test]
    fn test_eval_binary_function_scalar_vector() {
        /// foo(v) performs v1 - float(v2).
        #[rpn_fn(nullable)]
        fn foo(v1: Option<&Real>, v2: Option<&i64>) -> Result<Option<Real>> {
            Ok(Some(*v1.unwrap() - *v2.unwrap() as f64))
        }

        let mut columns = LazyBatchColumnVec::from(vec![{
            let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(3, EvalType::Int);
            col.mut_decoded().push_int(Some(1));
            col.mut_decoded().push_int(Some(5));
            col.mut_decoded().push_int(Some(-4));
            col
        }]);
        let schema = &[FieldTypeTp::LongLong.into()];

        let exp = RpnExpressionBuilder::new_for_test()
            .push_constant_for_test(1.5f64)
            .push_column_ref_for_test(0)
            .push_fn_call_for_test(foo_fn_meta(), 2, FieldTypeTp::Double)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let result = exp.eval(&mut ctx, schema, &mut columns, &[1, 2], 2);
        let val = result.unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_real_vec(),
            [
                Real::new(-3.5).ok(), // original row 1
                Real::new(5.5).ok(),  // original row 2
            ]
        );
        assert_eq!(val.vector_value().unwrap().logical_rows(), &[0, 1]);
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::Double);
    }

    /// Binary function (arguments are vector, vector)
    #[test]
    fn test_eval_binary_function_vector_vector() {
        /// foo(v) performs int(v1*2.5 - float(v2)*3.5).
        #[rpn_fn(nullable)]
        fn foo(v1: Option<&Real>, v2: Option<&i64>) -> Result<Option<i64>> {
            Ok(Some(
                (v1.unwrap().into_inner() * 2.5 - (*v2.unwrap() as f64) * 3.5) as i64,
            ))
        }

        let mut columns = LazyBatchColumnVec::from(vec![
            {
                let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(3, EvalType::Int);
                col.mut_decoded().push_int(Some(1));
                col.mut_decoded().push_int(Some(5));
                col.mut_decoded().push_int(Some(-4));
                col
            },
            {
                let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(3, EvalType::Real);
                col.mut_decoded().push_real(Real::new(0.5).ok());
                col.mut_decoded().push_real(Real::new(-0.1).ok());
                col.mut_decoded().push_real(Real::new(3.5).ok());
                col
            },
        ]);
        let schema = &[FieldTypeTp::LongLong.into(), FieldTypeTp::Double.into()];

        // foo(col1, col0)
        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(1)
            .push_column_ref_for_test(0)
            .push_fn_call_for_test(foo_fn_meta(), 2, FieldTypeTp::LongLong)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let result = exp.eval(&mut ctx, schema, &mut columns, &[0, 2, 1], 3);
        let val = result.unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_int_vec(),
            [
                Some(-2),  // original row 0
                Some(22),  // original row 2
                Some(-17), // original row 1
            ]
        );
        assert_eq!(val.vector_value().unwrap().logical_rows(), &[0, 1, 2]);
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::LongLong);
    }

    /// Binary function (arguments are both raw columns). The same column is
    /// referred multiple times and it should be Ok.
    #[test]
    fn test_eval_binary_function_raw_column() {
        /// foo(v1, v2) performs v1 * v2.
        #[rpn_fn(nullable)]
        fn foo(v1: Option<&i64>, v2: Option<&i64>) -> Result<Option<i64>> {
            Ok(Some(v1.unwrap() * v2.unwrap()))
        }

        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::from(vec![{
            let mut col = LazyBatchColumn::raw_with_capacity(3);

            let mut datum_raw = Vec::new();
            datum_raw
                .write_datum(&mut ctx, &[Datum::I64(-5)], false)
                .unwrap();
            col.mut_raw().push(&datum_raw);

            let mut datum_raw = Vec::new();
            datum_raw
                .write_datum(&mut ctx, &[Datum::I64(-7)], false)
                .unwrap();
            col.mut_raw().push(&datum_raw);

            let mut datum_raw = Vec::new();
            datum_raw
                .write_datum(&mut ctx, &[Datum::I64(3)], false)
                .unwrap();
            col.mut_raw().push(&datum_raw);

            col
        }]);
        let schema = &[FieldTypeTp::LongLong.into(), FieldTypeTp::LongLong.into()];

        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(0)
            .push_column_ref_for_test(0)
            .push_fn_call_for_test(foo_fn_meta(), 2, FieldTypeTp::LongLong)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let result = exp.eval(&mut ctx, schema, &mut columns, &[1], 1);
        let val = result.unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_int_vec(),
            [Some(49)]
        );
        assert_eq!(val.vector_value().unwrap().logical_rows(), &[0]);
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::LongLong);
    }

    /// Ternary function (arguments are vector, scalar, vector)
    #[test]
    fn test_eval_ternary_function() {
        /// foo(v) performs v1 - v2 * v3.
        #[rpn_fn(nullable)]
        fn foo(v1: Option<&i64>, v2: Option<&i64>, v3: Option<&i64>) -> Result<Option<i64>> {
            Ok(Some(v1.unwrap() - v2.unwrap() * v3.unwrap()))
        }

        let mut columns = LazyBatchColumnVec::from(vec![{
            let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(3, EvalType::Int);
            col.mut_decoded().push_int(Some(1));
            col.mut_decoded().push_int(Some(5));
            col.mut_decoded().push_int(Some(-4));
            col
        }]);
        let schema = &[FieldTypeTp::LongLong.into()];

        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(0)
            .push_constant_for_test(3i64)
            .push_column_ref_for_test(0)
            .push_fn_call_for_test(foo_fn_meta(), 3, FieldTypeTp::LongLong)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let result = exp.eval(&mut ctx, schema, &mut columns, &[1, 0, 2], 3);
        let val = result.unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_int_vec(),
            [Some(-10), Some(-2), Some(8)]
        );
        assert_eq!(val.vector_value().unwrap().logical_rows(), &[0, 1, 2]);
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::LongLong);
    }

    // Comprehensive expression:
    //      fn_a(
    //          Col0,
    //          fn_b(),
    //          fn_c(
    //              fn_d(Col1, Const0),
    //              Const1
    //          )
    //      )
    //
    // RPN: Col0, fn_b, Col1, Const0, fn_d, Const1, fn_c, fn_a
    #[test]
    fn test_eval_comprehensive() {
        /// fn_a(v1, v2, v3) performs v1 * v2 - v3.
        #[rpn_fn(nullable)]
        fn fn_a(v1: Option<&Real>, v2: Option<&Real>, v3: Option<&Real>) -> Result<Option<Real>> {
            Ok(Some(*v1.unwrap() * *v2.unwrap() - *v3.unwrap()))
        }

        /// fn_b() returns 42.0.
        #[rpn_fn(nullable)]
        fn fn_b() -> Result<Option<Real>> {
            Ok(Real::new(42.0).ok())
        }

        /// fn_c(v1, v2) performs float(v2 - v1).
        #[rpn_fn(nullable)]
        fn fn_c(v1: Option<&i64>, v2: Option<&i64>) -> Result<Option<Real>> {
            Ok(Real::new((v2.unwrap() - v1.unwrap()) as f64).ok())
        }

        /// fn_d(v1, v2) performs v1 + v2 * 2.
        #[rpn_fn(nullable)]
        fn fn_d(v1: Option<&i64>, v2: Option<&i64>) -> Result<Option<i64>> {
            Ok(Some(v1.unwrap() + v2.unwrap() * 2))
        }

        let mut columns = LazyBatchColumnVec::from(vec![
            {
                let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(3, EvalType::Real);
                col.mut_decoded().push_real(Real::new(0.5).ok());
                col.mut_decoded().push_real(Real::new(-0.1).ok());
                col.mut_decoded().push_real(Real::new(3.5).ok());
                col
            },
            {
                let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(3, EvalType::Int);
                col.mut_decoded().push_int(Some(1));
                col.mut_decoded().push_int(Some(5));
                col.mut_decoded().push_int(Some(-4));
                col
            },
        ]);
        let schema = &[FieldTypeTp::Double.into(), FieldTypeTp::LongLong.into()];

        // Col0, fn_b, Col1, Const0, fn_d, Const1, fn_c, fn_a
        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(0)
            .push_fn_call_for_test(fn_b_fn_meta(), 0, FieldTypeTp::Double)
            .push_column_ref_for_test(1)
            .push_constant_for_test(7i64)
            .push_fn_call_for_test(fn_d_fn_meta(), 2, FieldTypeTp::LongLong)
            .push_constant_for_test(11i64)
            .push_fn_call_for_test(fn_c_fn_meta(), 2, FieldTypeTp::Double)
            .push_fn_call_for_test(fn_a_fn_meta(), 3, FieldTypeTp::Double)
            .build_for_test();

        //      fn_a(
        //          [0.5, -0.1, 3.5],
        //          42.0,
        //          fn_c(
        //              fn_d([1, 5, -4], 7),
        //              11
        //          )
        //      )
        //      => [25.0, 3.8, 146.0]

        let mut ctx = EvalContext::default();
        let result = exp.eval(&mut ctx, schema, &mut columns, &[2, 0], 2);
        let val = result.unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_real_vec(),
            [Real::new(146.0).ok(), Real::new(25.0).ok(),]
        );
        assert_eq!(val.vector_value().unwrap().logical_rows(), &[0, 1]);
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::Double);
    }

    /// Unary function, but supplied zero arguments. Should panic.
    #[test]
    fn test_eval_fail_1() {
        #[rpn_fn(nullable)]
        fn foo(_v: Option<&i64>) -> Result<Option<i64>> {
            unreachable!()
        }

        let exp = RpnExpressionBuilder::new_for_test()
            .push_fn_call_for_test(foo_fn_meta(), 1, FieldTypeTp::LongLong)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        let hooked_eval = panic_hook::recover_safe(|| {
            let _ = exp.eval(&mut ctx, &[], &mut columns, &[], 3);
        });
        hooked_eval.unwrap_err();
    }

    /// Irregular RPN expression (contains unused node). Should panic.
    #[test]
    fn test_eval_fail_2() {
        /// foo(v) performs v * 2.
        #[rpn_fn(nullable)]
        fn foo(v: Option<&Real>) -> Result<Option<Real>> {
            Ok(v.map(|v| *v * 2.0))
        }

        // foo() only accepts 1 parameter but we will give 2.

        let exp = RpnExpressionBuilder::new_for_test()
            .push_constant_for_test(3.0f64)
            .push_constant_for_test(1.5f64)
            .push_fn_call_for_test(foo_fn_meta(), 1, FieldTypeTp::Double)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        let hooked_eval = panic_hook::recover_safe(|| {
            let _ = exp.eval(&mut ctx, &[], &mut columns, &[], 3);
        });
        hooked_eval.unwrap_err();
    }

    /// Eval type does not match. Should panic.
    /// Note: When field type is not matching, it doesn't panic.
    #[test]
    fn test_eval_fail_3() {
        /// Expects real argument, receives int argument.
        #[rpn_fn(nullable)]
        fn foo(v: Option<&Real>) -> Result<Option<Real>> {
            Ok(v.map(|v| *v * 2.5))
        }

        let exp = RpnExpressionBuilder::new_for_test()
            .push_constant_for_test(7i64)
            .push_fn_call_for_test(foo_fn_meta(), 1, FieldTypeTp::Double)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        let hooked_eval = panic_hook::recover_safe(|| {
            let _ = exp.eval(&mut ctx, &[], &mut columns, &[], 3);
        });
        hooked_eval.unwrap_err();
    }

    /// Parse from an expression tree then evaluate.
    #[test]
    fn test_parse_and_eval() {
        use tipb::{Expr, ScalarFuncSig};

        // We will build an expression tree from:
        //      fn_d(
        //          fn_a(
        //              Const1,
        //              fn_b(Col1, fn_c()),
        //              Col0
        //          )
        //      )

        /// fn_a(a: int, b: float, c: int) performs: float(a) - b * float(c)
        #[rpn_fn(nullable)]
        fn fn_a(a: Option<&i64>, b: Option<&Real>, c: Option<&i64>) -> Result<Option<Real>> {
            Ok(Real::new(*a.unwrap() as f64 - b.unwrap().into_inner() * *c.unwrap() as f64).ok())
        }

        /// fn_b(a: float, b: int) performs: a * (float(b) - 1.5)
        #[rpn_fn(nullable)]
        fn fn_b(a: Option<&Real>, b: Option<&i64>) -> Result<Option<Real>> {
            Ok(Real::new(a.unwrap().into_inner() * (*b.unwrap() as f64 - 1.5)).ok())
        }

        /// fn_c() returns: int(42)
        #[rpn_fn(nullable)]
        fn fn_c() -> Result<Option<i64>> {
            Ok(Some(42))
        }

        /// fn_d(a: float) performs: int(a)
        #[rpn_fn(nullable)]
        fn fn_d(a: Option<&Real>) -> Result<Option<i64>> {
            Ok(Some(a.unwrap().into_inner() as i64))
        }

        fn fn_mapper(expr: &Expr) -> Result<RpnFnMeta> {
            // fn_a: CastIntAsInt
            // fn_b: CastIntAsReal
            // fn_c: CastIntAsString
            // fn_d: CastIntAsDecimal
            Ok(match expr.get_sig() {
                ScalarFuncSig::CastIntAsInt => fn_a_fn_meta(),
                ScalarFuncSig::CastIntAsReal => fn_b_fn_meta(),
                ScalarFuncSig::CastIntAsString => fn_c_fn_meta(),
                ScalarFuncSig::CastIntAsDecimal => fn_d_fn_meta(),
                _ => unreachable!(),
            })
        }

        let node =
            ExprDefBuilder::scalar_func(ScalarFuncSig::CastIntAsDecimal, FieldTypeTp::LongLong)
                .push_child(
                    ExprDefBuilder::scalar_func(ScalarFuncSig::CastIntAsInt, FieldTypeTp::Double)
                        .push_child(ExprDefBuilder::constant_int(7))
                        .push_child(
                            ExprDefBuilder::scalar_func(
                                ScalarFuncSig::CastIntAsReal,
                                FieldTypeTp::Double,
                            )
                            .push_child(ExprDefBuilder::column_ref(1, FieldTypeTp::Double))
                            .push_child(ExprDefBuilder::scalar_func(
                                ScalarFuncSig::CastIntAsString,
                                FieldTypeTp::LongLong,
                            )),
                        )
                        .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong)),
                )
                .build();

        // Build RPN expression from this expression tree.
        let exp =
            RpnExpressionBuilder::build_from_expr_tree_with_fn_mapper(node, fn_mapper, 2).unwrap();

        let mut columns = LazyBatchColumnVec::from(vec![
            {
                let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(3, EvalType::Int);
                col.mut_decoded().push_int(Some(1)); // row 1
                col.mut_decoded().push_int(Some(5));
                col.mut_decoded().push_int(Some(-4)); // row 0
                col
            },
            {
                let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(3, EvalType::Real);
                col.mut_decoded().push_real(Real::new(0.5).ok());
                col.mut_decoded().push_real(Real::new(-0.1).ok());
                col.mut_decoded().push_real(Real::new(3.5).ok());
                col
            },
        ]);
        let schema = &[FieldTypeTp::LongLong.into(), FieldTypeTp::Double.into()];

        let mut ctx = EvalContext::default();
        let result = exp.eval(&mut ctx, schema, &mut columns, &[2, 0], 2);
        let val = result.unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_int_vec(),
            [Some(574), Some(-13)]
        );
        assert_eq!(val.vector_value().unwrap().logical_rows(), &[0, 1]);
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::LongLong);
    }

    #[test]
    fn test_rpn_fn_data() {
        use tidb_query_datatype::codec::data_type::Evaluable;
        use tipb::{Expr, ScalarFuncSig};

        #[allow(clippy::trivially_copy_pass_by_ref)]
        #[allow(clippy::extra_unused_type_parameters)]
        #[rpn_fn(capture = [metadata], metadata_mapper = prepare_a::<T>)]
        fn fn_a_nonnull<T: Evaluable + EvaluableRet>(
            metadata: &i64,
            v: &Int,
        ) -> Result<Option<Int>> {
            assert_eq!(*metadata, 42);
            Ok(Some(v + *metadata))
        }

        #[allow(clippy::extra_unused_type_parameters)]
        fn prepare_a<T: Evaluable>(_expr: &mut crate::types::function::CallBuild) -> Result<i64> {
            Ok(42)
        }

        #[allow(clippy::trivially_copy_pass_by_ref, clippy::ptr_arg)]
        #[rpn_fn(nullable, varg, capture = [metadata], metadata_mapper = prepare_b::<T>)]
        fn fn_b<T: Evaluable + EvaluableRet>(
            metadata: &String,
            v: &[Option<&T>],
        ) -> Result<Option<T>> {
            assert_eq!(metadata, &format!("{}", std::mem::size_of::<T>()));
            Ok(v[0].cloned())
        }

        fn prepare_b<T: Evaluable>(
            _expr: &mut crate::types::function::CallBuild,
        ) -> Result<String> {
            Ok(format!("{}", std::mem::size_of::<T>()))
        }

        #[allow(clippy::trivially_copy_pass_by_ref)]
        #[rpn_fn(nullable, raw_varg, capture = [metadata], metadata_mapper = prepare_c::<T>)]
        fn fn_c<T: Evaluable>(
            _data: &std::marker::PhantomData<T>,
            args: &[ScalarValueRef<'_>],
        ) -> Result<Option<Int>> {
            Ok(Some(args.len() as i64))
        }

        fn prepare_c<T: Evaluable>(
            _expr: &mut crate::types::function::CallBuild,
        ) -> Result<std::marker::PhantomData<T>> {
            Ok(std::marker::PhantomData)
        }

        fn fn_mapper(expr: &Expr) -> Result<RpnFnMeta> {
            // fn_a: CastIntAsInt
            // fn_b: CastIntAsReal
            // fn_c: CastIntAsString
            Ok(match expr.get_sig() {
                ScalarFuncSig::CastIntAsInt => fn_a_nonnull_fn_meta::<Real>(),
                ScalarFuncSig::CastIntAsReal => fn_b_fn_meta::<Real>(),
                ScalarFuncSig::CastIntAsString => fn_c_fn_meta::<Int>(),
                _ => unreachable!(),
            })
        }

        let node =
            ExprDefBuilder::scalar_func(ScalarFuncSig::CastIntAsString, FieldTypeTp::LongLong)
                .push_child(
                    ExprDefBuilder::scalar_func(ScalarFuncSig::CastIntAsReal, FieldTypeTp::Double)
                        .push_child(ExprDefBuilder::constant_real(0.5)),
                )
                .push_child(
                    ExprDefBuilder::scalar_func(ScalarFuncSig::CastIntAsInt, FieldTypeTp::LongLong)
                        .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong)),
                )
                .build();

        // Build RPN expression from this expression tree.
        let exp =
            RpnExpressionBuilder::build_from_expr_tree_with_fn_mapper(node, fn_mapper, 1).unwrap();

        let mut columns = LazyBatchColumnVec::from(vec![{
            let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(2, EvalType::Int);
            col.mut_decoded().push_int(Some(1)); // row 1
            col.mut_decoded().push_int(None); // row 0
            col
        }]);
        let schema = &[FieldTypeTp::LongLong.into(), FieldTypeTp::Double.into()];

        let mut ctx = EvalContext::default();
        let result = exp.eval(&mut ctx, schema, &mut columns, &[1, 0], 2);
        let val = result.unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_int_vec(),
            [Some(2), Some(2)]
        );
        assert_eq!(val.vector_value().unwrap().logical_rows(), &[0, 1]);
        assert_eq!(val.field_type().as_accessor().tp(), FieldTypeTp::LongLong);
    }

    #[test]
    fn test_merge_nulls_constant_null() {
        /// Expects real argument, receives int argument.
        #[rpn_fn]
        fn foo(v: &Real) -> Result<Option<Real>> {
            Ok(Some(*v * 2.5))
        }

        let exp = RpnExpressionBuilder::new_for_test()
            .push_constant_for_test(ScalarValue::Real(None))
            .push_fn_call_for_test(foo_fn_meta(), 1, FieldTypeTp::Double)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        let val = exp.eval(&mut ctx, &[], &mut columns, &[], 10).unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_real_vec(),
            (0..10).map(|_| None).collect::<Vec<Option<Real>>>()
        );
    }

    #[test]
    fn test_merge_nulls_constant() {
        /// Expects real argument, receives int argument.
        #[rpn_fn]
        fn foo(v: &Real) -> Result<Option<Real>> {
            Ok(Some(*v * 2.5))
        }

        let exp = RpnExpressionBuilder::new_for_test()
            .push_constant_for_test(ScalarValue::Real(Real::new(10.0).ok()))
            .push_fn_call_for_test(foo_fn_meta(), 1, FieldTypeTp::Double)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let mut columns = LazyBatchColumnVec::empty();
        let val = exp.eval(&mut ctx, &[], &mut columns, &[], 10).unwrap();
        assert!(val.is_vector());
        assert_eq!(
            val.vector_value().unwrap().as_ref().to_real_vec(),
            (0..10)
                .map(|_| Real::new(25.0).ok())
                .collect::<Vec<Option<Real>>>()
        );
    }

    #[test]
    fn test_take_vector_value() {
        let scalar_node = RpnStackNode::Scalar {
            value: &ScalarValue::Real(Real::new(10.0).ok()),
            field_type: &FieldTypeTp::Double.into(),
        };
        scalar_node.take_vector_value().unwrap_err();

        let mut column = VectorValue::with_capacity(10, EvalType::Real);
        column.push_real(Real::new(10.0).ok());
        column.push_real(None);
        column.push_real(Real::new(20.0).ok());
        let vector_generate_node = RpnStackNode::Vector {
            value: (RpnStackNodeVectorValue::Generated {
                physical_value: (column),
            }),
            field_type: &FieldTypeTp::Double.into(),
        };
        let taked_value = vector_generate_node
            .take_vector_value()
            .unwrap()
            .to_real_vec();
        assert_eq!(taked_value[0].is_some_and(|x| x == 10.0), true);
        assert_eq!(taked_value[1].is_none(), true);
        assert_eq!(taked_value[2].is_some_and(|x| x == 20.0), true);

        let mut column2 = VectorValue::with_capacity(10, EvalType::Real);
        column2.push_real(Real::new(10.0).ok());
        column2.push_real(None);
        column2.push_real(Real::new(20.0).ok());
        column2.push_real(Real::new(40.0).ok());
        column2.push_real(None);
        let logical_rows = vec![0, 1, 3];
        let vector_generate_node = RpnStackNode::Vector {
            value: (RpnStackNodeVectorValue::Ref {
                physical_value: &column2,
                logical_rows: &logical_rows,
            }),
            field_type: &FieldTypeTp::Double.into(),
        };
        let taked_value = vector_generate_node
            .take_vector_value()
            .unwrap()
            .to_real_vec();
        assert_eq!(taked_value[0].is_some_and(|x| x == 10.0), true);
        assert_eq!(taked_value[1].is_none(), true);
        assert_eq!(taked_value[2].is_some_and(|x| x == 40.0), true);
    }

    #[bench]
    fn bench_eval_plus_1024_rows(b: &mut Bencher) {
        let mut columns = LazyBatchColumnVec::from(vec![{
            let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(1024, EvalType::Int);
            for i in 0..1024 {
                col.mut_decoded().push_int(Some(i));
            }
            col
        }]);
        let schema = &[FieldTypeTp::LongLong.into()];

        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(0)
            .push_column_ref_for_test(0)
            .push_fn_call_for_test(arithmetic_fn_meta::<IntIntPlus>(), 2, FieldTypeTp::LongLong)
            .build_for_test();
        let mut ctx = EvalContext::default();
        let logical_rows: Vec<_> = (0..1024).collect();

        profiler::start("./bench_eval_plus_1024_rows.profile");
        b.iter(|| {
            black_box(&exp)
                .eval(
                    black_box(&mut ctx),
                    black_box(schema),
                    black_box(&mut columns),
                    black_box(&logical_rows),
                    black_box(1024),
                )
                .unwrap();
        });
        profiler::stop();
    }

    #[bench]
    fn bench_eval_compare_1024_rows(b: &mut Bencher) {
        let mut columns = LazyBatchColumnVec::from(vec![{
            let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(1024, EvalType::Int);
            for i in 0..1024 {
                col.mut_decoded().push_int(Some(i));
            }
            col
        }]);
        let schema = &[FieldTypeTp::LongLong.into()];

        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(0)
            .push_column_ref_for_test(0)
            .push_fn_call_for_test(
                compare_fn_meta::<BasicComparer<Int, CmpOpLe>>(),
                2,
                FieldTypeTp::LongLong,
            )
            .build_for_test();
        let mut ctx = EvalContext::default();
        let logical_rows: Vec<_> = (0..1024).collect();

        profiler::start("./eval_compare_1024_rows.profile");
        b.iter(|| {
            black_box(&exp)
                .eval(
                    black_box(&mut ctx),
                    black_box(schema),
                    black_box(&mut columns),
                    black_box(&logical_rows),
                    black_box(1024),
                )
                .unwrap();
        });
        profiler::stop();
    }

    #[bench]
    fn bench_eval_compare_5_rows(b: &mut Bencher) {
        let mut columns = LazyBatchColumnVec::from(vec![{
            let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(5, EvalType::Int);
            for i in 0..5 {
                col.mut_decoded().push_int(Some(i));
            }
            col
        }]);
        let schema = &[FieldTypeTp::LongLong.into()];

        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(0)
            .push_column_ref_for_test(0)
            .push_fn_call_for_test(
                compare_fn_meta::<BasicComparer<Int, CmpOpLe>>(),
                2,
                FieldTypeTp::LongLong,
            )
            .build_for_test();
        let mut ctx = EvalContext::default();
        let logical_rows: Vec<_> = (0..5).collect();

        profiler::start("./bench_eval_compare_5_rows.profile");
        b.iter(|| {
            black_box(&exp)
                .eval(
                    black_box(&mut ctx),
                    black_box(schema),
                    black_box(&mut columns),
                    black_box(&logical_rows),
                    black_box(5),
                )
                .unwrap();
        });
        profiler::stop();
    }
}

#[cfg(test)]
mod benches {
    use tidb_query_codegen::rpn_fn;
    use tidb_query_common::Result;
    use tidb_query_datatype::{
        EvalType, FieldTypeTp,
        codec::{batch::LazyBatchColumn, data_type::*},
        expr::EvalContext,
    };

    use super::*;
    use crate::RpnExpressionBuilder;

    #[bench]
    fn bench_int_eval(b: &mut test::Bencher) {
        /// Expects real argument, receives 3 real arguments.
        #[rpn_fn]
        fn foo(u: &Real, v: &Real, w: &Real) -> Result<Option<Real>> {
            Ok(Some(*u * 2.5 + *v * 2.5 + *w * 2.5))
        }

        let mut columns = LazyBatchColumnVec::from(vec![
            {
                let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(1024, EvalType::Real);
                for _i in 0..256 {
                    col.mut_decoded().push_real(Real::new(233.0).ok());
                    col.mut_decoded().push_real(Real::new(233.0).ok());
                    col.mut_decoded().push_real(Real::new(233.0).ok());
                    col.mut_decoded().push_real(Real::new(233.0).ok());
                }
                col
            },
            {
                let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(1024, EvalType::Real);
                for _i in 0..256 {
                    col.mut_decoded().push_real(Real::new(233.0).ok());
                    col.mut_decoded().push_real(Real::new(233.0).ok());
                    col.mut_decoded().push_real(Real::new(233.0).ok());
                    col.mut_decoded().push_real(None);
                }
                col
            },
            {
                let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(1024, EvalType::Real);
                for _i in 0..256 {
                    col.mut_decoded().push_real(Real::new(233.0).ok());
                    col.mut_decoded().push_real(None);
                    col.mut_decoded().push_real(Real::new(233.0).ok());
                    col.mut_decoded().push_real(None);
                }
                col
            },
        ]);

        let input_logical_rows: Vec<usize> = (0..1024).collect();

        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(0)
            .push_column_ref_for_test(1)
            .push_column_ref_for_test(2)
            .push_fn_call_for_test(foo_fn_meta(), 3, FieldTypeTp::Double)
            .build_for_test();

        let schema = &[
            FieldTypeTp::Double.into(),
            FieldTypeTp::Double.into(),
            FieldTypeTp::Double.into(),
        ];

        b.iter(|| {
            let mut ctx = EvalContext::default();
            exp.eval(
                &mut ctx,
                schema,
                &mut columns,
                input_logical_rows.as_slice(),
                input_logical_rows.len(),
            )
            .unwrap();
        });
    }

    #[bench]
    fn bench_bytes_eval(b: &mut test::Bencher) {
        /// Expects real argument, receives 3 real arguments.
        #[rpn_fn(writer)]
        fn foo(u: BytesRef, v: BytesRef, w: BytesRef, writer: BytesWriter) -> Result<BytesGuard> {
            let mut partial = writer.begin();
            partial.partial_write(u);
            partial.partial_write(v);
            partial.partial_write(w);
            Ok(partial.finish())
        }

        let mut bytes_vec: Vec<u8> = vec![];
        for _i in 0..10 {
            bytes_vec.append(&mut b"2333333333".to_vec());
        }

        let mut columns = LazyBatchColumnVec::from(vec![
            {
                let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(1024, EvalType::Bytes);
                for _i in 0..256 {
                    col.mut_decoded().push_bytes(Some(bytes_vec.clone()));
                    col.mut_decoded().push_bytes(Some(bytes_vec.clone()));
                    col.mut_decoded().push_bytes(Some(bytes_vec.clone()));
                    col.mut_decoded().push_bytes(Some(bytes_vec.clone()));
                }
                col
            },
            {
                let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(1024, EvalType::Bytes);
                for _i in 0..256 {
                    col.mut_decoded().push_bytes(Some(bytes_vec.clone()));
                    col.mut_decoded().push_bytes(None);
                    col.mut_decoded().push_bytes(Some(bytes_vec.clone()));
                    col.mut_decoded().push_bytes(Some(bytes_vec.clone()));
                }
                col
            },
            {
                let mut col = LazyBatchColumn::decoded_with_capacity_and_tp(1024, EvalType::Bytes);
                for _i in 0..256 {
                    col.mut_decoded().push_bytes(Some(bytes_vec.clone()));
                    col.mut_decoded().push_bytes(None);
                    col.mut_decoded().push_bytes(Some(bytes_vec.clone()));
                    col.mut_decoded().push_bytes(None);
                }
                col
            },
        ]);

        let input_logical_rows: Vec<usize> = (0..1024).collect();

        let exp = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(0)
            .push_column_ref_for_test(1)
            .push_column_ref_for_test(2)
            .push_fn_call_for_test(foo_fn_meta(), 3, FieldTypeTp::String)
            .build_for_test();

        let schema = &[
            FieldTypeTp::String.into(),
            FieldTypeTp::String.into(),
            FieldTypeTp::String.into(),
        ];

        b.iter(|| {
            let mut ctx = EvalContext::default();
            exp.eval(
                &mut ctx,
                schema,
                &mut columns,
                input_logical_rows.as_slice(),
                input_logical_rows.len(),
            )
            .unwrap();
        });
    }
}
