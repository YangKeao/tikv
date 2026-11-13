// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::{any::Any, sync::OnceLock};

use tidb_query_datatype::codec::data_type::ScalarValue;
use tipb::FieldType;

use super::super::function::RpnFnMeta;
use crate::ShortCircuitFnMeta;

/// Native expression values outside the original protobuf scalar domain.
/// These are transported intact; individual kernels own range checks.
#[derive(Clone, Debug)]
pub enum ExpressionScalar {
    Real(Option<f64>),
    Decimal(Option<ExpressionDecimal>),
}

/// Native intermediate columns retain full floating and decimal domains.
#[derive(Debug)]
pub enum ExpressionVector {
    Real(Vec<Option<f64>>),
    Decimal(Vec<Option<ExpressionDecimal>>),
}

/// Exact expression-only Decimal data, independent of fixed-size storage.
#[derive(Clone, Debug)]
pub struct ExpressionDecimal {
    pub negative: bool,
    pub coefficient: Vec<u8>,
    pub storage_scale: u32,
    pub result_scale: u32,
    pub declared_shape: Option<(i64, i64)>,
}

/// Invocation contract is independent of vector width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvocationMode {
    Scalar,
    Vector,
}

/// Projection traversal is separate from a function's invocation mode.
#[derive(Debug, Clone, Copy)]
pub enum OutputOrder {
    Columns,
    Rows,
}

/// A type for each node in the RPN expression list.
#[derive(Debug)]
pub enum RpnExpressionNode {
    /// BENCHMARK evaluates its body anew, after count and type demand checks.
    Repeat {
        end: usize,
        supported_type: bool,
        field_type: FieldType,
    },
    RepeatEnd {
        field_type: FieldType,
    },
    /// Names are build-time tokens; NULL assignment leaves the session
    /// unchanged.
    UserVariable {
        name: String,
        write: bool,
        field_type: FieldType,
    },
    /// Native session RNG and per-occurrence constant-seed contract.
    Rand {
        seeded: bool,
        constant_seed: bool,
        field_type: FieldType,
    },
    /// Native row fallback: condition and demanded child share one row scope.
    OrderedRows {
        end: usize,
        field_type: FieldType,
    },
    /// Scalar numeric demand skips the right child after a NULL left operand.
    NullGuard {
        target: usize,
        field_type: FieldType,
    },
    /// Native IF control edges. Targets are instruction offsets, not subtrees.
    JumpIfFalse {
        target: usize,
        end: usize,
        field_type: FieldType,
    },
    Jump {
        target: usize,
        field_type: FieldType,
    },
    /// A native scalar whose full domain is wider than protobuf storage.
    ExpressionConstant {
        value: ExpressionScalar,
        field_type: FieldType,
    },
    /// Represents a function call that decides which arguments and rows need to
    /// be evaluated. Each argument remains an independent RPN expression.
    ShortCircuitFnCall {
        func_meta: ShortCircuitFnMeta,
        args: Box<[RpnExpression]>,
        field_type: FieldType,
    },

    /// Represents a function call.
    FnCall {
        func_meta: RpnFnMeta,
        args_len: usize,
        field_type: FieldType,
        metadata: Box<dyn Any + Send>,
    },

    /// Represents a scalar constant value.
    Constant {
        value: ScalarValue,
        field_type: FieldType,
    },

    /// Represents a reference to a column in the columns specified in
    /// evaluation.
    ColumnRef {
        offset: usize,
    },
}

impl RpnExpressionNode {
    /// Gets the field type.
    #[cfg(test)]
    pub fn field_type(&self) -> &FieldType {
        match self {
            RpnExpressionNode::ShortCircuitFnCall { field_type, .. } => field_type,
            RpnExpressionNode::FnCall { field_type, .. }
            | RpnExpressionNode::JumpIfFalse { field_type, .. }
            | RpnExpressionNode::Jump { field_type, .. }
            | RpnExpressionNode::NullGuard { field_type, .. }
            | RpnExpressionNode::OrderedRows { field_type, .. }
            | RpnExpressionNode::Rand { field_type, .. }
            | RpnExpressionNode::UserVariable { field_type, .. }
            | RpnExpressionNode::Repeat { field_type, .. }
            | RpnExpressionNode::RepeatEnd { field_type } => field_type,
            RpnExpressionNode::Constant { field_type, .. }
            | RpnExpressionNode::ExpressionConstant { field_type, .. } => field_type,
            RpnExpressionNode::ColumnRef { .. } => panic!(),
        }
    }

    #[cfg(test)]
    pub fn expr_tp(&self) -> tipb::ExprType {
        use tidb_query_datatype::{EvalType, FieldTypeAccessor};
        use tipb::ExprType;

        match self {
            RpnExpressionNode::ShortCircuitFnCall { .. } => ExprType::ScalarFunc,
            RpnExpressionNode::FnCall { .. }
            | RpnExpressionNode::JumpIfFalse { .. }
            | RpnExpressionNode::Jump { .. }
            | RpnExpressionNode::NullGuard { .. }
            | RpnExpressionNode::OrderedRows { .. }
            | RpnExpressionNode::Rand { .. }
            | RpnExpressionNode::UserVariable { .. }
            | RpnExpressionNode::Repeat { .. }
            | RpnExpressionNode::RepeatEnd { .. } => ExprType::ScalarFunc,
            RpnExpressionNode::Constant { value, .. } => match value.eval_type() {
                EvalType::Bytes => ExprType::Bytes,
                EvalType::DateTime => ExprType::MysqlTime,
                EvalType::Decimal => ExprType::MysqlDecimal,
                EvalType::Duration => ExprType::MysqlDuration,
                EvalType::Int => ExprType::Int64,
                EvalType::Json => ExprType::MysqlJson,
                EvalType::Real => ExprType::Float64,
                EvalType::Enum => ExprType::MysqlEnum,
                EvalType::Set => ExprType::MysqlSet,
                EvalType::VectorFloat32 => ExprType::TiDbVectorFloat32,
            },
            RpnExpressionNode::ColumnRef { .. } => ExprType::ColumnRef,
            RpnExpressionNode::ExpressionConstant { value, .. } => match value {
                ExpressionScalar::Real(_) => ExprType::Float64,
                ExpressionScalar::Decimal(_) => ExprType::MysqlDecimal,
            },
        }
    }

    /// Borrows the function instance for `FnCall` variant.
    #[cfg(test)]
    pub fn fn_call_func(&self) -> RpnFnMeta {
        match self {
            RpnExpressionNode::FnCall { func_meta, .. } => *func_meta,
            _ => panic!(),
        }
    }

    /// Borrows the constant value for `Constant` variant.
    #[cfg(test)]
    pub fn constant_value(&self) -> &ScalarValue {
        match self {
            RpnExpressionNode::Constant { value, .. } => value,
            _ => panic!(),
        }
    }

    fn collect_metadata(&self, metadata: &mut RpnExpressionMetadata) {
        metadata.node_count += 1;
        match self {
            RpnExpressionNode::ShortCircuitFnCall { args, .. } => {
                // A flattened logical call represents a chain of binary
                // operations. Keep those operations in the work estimate even
                // though flattening stores them in one physical node.
                metadata.work_count += args.len().saturating_sub(1);
                for arg in args {
                    arg.collect_metadata(metadata);
                }
            }
            RpnExpressionNode::ColumnRef { offset } => {
                metadata.work_count += 1;
                metadata.column_ref_count += 1;
                metadata.referenced_column_offsets.push(*offset);
            }
            _ => {
                metadata.work_count += 1;
            }
        }
    }
}

#[derive(Debug, Default)]
struct RpnExpressionMetadata {
    node_count: usize,
    work_count: usize,
    column_ref_count: usize,
    referenced_column_offsets: Vec<usize>,
}

/// An expression in Reverse Polish notation, which is simply a list of RPN
/// expression nodes.
///
/// You may want to build it using `RpnExpressionBuilder`.
#[derive(Debug)]
pub struct RpnExpression {
    nodes: Vec<RpnExpressionNode>,
    return_type: Option<FieldType>,
    metadata: OnceLock<Box<RpnExpressionMetadata>>,
}

impl std::ops::Deref for RpnExpression {
    type Target = Vec<RpnExpressionNode>;

    fn deref(&self) -> &Vec<RpnExpressionNode> {
        &self.nodes
    }
}

impl std::ops::DerefMut for RpnExpression {
    fn deref_mut(&mut self) -> &mut Vec<RpnExpressionNode> {
        // Any mutable access may change the expression tree, so cached metadata
        // must be rebuilt the next time it is requested.
        assert!(
            self.return_type.is_none(),
            "finished control programs are immutable"
        );
        self.metadata = OnceLock::new();
        &mut self.nodes
    }
}

impl From<Vec<RpnExpressionNode>> for RpnExpression {
    fn from(v: Vec<RpnExpressionNode>) -> Self {
        Self {
            nodes: v,
            return_type: None,
            metadata: OnceLock::new(),
        }
    }
}

impl AsRef<[RpnExpressionNode]> for RpnExpression {
    fn as_ref(&self) -> &[RpnExpressionNode] {
        self.nodes.as_ref()
    }
}

impl AsMut<[RpnExpressionNode]> for RpnExpression {
    fn as_mut(&mut self) -> &mut [RpnExpressionNode] {
        assert!(
            self.return_type.is_none(),
            "finished control programs are immutable"
        );
        self.metadata = OnceLock::new();
        self.nodes.as_mut()
    }
}

impl RpnExpression {
    fn metadata(&self) -> &RpnExpressionMetadata {
        self.metadata
            .get_or_init(|| {
                let mut metadata = RpnExpressionMetadata::default();
                self.collect_metadata(&mut metadata);
                metadata.referenced_column_offsets.sort_unstable();
                metadata.referenced_column_offsets.dedup();
                Box::new(metadata)
            })
            .as_ref()
    }

    fn collect_metadata(&self, metadata: &mut RpnExpressionMetadata) {
        for node in &self.nodes {
            node.collect_metadata(metadata);
        }
    }

    /// Validate control edges, stack shapes and exit type, then freeze the
    /// program. Legacy protobuf builders retain their existing mutable
    /// straight-line API.
    pub fn finish(
        nodes: Vec<RpnExpressionNode>,
        schema: &[FieldType],
        return_type: FieldType,
    ) -> tidb_query_common::Result<Self> {
        use tidb_query_datatype::{EvalType, FieldTypeAccessor};
        let invalid = || other_err!("invalid shared control program");
        if nodes.is_empty() {
            return Err(invalid());
        }
        let mut states: Vec<Option<Vec<EvalType>>> = vec![None; nodes.len() + 1];
        states[0] = Some(Vec::new());
        let mut jumps = std::collections::HashMap::new();
        let mut repeat_ends = std::collections::HashMap::new();
        let mut regions = Vec::new();
        for (pc, node) in nodes.iter().enumerate() {
            while regions.last().is_some_and(|&(_, end)| pc >= end) {
                regions.pop();
            }
            if let RpnExpressionNode::Repeat {
                end,
                supported_type,
                ..
            } = node
            {
                if *end <= pc + 1
                    || *end > nodes.len()
                    || !matches!(nodes[end - 1], RpnExpressionNode::RepeatEnd { .. })
                    || repeat_ends.insert(end - 1, *supported_type).is_some()
                {
                    return Err(invalid());
                }
                for (at, candidate) in nodes.iter().enumerate() {
                    let targets: &[usize] = match candidate {
                        RpnExpressionNode::JumpIfFalse { target, end, .. } => &[*target, *end],
                        RpnExpressionNode::Jump { target, .. }
                        | RpnExpressionNode::NullGuard { target, .. } => &[*target],
                        RpnExpressionNode::Repeat { end, .. }
                        | RpnExpressionNode::OrderedRows { end, .. } => &[*end],
                        _ => &[],
                    };
                    for target in targets {
                        if (at > pc && at < *end && *target > end - 1)
                            || ((at < pc || at >= *end) && *target > pc && *target < *end)
                        {
                            return Err(invalid());
                        }
                    }
                }
            }
            if let RpnExpressionNode::NullGuard { target, .. } = node {
                if *target <= pc || *target > nodes.len() {
                    return Err(invalid());
                }
                if let Some(&(split, end)) = regions.last() {
                    if *target > if pc < split { split - 1 } else { end } {
                        return Err(invalid());
                    }
                }
            }
            if let RpnExpressionNode::JumpIfFalse { target, end, .. } = node {
                if *target <= pc + 1
                    || *target >= *end
                    || *end > nodes.len()
                    || !matches!(&nodes[target - 1], RpnExpressionNode::Jump { target: to, .. } if to == end)
                    || jumps.insert(target - 1, *end).is_some()
                {
                    return Err(invalid());
                }
                while regions.last().is_some_and(|&(_, end)| pc >= end) {
                    regions.pop();
                }
                if let Some(&(split, outer_end)) = regions.last() {
                    if *end > if pc < split { split - 1 } else { outer_end } {
                        return Err(invalid());
                    }
                }
                regions.push((*target, *end));
            }
        }
        for (pc, node) in nodes.iter().enumerate() {
            let Some(mut stack) = states[pc].clone() else {
                return Err(invalid());
            };
            let mut successors = vec![pc + 1];
            match node {
                RpnExpressionNode::Repeat {
                    end, field_type, ..
                } => {
                    if field_type.as_accessor().tp() != tidb_query_datatype::FieldTypeTp::LongLong {
                        return Err(invalid());
                    }
                    if stack.pop() != Some(EvalType::Int) {
                        return Err(invalid());
                    }
                    let mut skipped = stack.clone();
                    skipped.push(EvalType::Int);
                    if let Some(previous) = &states[*end] {
                        if previous != &skipped {
                            return Err(invalid());
                        }
                    } else {
                        states[*end] = Some(skipped);
                    }
                }
                RpnExpressionNode::RepeatEnd { field_type } => {
                    if field_type.as_accessor().tp() != tidb_query_datatype::FieldTypeTp::LongLong {
                        return Err(invalid());
                    }
                    let supported = repeat_ends.get(&pc).ok_or_else(invalid)?;
                    if *supported && stack.pop().is_none() {
                        return Err(invalid());
                    }
                    stack.push(EvalType::Int);
                }
                RpnExpressionNode::UserVariable {
                    write, field_type, ..
                } => {
                    if *write && stack.pop() != Some(EvalType::Bytes) {
                        return Err(invalid());
                    }
                    if EvalType::try_from(field_type.as_accessor().tp())
                        .map_err(|e| other_err!("{}", e))?
                        != EvalType::Bytes
                    {
                        return Err(invalid());
                    }
                    stack.push(EvalType::Bytes);
                }
                RpnExpressionNode::Rand {
                    seeded, field_type, ..
                } => {
                    if *seeded && stack.pop() != Some(EvalType::Int) {
                        return Err(invalid());
                    }
                    if EvalType::try_from(field_type.as_accessor().tp())
                        .map_err(|e| other_err!("{}", e))?
                        != EvalType::Real
                    {
                        return Err(invalid());
                    }
                    stack.push(EvalType::Real);
                }
                RpnExpressionNode::OrderedRows { end, .. } => {
                    if *end <= pc + 1 || *end > nodes.len() {
                        return Err(invalid());
                    }
                    for (at, candidate) in nodes.iter().enumerate() {
                        let targets: &[usize] = match candidate {
                            RpnExpressionNode::JumpIfFalse { target, end, .. } => &[*target, *end],
                            RpnExpressionNode::Jump { target, .. }
                            | RpnExpressionNode::NullGuard { target, .. } => &[*target],
                            RpnExpressionNode::OrderedRows { end, .. }
                            | RpnExpressionNode::Repeat { end, .. } => &[*end],
                            _ => &[],
                        };
                        for target in targets {
                            if (at > pc && at < *end && *target > *end)
                                || ((at < pc || at >= *end) && *target > pc && *target < *end)
                            {
                                return Err(invalid());
                            }
                        }
                    }
                }
                RpnExpressionNode::NullGuard { target, field_type } => {
                    if stack.last() != Some(&EvalType::Int)
                        || EvalType::try_from(field_type.as_accessor().tp())
                            .map_err(|e| other_err!("{}", e))?
                            != EvalType::Int
                    {
                        return Err(invalid());
                    }
                    successors.push(*target);
                }
                RpnExpressionNode::JumpIfFalse { target, .. } => {
                    if stack.pop() != Some(EvalType::Int) {
                        return Err(invalid());
                    }
                    successors.push(*target);
                }
                RpnExpressionNode::Jump { target, .. } => {
                    if jumps.get(&pc) != Some(target) {
                        return Err(invalid());
                    }
                    successors[0] = *target;
                }
                RpnExpressionNode::ColumnRef { offset } => {
                    stack.push(
                        EvalType::try_from(
                            schema.get(*offset).ok_or_else(invalid)?.as_accessor().tp(),
                        )
                        .map_err(|e| other_err!("{}", e))?,
                    );
                }
                RpnExpressionNode::FnCall {
                    args_len,
                    field_type,
                    ..
                } => {
                    if stack.len() < *args_len {
                        return Err(invalid());
                    }
                    stack.truncate(stack.len() - args_len);
                    stack.push(
                        EvalType::try_from(field_type.as_accessor().tp())
                            .map_err(|e| other_err!("{}", e))?,
                    );
                }
                RpnExpressionNode::Constant { field_type, value } => {
                    let declared = EvalType::try_from(field_type.as_accessor().tp())
                        .map_err(|e| other_err!("{}", e))?;
                    if declared != value.eval_type() {
                        return Err(invalid());
                    }
                    stack.push(declared);
                }
                RpnExpressionNode::ExpressionConstant { field_type, value } => {
                    let declared = EvalType::try_from(field_type.as_accessor().tp())
                        .map_err(|e| other_err!("{}", e))?;
                    let actual = match value {
                        ExpressionScalar::Real(_) => EvalType::Real,
                        ExpressionScalar::Decimal(_) => EvalType::Decimal,
                    };
                    if declared != actual {
                        return Err(invalid());
                    }
                    stack.push(declared);
                }
                RpnExpressionNode::ShortCircuitFnCall { field_type, .. } => {
                    stack.push(
                        EvalType::try_from(field_type.as_accessor().tp())
                            .map_err(|e| other_err!("{}", e))?,
                    );
                }
            }
            for next in successors {
                if next > nodes.len() {
                    return Err(invalid());
                }
                if let Some(previous) = &states[next] {
                    if previous != &stack {
                        return Err(invalid());
                    }
                } else {
                    states[next] = Some(stack.clone());
                }
            }
        }
        for (pc, node) in nodes.iter().enumerate() {
            if let RpnExpressionNode::OrderedRows { end, field_type } = node {
                let before = states[pc].as_ref().ok_or_else(invalid)?;
                let after = states[*end].as_ref().ok_or_else(invalid)?;
                if after.len() != before.len() + 1
                    || after[..before.len()] != before[..]
                    || after.last()
                        != Some(
                            &EvalType::try_from(field_type.as_accessor().tp())
                                .map_err(|e| other_err!("{}", e))?,
                        )
                {
                    return Err(invalid());
                }
            }
            if let RpnExpressionNode::JumpIfFalse {
                end, field_type, ..
            } = node
            {
                let before = states[pc].as_ref().ok_or_else(invalid)?;
                let after = states[*end].as_ref().ok_or_else(invalid)?;
                if after.len() != before.len()
                    || after.last()
                        != Some(
                            &EvalType::try_from(field_type.as_accessor().tp())
                                .map_err(|e| other_err!("{}", e))?,
                        )
                    || after[..after.len() - 1] != before[..before.len() - 1]
                {
                    return Err(invalid());
                }
            }
        }
        if states[nodes.len()].as_deref()
            != Some(&[EvalType::try_from(return_type.as_accessor().tp())
                .map_err(|e| other_err!("{}", e))?])
        {
            return Err(invalid());
        }
        Ok(Self {
            nodes,
            return_type: Some(return_type),
            metadata: OnceLock::new(),
        })
    }

    /// Gets the field type of the return value.
    pub fn ret_field_type<'a>(&'a self, schema: &'a [FieldType]) -> &'a FieldType {
        assert!(!self.nodes.is_empty());
        if let Some(field_type) = &self.return_type {
            return field_type;
        }
        let last_node = self.nodes.last().unwrap();
        match last_node {
            RpnExpressionNode::FnCall { field_type, .. }
            | RpnExpressionNode::JumpIfFalse { field_type, .. }
            | RpnExpressionNode::Jump { field_type, .. }
            | RpnExpressionNode::NullGuard { field_type, .. }
            | RpnExpressionNode::OrderedRows { field_type, .. }
            | RpnExpressionNode::Rand { field_type, .. }
            | RpnExpressionNode::UserVariable { field_type, .. }
            | RpnExpressionNode::Repeat { field_type, .. }
            | RpnExpressionNode::RepeatEnd { field_type } => field_type,
            RpnExpressionNode::Constant { field_type, .. }
            | RpnExpressionNode::ExpressionConstant { field_type, .. } => field_type,
            RpnExpressionNode::ColumnRef { offset } => &schema[*offset],
            RpnExpressionNode::ShortCircuitFnCall { field_type, .. } => field_type,
        }
    }

    /// Unwraps into the underlying expression node vector.
    pub fn into_inner(self) -> Vec<RpnExpressionNode> {
        self.nodes
    }

    /// Returns true if the last element of expression is a `Constant` variant.
    pub fn is_last_constant(&self) -> bool {
        assert!(!self.nodes.is_empty());
        self.return_type.is_none()
            && matches!(
                self.nodes.last().unwrap(),
                RpnExpressionNode::Constant { .. }
            )
    }

    /// Returns the number of nodes, including nodes nested in short-circuit
    /// arguments.
    pub fn node_count(&self) -> usize {
        self.metadata().node_count
    }

    /// Returns the approximate executor work units for this expression,
    /// including nodes nested in short-circuit arguments.
    ///
    /// A flattened short-circuit call is counted as one unit per logical
    /// operation (`args.len() - 1`) so flattening does not hide the work of the
    /// logical operations it replaces.
    pub fn work_count(&self) -> usize {
        self.metadata().work_count
    }

    /// Returns the number of column references, including references nested in
    /// short-circuit arguments.
    pub fn column_ref_count(&self) -> usize {
        self.metadata().column_ref_count
    }

    /// Returns sorted, deduplicated offsets of all referenced columns,
    /// including references nested in short-circuit arguments.
    pub(crate) fn referenced_column_offsets(&self) -> &[usize] {
        &self.metadata().referenced_column_offsets
    }
}

// For `RpnExpression::eval`, see `expr_eval` file.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cached_metadata_is_deduplicated_and_invalidated_by_mutation() {
        let mut expr = RpnExpression::from(vec![
            RpnExpressionNode::ColumnRef { offset: 2 },
            RpnExpressionNode::ColumnRef { offset: 0 },
            RpnExpressionNode::ColumnRef { offset: 2 },
        ]);

        assert_eq!(expr.node_count(), 3);
        assert_eq!(expr.work_count(), 3);
        assert_eq!(expr.column_ref_count(), 3);
        assert_eq!(expr.referenced_column_offsets(), &[0, 2]);

        expr.push(RpnExpressionNode::ColumnRef { offset: 1 });

        assert_eq!(expr.node_count(), 4);
        assert_eq!(expr.work_count(), 4);
        assert_eq!(expr.column_ref_count(), 4);
        assert_eq!(expr.referenced_column_offsets(), &[0, 1, 2]);
    }
}
