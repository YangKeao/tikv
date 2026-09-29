// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::{any::Any, mem, sync::OnceLock};

use tidb_query_datatype::codec::data_type::ScalarValue;
use tipb::{FieldType, ScalarFuncSig};

use super::super::function::RpnFnMeta;
use crate::{
    ShortCircuitFnMeta,
    local::{CheckedResultFlow, LocalError, LocalResult, PreparedHostCall, PreparedOrdinaryCall},
};

/// A type for each node in the RPN expression list.
#[derive(Debug)]
pub enum RpnExpressionNode {
    /// Represents a function call that decides which arguments and rows need to
    /// be evaluated. Each argument remains an independent RPN expression.
    ShortCircuitFnCall {
        func_meta: ShortCircuitFnMeta,
        args: Box<[RpnExpression]>,
        field_type: FieldType,
    },

    /// Represents a checked ordinary call whose consumer profile controls
    /// argument demand. Preparation retains its exact function and call site;
    /// each child is evaluated by the same RPN frame driver.
    OrdinaryFnCall {
        prepared: PreparedOrdinaryCall,
        args: Box<[RpnExpression]>,
    },

    /// Represents a registered host call with independently demandable
    /// children. The prepared catalog signature owns the result type, not a
    /// wire function ID.
    HostCall {
        prepared: PreparedHostCall,
        args: Box<[RpnExpression]>,
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
    ColumnRef { offset: usize },
}

impl RpnExpressionNode {
    /// Gets the field type.
    #[cfg(test)]
    pub fn field_type(&self) -> &FieldType {
        match self {
            RpnExpressionNode::ShortCircuitFnCall { field_type, .. } => field_type,
            RpnExpressionNode::HostCall { prepared, .. } => prepared.return_type(),
            RpnExpressionNode::OrdinaryFnCall { prepared, .. } => prepared.return_type(),
            RpnExpressionNode::FnCall { field_type, .. } => field_type,
            RpnExpressionNode::Constant { field_type, .. } => field_type,
            RpnExpressionNode::ColumnRef { .. } => panic!(),
        }
    }

    #[cfg(test)]
    pub fn expr_tp(&self) -> tipb::ExprType {
        use tidb_query_datatype::EvalType;
        use tipb::ExprType;

        match self {
            RpnExpressionNode::ShortCircuitFnCall { .. }
            | RpnExpressionNode::HostCall { .. }
            | RpnExpressionNode::OrdinaryFnCall { .. }
            | RpnExpressionNode::FnCall { .. } => ExprType::ScalarFunc,
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
///
/// Metadata collection and destruction do not recurse through nested arguments.
/// The derived `Debug` implementation is not covered by that depth guarantee.
#[derive(Debug)]
pub struct RpnExpression {
    nodes: Vec<RpnExpressionNode>,
    metadata: OnceLock<Box<RpnExpressionMetadata>>,
    // Detached, Copy result identity on this executable subprogram, not another
    // graph or an owner of source tables. Only checked singleton compilation
    // attaches it; legacy/wire construction and structural mutation leave None.
    result_flow: Option<CheckedResultFlow>,
}

impl Drop for RpnExpression {
    fn drop(&mut self) {
        let mut pending = mem::take(&mut self.nodes);
        while let Some(mut node) = pending.pop() {
            if let RpnExpressionNode::ShortCircuitFnCall { args, .. }
            | RpnExpressionNode::HostCall { args, .. }
            | RpnExpressionNode::OrdinaryFnCall { args, .. } = &mut node
            {
                for arg in args.iter_mut() {
                    // Empty each child before its owning node is dropped. Its
                    // own Drop then has no descendants to visit recursively.
                    pending.append(&mut arg.nodes);
                }
            }
        }
    }
}

impl std::ops::Deref for RpnExpression {
    type Target = Vec<RpnExpressionNode>;

    fn deref(&self) -> &Vec<RpnExpressionNode> {
        &self.nodes
    }
}

impl std::ops::DerefMut for RpnExpression {
    fn deref_mut(&mut self) -> &mut Vec<RpnExpressionNode> {
        // Any mutable access may change the expression tree, invalidating both
        // the metadata cache and the checked result-flow attachment.
        self.invalidate_structure();
        &mut self.nodes
    }
}

impl From<Vec<RpnExpressionNode>> for RpnExpression {
    fn from(v: Vec<RpnExpressionNode>) -> Self {
        Self {
            nodes: v,
            metadata: OnceLock::new(),
            result_flow: None,
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
        self.invalidate_structure();
        self.nodes.as_mut()
    }
}

impl RpnExpression {
    fn invalidate_structure(&mut self) {
        self.metadata = OnceLock::new();
        self.result_flow = None;
    }

    pub(crate) fn checked_result_flow(&self) -> Option<CheckedResultFlow> {
        self.result_flow
    }

    /// Attaches flow derived from immutable checked source facts. The shared
    /// compiler validates the node/flow pairing; raw or multi-node construction
    /// cannot acquire a result-flow attachment through a public API.
    pub(crate) fn with_result_flow(mut self, flow: CheckedResultFlow) -> LocalResult<Self> {
        if self.nodes.len() != 1 {
            return Err(LocalError::InvalidSpec(
                "result-flow annotation requires one RPN node".into(),
            ));
        }
        self.result_flow = Some(flow);
        Ok(self)
    }

    /// Observes only this expression's already-initialized metadata heap.
    /// This never warms the cache. The node Vec, child programs, FieldTypes and
    /// opaque function payloads are outside this narrow allocation observation.
    pub(crate) fn retained_metadata_heap_bytes(&self) -> Option<usize> {
        let Some(metadata) = self.metadata.get() else {
            return Some(0);
        };
        mem::size_of::<RpnExpressionMetadata>().checked_add(
            metadata
                .referenced_column_offsets
                .capacity()
                .checked_mul(mem::size_of::<usize>())?,
        )
    }

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
        let mut pending = vec![self];
        while let Some(expr) = pending.pop() {
            for node in &expr.nodes {
                metadata.node_count += 1;
                match node {
                    RpnExpressionNode::ShortCircuitFnCall {
                        func_meta, args, ..
                    } => {
                        // Only flattened AND/OR chains represent multiple
                        // binary operations in one physical control node.
                        metadata.work_count += if matches!(
                            func_meta.sig,
                            ScalarFuncSig::LogicalAnd | ScalarFuncSig::LogicalOr
                        ) {
                            args.len().saturating_sub(1)
                        } else {
                            1
                        };
                        pending.extend(args.iter().rev());
                    }
                    RpnExpressionNode::HostCall { args, .. }
                    | RpnExpressionNode::OrdinaryFnCall { args, .. } => {
                        metadata.work_count += 1;
                        pending.extend(args.iter().rev());
                    }
                    RpnExpressionNode::ColumnRef { offset } => {
                        metadata.work_count += 1;
                        metadata.column_ref_count += 1;
                        metadata.referenced_column_offsets.push(*offset);
                    }
                    _ => metadata.work_count += 1,
                }
            }
        }
    }

    /// Gets the field type of the return value.
    pub fn ret_field_type<'a>(&'a self, schema: &'a [FieldType]) -> &'a FieldType {
        assert!(!self.nodes.is_empty());
        let last_node = self.nodes.last().unwrap();
        match last_node {
            RpnExpressionNode::FnCall { field_type, .. } => field_type,
            RpnExpressionNode::Constant { field_type, .. } => field_type,
            RpnExpressionNode::ColumnRef { offset } => &schema[*offset],
            RpnExpressionNode::ShortCircuitFnCall { field_type, .. } => field_type,
            RpnExpressionNode::HostCall { prepared, .. } => prepared.return_type(),
            RpnExpressionNode::OrdinaryFnCall { prepared, .. } => prepared.return_type(),
        }
    }

    /// Unwraps into the underlying expression node vector, discarding this
    /// root's result-flow attachment. Intact child subprograms keep their own
    /// attachments until structurally mutated; wrapping these nodes again does
    /// not restore the root's attachment.
    pub fn into_inner(mut self) -> Vec<RpnExpressionNode> {
        self.invalidate_structure();
        mem::take(&mut self.nodes)
    }

    /// Returns true if the last element of expression is a `Constant` variant.
    pub fn is_last_constant(&self) -> bool {
        assert!(!self.nodes.is_empty());
        matches!(
            self.nodes.last().unwrap(),
            RpnExpressionNode::Constant { .. }
        )
    }

    /// Returns the number of nodes, including nodes nested in arguments.
    pub fn node_count(&self) -> usize {
        self.metadata().node_count
    }

    /// Returns the approximate executor work units for this expression,
    /// including nodes nested in arguments.
    ///
    /// A flattened logical AND/OR call is counted as one unit per logical
    /// operation (`args.len() - 1`) so flattening does not hide the work of the
    /// logical operations it replaces. Other controls, host calls and profiled
    /// ordinary calls contribute one unit in addition to their children's work.
    pub fn work_count(&self) -> usize {
        self.metadata().work_count
    }

    /// Returns the number of column references, including references nested in
    /// arguments.
    pub fn column_ref_count(&self) -> usize {
        self.metadata().column_ref_count
    }

    /// Returns sorted, deduplicated offsets of all referenced columns,
    /// including references nested in arguments.
    pub(crate) fn referenced_column_offsets(&self) -> &[usize] {
        &self.metadata().referenced_column_offsets
    }
}

// For `RpnExpression::eval`, see `expr_eval` file.

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use tidb_query_datatype::{
        FieldTypeTp,
        expr::{EvalConfig, EvalContext, Flag},
    };
    use tikv_util::sys::thread::StdThreadBuildWrapper;
    use tipb_helper::ExprDefBuilder;

    use super::*;
    use crate::{
        RpnExpressionBuilder,
        local::{
            CallMetadata, CompileLimits, FunctionRef, HostCatalog, HostSignature, LineageCarrier,
            LocalCompileContext, LocalExpr, OrdinaryCallSite, OrdinaryProfile, OrdinaryProfileSpec,
            OrdinarySourceId, ResultMetaId, compile_local_profiled,
        },
    };

    fn logical_meta(sig: ScalarFuncSig) -> ShortCircuitFnMeta {
        // Obtain the official descriptor without depending on its execution
        // fields. The nontrivial RHS makes legacy wire admission worthwhile.
        let tree = ExprDefBuilder::scalar_func(sig, FieldTypeTp::LongLong)
            .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
            .push_child(
                ExprDefBuilder::scalar_func(ScalarFuncSig::AbsInt, FieldTypeTp::LongLong)
                    .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong)),
            )
            .build();
        let mut ctx = EvalContext::new(Arc::new(EvalConfig::from_flag(
            Flag::ENABLE_SHORT_CIRCUIT_EXPRESSION,
        )));
        let expression = RpnExpressionBuilder::build_from_expr_tree(tree, &mut ctx, 1).unwrap();
        match expression.as_ref() {
            [RpnExpressionNode::ShortCircuitFnCall { func_meta, .. }] => *func_meta,
            _ => panic!("expected the official short-circuit descriptor"),
        }
    }

    fn column(offset: usize) -> RpnExpression {
        RpnExpression::from(vec![RpnExpressionNode::ColumnRef { offset }])
    }

    fn leaf_flow(record: u64) -> CheckedResultFlow {
        CheckedResultFlow::Leaf {
            id: ResultMetaId::new(0xfedc_ba98_7654_3210, record),
            carrier: LineageCarrier::Int,
        }
    }

    fn own_flow(record: u64) -> CheckedResultFlow {
        CheckedResultFlow::OwnResult {
            id: ResultMetaId::new(0xfedc_ba98_7654_3210, record),
        }
    }

    fn tagged_column(offset: usize, record: u64) -> RpnExpression {
        column(offset).with_result_flow(leaf_flow(record)).unwrap()
    }

    #[test]
    fn test_result_flow_defaults_none_for_raw_and_wire_construction() {
        for expression in [
            RpnExpression::from(vec![]),
            column(0),
            RpnExpression::from(vec![
                RpnExpressionNode::ColumnRef { offset: 0 },
                RpnExpressionNode::ColumnRef { offset: 1 },
            ]),
        ] {
            assert_eq!(expression.checked_result_flow(), None);
        }
        let wire = ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong).build();
        let expression =
            RpnExpressionBuilder::build_from_expr_tree(wire, &mut EvalContext::default(), 1)
                .unwrap();
        assert_eq!(expression.checked_result_flow(), None);
        assert_eq!(expression.node_count(), 1);
    }

    #[test]
    fn test_result_flow_requires_one_root_node() {
        let flow = leaf_flow(u64::MAX);
        let expression = column(0).with_result_flow(flow).unwrap();
        assert_eq!(expression.checked_result_flow(), Some(flow));
        // The returned annotation is detached Copy metadata, not a borrow.
        let detached = expression.checked_result_flow();
        drop(expression);
        assert_eq!(detached, Some(flow));
        for nodes in [
            vec![],
            vec![
                RpnExpressionNode::ColumnRef { offset: 0 },
                RpnExpressionNode::ColumnRef { offset: 1 },
            ],
        ] {
            assert!(matches!(
                RpnExpression::from(nodes).with_result_flow(flow),
                Err(LocalError::InvalidSpec(_))
            ));
        }
    }

    #[test]
    fn test_result_flow_attachment_preserves_counts_and_child_annotations() {
        let expression = RpnExpression::from(vec![RpnExpressionNode::ShortCircuitFnCall {
            func_meta: logical_meta(ScalarFuncSig::LogicalAnd),
            args: vec![tagged_column(2, 1), tagged_column(0, 2)].into_boxed_slice(),
            field_type: FieldTypeTp::LongLong.into(),
        }]);
        assert_eq!(expression.checked_result_flow(), None);
        assert_eq!(expression.node_count(), 3);
        assert_eq!(expression.work_count(), 3);
        assert_eq!(expression.column_ref_count(), 2);
        assert_eq!(expression.referenced_column_offsets(), &[0, 2]);
        let cached = expression.metadata() as *const RpnExpressionMetadata;
        // One structured root is eligible even with multiple descendant nodes.
        let mut expression = expression.with_result_flow(own_flow(0)).unwrap();
        assert_eq!(expression.checked_result_flow(), Some(own_flow(0)));
        assert_eq!(
            expression.metadata() as *const RpnExpressionMetadata,
            cached
        );
        assert_eq!(expression.node_count(), 3);
        assert_eq!(expression.work_count(), 3);
        assert_eq!(expression.column_ref_count(), 2);
        assert_eq!(expression.referenced_column_offsets(), &[0, 2]);
        let [RpnExpressionNode::ShortCircuitFnCall { args, .. }] = expression.as_ref() else {
            panic!("expected a structured control");
        };
        assert_eq!(args[0].checked_result_flow(), Some(leaf_flow(1)));
        assert_eq!(args[1].checked_result_flow(), Some(leaf_flow(2)));
        assert_eq!(expression.checked_result_flow(), Some(own_flow(0)));

        // Public access to a nested mutable child invalidates the ancestor on
        // the way in and that child's own attachment when it is changed.
        let [RpnExpressionNode::ShortCircuitFnCall { args, .. }] = expression.as_mut() else {
            panic!("expected a structured control");
        };
        args[0][0] = RpnExpressionNode::ColumnRef { offset: 3 };
        assert_eq!(args[0].checked_result_flow(), None);
        assert_eq!(args[1].checked_result_flow(), Some(leaf_flow(2)));
        assert_eq!(expression.checked_result_flow(), None);
        assert!(expression.metadata.get().is_none());
        assert_eq!(expression.node_count(), 3);
        assert_eq!(expression.referenced_column_offsets(), &[0, 3]);
    }

    #[test]
    fn test_result_flow_mutable_seams_invalidate_attachment_and_cache() {
        for seam in 0..6 {
            let mut expression = tagged_column(2, 1);
            assert_eq!(expression.node_count(), 1);
            assert_eq!(expression.referenced_column_offsets(), &[2]);
            match seam {
                0 => {
                    let _ = std::ops::DerefMut::deref_mut(&mut expression);
                }
                1 => {
                    let _: &mut [RpnExpressionNode] = expression.as_mut();
                }
                2 => {
                    expression.as_mut_slice()[0] = RpnExpressionNode::ColumnRef { offset: 1 };
                }
                3 => expression[0] = RpnExpressionNode::ColumnRef { offset: 1 },
                4 => {
                    *expression.get_mut(0).unwrap() = RpnExpressionNode::ColumnRef { offset: 1 };
                }
                5 => expression.push(RpnExpressionNode::ColumnRef { offset: 1 }),
                _ => unreachable!(),
            }
            assert_eq!(expression.checked_result_flow(), None);
            assert!(expression.metadata.get().is_none());
            let count = if seam == 5 { 2 } else { 1 };
            assert_eq!(expression.node_count(), count);
            assert_eq!(expression.work_count(), count);
            assert_eq!(expression.column_ref_count(), count);
            let offsets: &[usize] = match seam {
                0 | 1 => &[2],
                5 => &[1, 2],
                _ => &[1],
            };
            assert_eq!(expression.referenced_column_offsets(), offsets);
        }
    }

    #[test]
    fn test_result_flow_into_inner_discards_root_not_intact_children() {
        let expression = RpnExpression::from(vec![RpnExpressionNode::ShortCircuitFnCall {
            func_meta: logical_meta(ScalarFuncSig::LogicalOr),
            args: vec![tagged_column(0, 1), tagged_column(1, 2)].into_boxed_slice(),
            field_type: FieldTypeTp::LongLong.into(),
        }])
        .with_result_flow(own_flow(0))
        .unwrap();
        assert_eq!(expression.node_count(), 3);
        let expression = RpnExpression::from(expression.into_inner());
        assert_eq!(expression.checked_result_flow(), None);
        assert!(expression.metadata.get().is_none());
        let [RpnExpressionNode::ShortCircuitFnCall { args, .. }] = expression.as_ref() else {
            panic!("expected a structured control");
        };
        assert_eq!(args[0].checked_result_flow(), Some(leaf_flow(1)));
        assert_eq!(args[1].checked_result_flow(), Some(leaf_flow(2)));
        assert_eq!(expression.node_count(), 3);
        assert_eq!(expression.work_count(), 3);
        assert_eq!(expression.column_ref_count(), 2);
    }

    #[test]
    fn test_deep_result_flow_metadata_and_drop_on_small_stack() {
        let logical = [
            logical_meta(ScalarFuncSig::LogicalAnd),
            logical_meta(ScalarFuncSig::LogicalOr),
        ];
        thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn_wrapper(move || {
                for depth in [33, 64, 256, 16_384] {
                    for detach_root in [false, true] {
                        let mut expression = tagged_column(0, 0);
                        for level in 0..depth {
                            let record = u64::try_from(level).unwrap() * 2 + 1;
                            expression =
                                RpnExpression::from(vec![RpnExpressionNode::ShortCircuitFnCall {
                                    func_meta: logical[level % logical.len()],
                                    args: vec![expression, tagged_column((level + 1) % 4, record)]
                                        .into_boxed_slice(),
                                    field_type: FieldTypeTp::LongLong.into(),
                                }])
                                .with_result_flow(own_flow(record + 1))
                                .unwrap();
                        }
                        assert!(expression.checked_result_flow().is_some());
                        if detach_root {
                            // Uncached teardown after losing the root tag still
                            // drops the intact tagged descendants iteratively.
                            drop(expression.into_inner());
                        } else {
                            assert_eq!(expression.node_count(), 2 * depth + 1);
                            assert_eq!(expression.work_count(), 2 * depth + 1);
                            assert_eq!(expression.column_ref_count(), depth + 1);
                            assert_eq!(expression.referenced_column_offsets(), &[0, 1, 2, 3]);
                            drop(expression);
                        }
                    }
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    fn nested_controls(
        mut expression: RpnExpression,
        depth: usize,
        logical: [ShortCircuitFnMeta; 2],
    ) -> RpnExpression {
        for level in 0..depth {
            expression = RpnExpression::from(vec![RpnExpressionNode::ShortCircuitFnCall {
                func_meta: logical[level % logical.len()],
                args: vec![expression, column((level + 1) % 4)].into_boxed_slice(),
                field_type: FieldTypeTp::LongLong.into(),
            }]);
        }
        expression
    }

    fn host_catalog(arity: usize) -> HostCatalog {
        let field_type: FieldType = FieldTypeTp::LongLong.into();
        HostCatalog::new(vec![HostSignature {
            arg_types: vec![field_type.clone(); arity].into_boxed_slice(),
            return_type: field_type,
        }])
        .unwrap()
    }

    fn host_call(catalog: &HostCatalog, args: Vec<RpnExpression>) -> RpnExpression {
        let field_type: FieldType = FieldTypeTp::LongLong.into();
        let prepared = PreparedHostCall::prepare(
            catalog,
            catalog.slot(0).unwrap(),
            &vec![field_type.clone(); args.len()],
            &field_type,
        )
        .unwrap();
        RpnExpression::from(vec![RpnExpressionNode::HostCall {
            prepared,
            args: args.into_boxed_slice(),
        }])
    }

    fn nested_host_controls(
        mut expression: RpnExpression,
        depth: usize,
        catalog: &HostCatalog,
        logical: [ShortCircuitFnMeta; 2],
    ) -> RpnExpression {
        for level in 0..depth {
            if level % 2 == 0 {
                expression = host_call(catalog, vec![expression]);
            } else {
                expression = RpnExpression::from(vec![RpnExpressionNode::ShortCircuitFnCall {
                    func_meta: logical[(level / 2) % logical.len()],
                    args: vec![expression, column((level / 2 + 1) % 4)].into_boxed_slice(),
                    field_type: FieldTypeTp::LongLong.into(),
                }]);
            }
        }
        expression
    }

    fn ordinary_input(slot: usize, field_type: &FieldType) -> LocalExpr {
        LocalExpr::InputSlot {
            slot,
            field_type: field_type.clone(),
        }
    }

    fn ordinary_spec(lhs: LocalExpr, rhs: LocalExpr, field_type: &FieldType) -> LocalExpr {
        LocalExpr::Call {
            function: FunctionRef::TiPb(ScalarFuncSig::PlusInt),
            args: vec![lhs, rhs].into_boxed_slice(),
            return_type: field_type.clone(),
            metadata: CallMetadata::None,
        }
    }

    fn ordinary_site(profile: OrdinaryProfile, ordinal: usize) -> OrdinaryCallSite {
        // Source identity is deliberately distinct from the preorder ordinal,
        // and retains bits that would be lost through a narrower wire integer.
        let source = OrdinarySourceId::new(
            0x1234_5678_abcd_ef01,
            u64::MAX - u64::try_from(ordinal).unwrap(),
        );
        match profile {
            OrdinaryProfile::TypedRow => OrdinaryCallSite::typed_row(ordinal, source),
            OrdinaryProfile::PbRow => OrdinaryCallSite::pb_row(ordinal, source, 203),
            _ => panic!("ordinary fixture requires an admitted row profile"),
        }
    }

    fn compile_ordinary(
        spec: &LocalExpr,
        schema: &[FieldType],
        profile: OrdinaryProfile,
        limits: CompileLimits,
    ) -> RpnExpression {
        // The actual local compiler, rather than an unchecked prepared-call
        // constructor, supplies every ordinary descriptor used by these tests.
        let mut pending = vec![spec];
        let mut sites = Vec::new();
        let mut ordinal = 0;
        while let Some(expr) = pending.pop() {
            match expr {
                LocalExpr::Call { function, args, .. } => {
                    assert_eq!(*function, FunctionRef::TiPb(ScalarFuncSig::PlusInt));
                    sites.push(ordinary_site(profile, ordinal));
                    pending.extend(args.iter().rev());
                }
                LocalExpr::Constant { .. } | LocalExpr::InputSlot { .. } => {}
                LocalExpr::HostCall { .. } => panic!("ordinary fixture excludes host calls"),
            }
            ordinal += 1;
        }
        let facts = OrdinaryProfileSpec::new(spec, schema, profile, sites, limits).unwrap();
        compile_local_profiled(spec, schema, LocalCompileContext { limits }, &facts)
            .unwrap()
            .into_expression_for_test()
    }

    #[test]
    fn test_ordinary_metadata_retains_exact_prepared_type_and_site() {
        fn assert_send<T: Send>() {}
        assert_send::<PreparedOrdinaryCall>();
        assert_send::<RpnExpression>();

        let mut field_type: FieldType = FieldTypeTp::LongLong.into();
        field_type.set_flen(19);
        field_type.set_decimal(0);
        let schema = vec![field_type.clone(); 3];
        let spec = ordinary_spec(
            ordinary_input(2, &field_type),
            ordinary_spec(
                ordinary_input(0, &field_type),
                ordinary_input(2, &field_type),
                &field_type,
            ),
            &field_type,
        );
        for profile in [OrdinaryProfile::TypedRow, OrdinaryProfile::PbRow] {
            let expression = compile_ordinary(&spec, &schema, profile, CompileLimits::default());
            assert_eq!(expression.node_count(), 5);
            assert_eq!(expression.work_count(), 5);
            assert_eq!(expression.column_ref_count(), 3);
            assert_eq!(expression.referenced_column_offsets(), &[0, 2]);
            assert_eq!(expression.ret_field_type(&[]), &field_type);
            assert_eq!(expression[0].field_type(), &field_type);
            assert_eq!(expression[0].expr_tp(), tipb::ExprType::ScalarFunc);
            assert!(!expression.is_last_constant());
            let [RpnExpressionNode::OrdinaryFnCall { prepared, args }] = expression.as_ref() else {
                panic!("expected a prepared ordinary root");
            };
            assert_eq!(
                prepared.function(),
                FunctionRef::TiPb(ScalarFuncSig::PlusInt)
            );
            assert_eq!(prepared.site(), &ordinary_site(profile, 0));
            assert_eq!(args.len(), 2);
            let [RpnExpressionNode::OrdinaryFnCall { prepared, .. }] = args[1].as_ref() else {
                panic!("expected a prepared ordinary right child");
            };
            // The intervening input leaf is ordinal 1, not a call site.
            assert_eq!(prepared.site(), &ordinary_site(profile, 2));
            assert_eq!(prepared.return_type(), &field_type);
        }
    }

    #[test]
    fn test_ordinary_metadata_counts_one_call_for_each_argument_shape() {
        let field_type: FieldType = FieldTypeTp::LongLong.into();
        let schema = vec![field_type.clone(); 2];
        let spec = ordinary_spec(
            ordinary_input(0, &field_type),
            ordinary_input(1, &field_type),
            &field_type,
        );
        for arity in [0, 1, 4] {
            let mut expression = compile_ordinary(
                &spec,
                &schema,
                OrdinaryProfile::TypedRow,
                CompileLimits::default(),
            );
            assert_eq!(expression.node_count(), 3);
            // Only the tree walk is under test: these deliberately changed
            // child counts are never evaluated or admitted as PlusInt calls.
            // Mutable access must invalidate the already populated cache.
            let [RpnExpressionNode::OrdinaryFnCall { args, .. }] = expression.as_mut() else {
                panic!("expected a prepared ordinary root");
            };
            *args = (0..arity).map(|index| column(index % 2)).collect();
            assert_eq!(expression.node_count(), arity + 1);
            assert_eq!(expression.work_count(), arity + 1);
            assert_eq!(expression.column_ref_count(), arity);
            let offsets: Vec<_> = (0..arity.min(2)).collect();
            assert_eq!(expression.referenced_column_offsets(), offsets.as_slice());
        }
    }

    #[test]
    fn test_deep_ordinary_metadata_and_drop_on_small_stack() {
        thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn_wrapper(|| {
                let field_type: FieldType = FieldTypeTp::LongLong.into();
                let schema = vec![field_type.clone(); 4];
                for depth in [33, 64, 256, 16_384] {
                    let mut spec = ordinary_input(0, &field_type);
                    for level in 0..depth {
                        spec = ordinary_spec(
                            spec,
                            ordinary_input((level + 1) % 4, &field_type),
                            &field_type,
                        );
                    }
                    let limits = CompileLimits {
                        max_nodes: 2 * depth + 1,
                        max_depth: depth + 1,
                    };
                    let expression =
                        compile_ordinary(&spec, &schema, OrdinaryProfile::TypedRow, limits);
                    assert_eq!(expression.node_count(), 2 * depth + 1);
                    assert_eq!(expression.work_count(), 2 * depth + 1);
                    assert_eq!(expression.column_ref_count(), depth + 1);
                    assert_eq!(expression.referenced_column_offsets(), &[0, 1, 2, 3]);
                    drop(expression);

                    // Also tear down an uncached tree after extracting its root
                    // vector, without recursively cloning either specification.
                    let expression =
                        compile_ordinary(&spec, &schema, OrdinaryProfile::TypedRow, limits);
                    drop(expression.into_inner());
                    drop(spec);
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn test_ordinary_into_inner_drops_mixed_child_metadata_once() {
        struct DropCounter(Arc<AtomicUsize>);

        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let field_type: FieldType = FieldTypeTp::LongLong.into();
        let schema = vec![field_type.clone(); 2];
        let spec = ordinary_spec(
            ordinary_input(0, &field_type),
            ordinary_input(1, &field_type),
            &field_type,
        );
        let mut expression = compile_ordinary(
            &spec,
            &schema,
            OrdinaryProfile::TypedRow,
            CompileLimits::default(),
        );
        let drops = Arc::new(AtomicUsize::new(0));
        let [RpnExpressionNode::OrdinaryFnCall { args, .. }] = expression.as_mut() else {
            panic!("expected a prepared ordinary root");
        };
        for arg in args.iter_mut() {
            // Destruction-only sentinels, not executable/admitted kernel calls.
            *arg = RpnExpression::from(vec![RpnExpressionNode::FnCall {
                func_meta: crate::impl_op::logical_and_fn_meta(),
                args_len: 2,
                field_type: field_type.clone(),
                metadata: Box::new(DropCounter(Arc::clone(&drops))),
            }]);
        }
        let logical = [
            logical_meta(ScalarFuncSig::LogicalAnd),
            logical_meta(ScalarFuncSig::LogicalOr),
        ];
        let expression = nested_host_controls(expression, 64, &host_catalog(1), logical);
        assert_eq!(expression.node_count(), 99);
        assert_eq!(expression.work_count(), 99);
        let nodes = expression.into_inner();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(nodes);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_host_metadata_counts_one_call_plus_children() {
        fn assert_send<T: Send>() {}
        assert_send::<RpnExpression>();

        let field_type: FieldType = FieldTypeTp::LongLong.into();
        for arity in [0, 1, 4] {
            let catalog = host_catalog(arity);
            let expression = host_call(&catalog, (0..arity).map(|i| column(i % 2)).collect());
            assert_eq!(expression.node_count(), arity + 1);
            assert_eq!(expression.work_count(), arity + 1);
            assert_eq!(expression.column_ref_count(), arity);
            let offsets: Vec<_> = (0..arity.min(2)).collect();
            assert_eq!(expression.referenced_column_offsets(), offsets.as_slice());
            assert_eq!(expression.ret_field_type(&[]), &field_type);
            assert_eq!(expression[0].field_type(), &field_type);
            assert_eq!(expression[0].expr_tp(), tipb::ExprType::ScalarFunc);
            assert!(!expression.is_last_constant());
        }
    }

    #[test]
    fn test_deep_host_metadata_and_drop_on_small_stack() {
        let logical = [
            logical_meta(ScalarFuncSig::LogicalAnd),
            logical_meta(ScalarFuncSig::LogicalOr),
        ];
        thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn_wrapper(move || {
                let catalog = host_catalog(1);
                for depth in [33, 64, 256, 16_384] {
                    let expression = nested_host_controls(column(0), depth, &catalog, logical);
                    let controls = depth / 2;
                    assert_eq!(expression.node_count(), depth + controls + 1);
                    assert_eq!(expression.work_count(), depth + controls + 1);
                    assert_eq!(expression.column_ref_count(), controls + 1);
                    assert_eq!(expression.referenced_column_offsets(), &[0, 1, 2, 3]);
                    drop(expression);

                    let expression = nested_host_controls(column(0), depth, &catalog, logical);
                    drop(expression.into_inner());
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn test_host_into_inner_drops_child_metadata_once() {
        struct DropCounter(Arc<AtomicUsize>);

        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let catalog = host_catalog(4);
        // These opaque sentinels test destruction only, never kernel execution.
        let children = (0..4)
            .map(|_| {
                RpnExpression::from(vec![RpnExpressionNode::FnCall {
                    func_meta: crate::impl_op::logical_and_fn_meta(),
                    args_len: 2,
                    field_type: FieldTypeTp::LongLong.into(),
                    metadata: Box::new(DropCounter(Arc::clone(&drops))),
                }])
            })
            .collect();
        let expression = host_call(&catalog, children);
        assert_eq!(expression.node_count(), 5);
        let nodes = expression.into_inner();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(nodes);
        assert_eq!(drops.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn test_control_metadata_counts_only_logical_chains_as_binary_work() {
        let logical = logical_meta(ScalarFuncSig::LogicalAnd);
        for (sig, arity, own_work) in [
            (ScalarFuncSig::LogicalAnd, 4, 3),
            (ScalarFuncSig::LogicalOr, 4, 3),
            (ScalarFuncSig::IfInt, 3, 1),
            (ScalarFuncSig::CoalesceInt, 4, 1),
        ] {
            // This test exercises metadata only, not evaluation. The real
            // signature determines work regardless of the execution descriptor.
            let mut func_meta = logical;
            func_meta.sig = sig;
            let expression = RpnExpression::from(vec![RpnExpressionNode::ShortCircuitFnCall {
                func_meta,
                args: (0..arity).map(|i| column(i % 2)).collect(),
                field_type: FieldTypeTp::LongLong.into(),
            }]);
            assert_eq!(expression.node_count(), arity + 1);
            assert_eq!(expression.work_count(), arity + own_work);
            assert_eq!(expression.column_ref_count(), arity);
            assert_eq!(expression.referenced_column_offsets(), &[0, 1]);
        }
    }

    #[test]
    fn test_deep_metadata_and_drop_on_small_stack() {
        let logical = [
            logical_meta(ScalarFuncSig::LogicalAnd),
            logical_meta(ScalarFuncSig::LogicalOr),
        ];
        thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn_wrapper(move || {
                for depth in [33, 64, 256, 16_384] {
                    let expression = nested_controls(column(0), depth, logical);
                    assert_eq!(expression.node_count(), 2 * depth + 1);
                    assert_eq!(expression.work_count(), 2 * depth + 1);
                    assert_eq!(expression.column_ref_count(), depth + 1);
                    assert_eq!(expression.referenced_column_offsets(), &[0, 1, 2, 3]);
                    drop(expression);

                    // Teardown must also work without a populated metadata
                    // cache and after handing the root node vector to a caller.
                    let expression = nested_controls(column(0), depth, logical);
                    drop(expression.into_inner());
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn test_into_inner_retains_nested_metadata_until_nodes_drop() {
        struct DropCounter(Arc<AtomicUsize>);

        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        // The opaque metadata is a teardown sentinel; this expression is never
        // evaluated or passed through function preparation.
        let leaf = RpnExpression::from(vec![RpnExpressionNode::FnCall {
            func_meta: crate::impl_op::logical_and_fn_meta(),
            args_len: 2,
            field_type: FieldTypeTp::LongLong.into(),
            metadata: Box::new(DropCounter(Arc::clone(&drops))),
        }]);
        let logical = [
            logical_meta(ScalarFuncSig::LogicalAnd),
            logical_meta(ScalarFuncSig::LogicalOr),
        ];
        let expression = nested_controls(leaf, 64, logical);
        assert_eq!(expression.node_count(), 129);
        let nodes = expression.into_inner();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(nodes);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

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

    #[test]
    fn test_retained_metadata_observer_stays_cold_and_excludes_node_capacity() {
        let mut nodes = Vec::with_capacity(32);
        nodes.push(RpnExpressionNode::ColumnRef { offset: 2 });
        let expression = RpnExpression::from(nodes);
        let node_capacity = expression.capacity();
        assert!(node_capacity >= 32);
        for _ in 0..3 {
            assert!(expression.metadata.get().is_none());
            assert_eq!(expression.retained_metadata_heap_bytes(), Some(0));
            assert!(expression.metadata.get().is_none());
            assert_eq!(expression.capacity(), node_capacity);
            assert_eq!(expression.len(), 1);
        }
    }

    #[test]
    fn test_retained_metadata_observer_counts_warm_box_with_no_offsets() {
        let expression = RpnExpression::from(vec![]);
        assert_eq!(expression.retained_metadata_heap_bytes(), Some(0));
        assert!(expression.metadata.get().is_none());
        // Only a real metadata getter initializes the box, even for no nodes.
        assert_eq!(expression.node_count(), 0);
        let metadata = expression.metadata.get().unwrap();
        assert_eq!(metadata.referenced_column_offsets.capacity(), 0);
        let address = metadata.as_ref() as *const RpnExpressionMetadata;
        for _ in 0..3 {
            assert_eq!(
                expression.retained_metadata_heap_bytes(),
                Some(mem::size_of::<RpnExpressionMetadata>())
            );
            assert_eq!(
                expression.metadata.get().unwrap().as_ref() as *const RpnExpressionMetadata,
                address
            );
        }
    }

    #[test]
    fn test_retained_metadata_observer_counts_offset_capacity_and_is_stable() {
        let mut nodes = Vec::with_capacity(64);
        nodes.extend([
            RpnExpressionNode::ColumnRef { offset: 2 },
            RpnExpressionNode::ColumnRef { offset: 0 },
            RpnExpressionNode::ColumnRef { offset: 2 },
        ]);
        let mut expression = RpnExpression::from(nodes);
        assert_eq!(expression.retained_metadata_heap_bytes(), Some(0));
        assert!(expression.metadata.get().is_none());
        assert_eq!(expression.node_count(), 3);
        assert_eq!(expression.referenced_column_offsets(), &[0, 2]);
        let (offset_len, offset_capacity) = {
            let metadata = expression.metadata.get_mut().unwrap();
            // Test-only spare capacity changes no logical metadata. The public
            // offsets slice cannot describe this owned allocation accurately.
            metadata.referenced_column_offsets.reserve(64);
            (
                metadata.referenced_column_offsets.len(),
                metadata.referenced_column_offsets.capacity(),
            )
        };
        assert!(offset_capacity > offset_len);
        let expected = mem::size_of::<RpnExpressionMetadata>()
            .checked_add(
                offset_capacity
                    .checked_mul(mem::size_of::<usize>())
                    .unwrap(),
            )
            .unwrap();
        assert!(
            expected
                > mem::size_of::<RpnExpressionMetadata>() + offset_len * mem::size_of::<usize>()
        );
        let metadata = expression.metadata.get().unwrap();
        let metadata_address = metadata.as_ref() as *const RpnExpressionMetadata;
        let offsets_address = metadata.referenced_column_offsets.as_ptr();
        let node_capacity = expression.capacity();
        for _ in 0..3 {
            assert_eq!(expression.retained_metadata_heap_bytes(), Some(expected));
            let metadata = expression.metadata.get().unwrap();
            assert_eq!(
                metadata.as_ref() as *const RpnExpressionMetadata,
                metadata_address
            );
            assert_eq!(metadata.referenced_column_offsets.as_ptr(), offsets_address);
            assert_eq!(
                metadata.referenced_column_offsets.capacity(),
                offset_capacity
            );
            assert_eq!(expression.referenced_column_offsets(), &[0, 2]);
            assert_eq!(expression.node_count(), 3);
            assert_eq!(expression.work_count(), 3);
            assert_eq!(expression.column_ref_count(), 3);
            assert_eq!(expression.capacity(), node_capacity);
        }
    }
}
