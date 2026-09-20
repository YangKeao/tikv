// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use tidb_query_common::Result;
pub use tidb_query_datatype::codec::data_type::{
    BATCH_MAX_SIZE, IDENTICAL_LOGICAL_ROWS, LogicalRows,
};
use tidb_query_datatype::{
    EvalType, FieldTypeAccessor,
    codec::{batch::LazyBatchColumnVec, data_type::*},
    expr::EvalContext,
};
use tipb::FieldType;

use super::{
    LazyChildren, RpnFnCallExtra,
    expr::{RpnExpression, RpnExpressionNode},
};

/// One decoded input column read at an ordered logical-row selection.
///
/// Each input owns its selection so a caller can represent a window frame or
/// join pair without copying and concatenating physical columns. Selections may
/// repeat or reorder rows; all selected inputs of one evaluation have the same
/// length.
#[derive(Clone, Copy)]
pub struct RpnSelectedColumn<'a> {
    /// Decoded physical vector for this input offset.
    pub physical_value: &'a VectorValue,
    /// Physical rows read for output positions in order.
    pub logical_rows: &'a [usize],
}

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
        for node in self.as_ref() {
            if let RpnExpressionNode::ColumnRef { offset, .. } = node {
                input_physical_columns[*offset].ensure_decoded(
                    ctx,
                    &schema[*offset],
                    LogicalRows::from_slice(input_logical_rows),
                )?;
            }
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
        self.eval_decoded_impl::<false>(
            ctx,
            schema,
            input_physical_columns,
            input_logical_rows,
            output_rows,
        )
    }

    /// Standalone-only safety boundary. Finite inputs do not guarantee finite
    /// kernel outputs (e.g. vector distance or rounding can overflow). Reject
    /// every nonfinite REAL before another kernel consumes it via NotNan
    /// arithmetic. This is not fallback and leaves the normal evaluator intact.
    ///
    /// The per-call RPN stack scratch is taken from the caller. The stack holds
    /// references into `self` and the inputs, so it is inherently call-scoped
    /// and cannot be stored in the compiled program; supplying it explicitly
    /// only makes the execution-state split visible at the facade.
    /// `eval_decoded` keeps allocating its own stack and its behavior is
    /// unchanged.
    pub(crate) fn eval_decoded_with_finite_reals_into<'a>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input_physical_columns: &'a LazyBatchColumnVec,
        input_logical_rows: &'a [usize],
        output_rows: usize,
        stack: &mut Vec<RpnStackNode<'a>>,
    ) -> Result<RpnStackNode<'a>> {
        self.eval_decoded_into::<true>(
            ctx,
            schema,
            input_physical_columns,
            input_logical_rows,
            output_rows,
            stack,
        )
    }

    fn eval_decoded_impl<'a, const CHECK_FINITE_REALS: bool>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input_physical_columns: &'a LazyBatchColumnVec,
        input_logical_rows: &'a [usize],
        output_rows: usize,
    ) -> Result<RpnStackNode<'a>> {
        let mut stack = Vec::with_capacity(self.len());
        self.eval_decoded_into::<CHECK_FINITE_REALS>(
            ctx,
            schema,
            input_physical_columns,
            input_logical_rows,
            output_rows,
            &mut stack,
        )
    }

    /// Compatibility adapter for the legacy shared-selection input shape.
    fn eval_decoded_into<'a, const CHECK_FINITE_REALS: bool>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input_physical_columns: &'a LazyBatchColumnVec,
        input_logical_rows: &'a [usize],
        output_rows: usize,
        stack: &mut Vec<RpnStackNode<'a>>,
    ) -> Result<RpnStackNode<'a>> {
        let inputs: Vec<RpnSelectedColumn<'a>> = (0..input_physical_columns.columns_len())
            .map(|offset| RpnSelectedColumn {
                physical_value: input_physical_columns[offset].decoded(),
                logical_rows: input_logical_rows,
            })
            .collect();
        self.eval_decoded_selected_into::<CHECK_FINITE_REALS>(
            ctx,
            schema,
            &inputs,
            output_rows,
            stack,
        )
    }

    /// Evaluates already-decoded inputs with an independent ordered selection
    /// per input column. All selections must have `output_rows` entries.
    pub fn eval_decoded_selected<'a>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        inputs: &[RpnSelectedColumn<'a>],
        output_rows: usize,
    ) -> Result<RpnStackNode<'a>> {
        let mut stack = Vec::with_capacity(self.len());
        self.eval_decoded_selected_into::<false>(ctx, schema, inputs, output_rows, &mut stack)
    }

    fn eval_decoded_selected_into<'a, const CHECK_FINITE_REALS: bool>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        inputs: &[RpnSelectedColumn<'a>],
        output_rows: usize,
        stack: &mut Vec<RpnStackNode<'a>>,
    ) -> Result<RpnStackNode<'a>> {
        assert!(!self.is_empty());
        assert!(output_rows > 0);
        assert!(output_rows <= BATCH_MAX_SIZE);
        // The program is exactly the subtree rooted at its last node. The old
        // flat loop rejected a program with unused nodes through
        // `assert_eq!(stack.len(), 1)`; keep that rejection now that only the
        // root subtree is walked.
        assert_eq!(
            self.subtree_start(self.len() - 1),
            0,
            "RPN expression contains nodes outside the root's subtree"
        );
        stack.clear();
        let node = self.eval_subtree::<CHECK_FINITE_REALS>(
            self.len() - 1,
            ctx,
            schema,
            inputs,
            output_rows,
            stack,
        )?;
        debug_assert!(stack.is_empty());
        Ok(node)
    }

    /// Evaluates the subtree whose last (root) node is `root`.
    ///
    /// `scratch` is call-scoped storage for the argument nodes of eager
    /// kernels; it is left exactly as deep as it was on entry. A lazy `FnCall`
    /// never uses it: its children are pulled through [`ChildHandle`].
    ///
    /// The explicit context/schema/columns/rows parameters keep the evaluator
    /// allocation-free and mirror the entry points' borrows; bundling them
    /// would only add a struct that outlives every call.
    #[allow(clippy::too_many_arguments)]
    fn eval_subtree<'x, const CHECK_FINITE_REALS: bool>(
        &'x self,
        root: usize,
        ctx: &mut EvalContext,
        schema: &'x [FieldType],
        inputs: &[RpnSelectedColumn<'x>],
        output_rows: usize,
        scratch: &mut Vec<RpnStackNode<'x>>,
    ) -> Result<RpnStackNode<'x>> {
        assert!(output_rows > 0 && output_rows <= BATCH_MAX_SIZE);

        let node = match &self[root] {
            RpnExpressionNode::Constant { value, field_type } => {
                RpnStackNode::Scalar { value, field_type }
            }
            RpnExpressionNode::ColumnRef { offset } => {
                let input = &inputs[*offset];
                assert_eq!(input.logical_rows.len(), output_rows);
                RpnStackNode::Vector {
                    value: RpnStackNodeVectorValue::Ref {
                        physical_value: input.physical_value,
                        logical_rows: input.logical_rows,
                    },
                    field_type: &schema[*offset],
                }
            }
            RpnExpressionNode::FnCall {
                func_meta,
                args_len,
                field_type: ret_field_type,
                metadata,
            } => {
                // Suppose that we have function call `Foo(A, B, C)`, the RPN nodes looks like
                // `[A, B, C, Foo]`. The children are the contiguous subtrees that
                // immediately precede `Foo`; `args_len` alone determines their roots,
                // so no cached plan or extra node field is needed.
                let roots = self.child_roots(root, *args_len);
                let mut call_extra = RpnFnCallExtra { ret_field_type };
                let ret = match func_meta.lazy_fn_ptr {
                    Some(lazy) => {
                        let mut children = ChildHandle::<CHECK_FINITE_REALS> {
                            expr: self,
                            schema,
                            inputs,
                            roots: &roots,
                            selection: Vec::new(),
                        };
                        (lazy)(
                            ctx,
                            output_rows,
                            &mut children,
                            &mut call_extra,
                            &**metadata,
                        )?
                    }
                    None => {
                        let stack_slice_begin = scratch.len();
                        for &child_root in &roots {
                            let child = self.eval_subtree::<CHECK_FINITE_REALS>(
                                child_root,
                                ctx,
                                schema,
                                inputs,
                                output_rows,
                                scratch,
                            )?;
                            scratch.push(child);
                        }
                        let ret = (func_meta.fn_ptr)(
                            ctx,
                            output_rows,
                            &scratch[stack_slice_begin..],
                            &mut call_extra,
                            &**metadata,
                        )?;
                        scratch.truncate(stack_slice_begin);
                        ret
                    }
                };
                RpnStackNode::Vector {
                    value: RpnStackNodeVectorValue::Generated {
                        physical_value: ret,
                    },
                    field_type: ret_field_type,
                }
            }
        };

        if CHECK_FINITE_REALS {
            ensure_finite_real_node(&node, output_rows)?;
        }
        Ok(node)
    }

    /// Node index of each child subtree of the `FnCall` at `func_call_index`,
    /// in argument order.
    fn child_roots(&self, func_call_index: usize, args_len: usize) -> Vec<usize> {
        assert!(
            func_call_index >= args_len,
            "RPN FnCall has {} arguments but only {} preceding nodes",
            args_len,
            func_call_index
        );
        let mut roots = vec![0; args_len];
        if args_len == 0 {
            // Nullary calls have no preceding nodes; computing `fc - 1` here
            // would underflow when the call is the root node.
            return roots;
        }
        let mut root = func_call_index - 1;
        for index in (0..args_len).rev() {
            roots[index] = root;
            if index > 0 {
                root = self.subtree_start(root) - 1;
            }
        }
        roots
    }

    /// First node index of the subtree whose last node is `root`.
    fn subtree_start(&self, root: usize) -> usize {
        let mut pending = 1usize;
        let mut index = root;
        loop {
            pending -= 1;
            if let RpnExpressionNode::FnCall { args_len, .. } = &self[index] {
                pending += *args_len;
            }
            if pending == 0 {
                return index;
            }
            assert!(
                index > 0,
                "RPN expression is not a well-formed post-order program"
            );
            index -= 1;
        }
    }
}

/// Rejects a nonfinite REAL produced by an *executed* node under the standalone
/// checked entry. A node skipped by a lazy kernel is never materialized as a
/// stack node and never reaches this check.
fn ensure_finite_real_node(node: &RpnStackNode<'_>, output_rows: usize) -> Result<()> {
    if matches!(node.get_logical_scalar_ref(0), ScalarValueRef::Real(_)) {
        for row in 0..output_rows {
            if let ScalarValueRef::Real(Some(value)) = node.get_logical_scalar_ref(row) {
                if !value.is_finite() {
                    return Err(other_err!(
                        "standalone evaluation produced a nonfinite REAL"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The same check for an owned dense vector materialized at a lazy boundary.
/// The child has no stack node of its own, so [`ChildHandle::eval`] applies it
/// directly.
fn ensure_finite_real_vector(value: &VectorValue, output_rows: usize) -> Result<()> {
    if let VectorValue::Real(column) = value {
        for row in 0..output_rows {
            if let Some(value) = column.get_option_ref(row) {
                if !value.is_finite() {
                    return Err(other_err!(
                        "standalone evaluation produced a nonfinite REAL"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Materializes the unevaluated children of one lazy RPN call.
///
/// It never hands out a borrow into a temporary selection:
/// [`LazyChildren::eval`] returns an owned dense [`VectorValue`], so the
/// child's stack nodes (which may be `Ref` nodes over the local selection)
/// cannot escape this call.
struct ChildHandle<'e, 's, 'i, 'r, const CHECK_FINITE_REALS: bool> {
    expr: &'e RpnExpression,
    schema: &'s [FieldType],
    inputs: &'i [RpnSelectedColumn<'s>],
    roots: &'r [usize],
    selection: Vec<Vec<usize>>,
}

impl<'e, 's, 'i, 'r, 'a, const CHECK_FINITE_REALS: bool> LazyChildren<'a>
    for ChildHandle<'e, 's, 'i, 'r, CHECK_FINITE_REALS>
{
    fn len(&self) -> usize {
        self.roots.len()
    }

    fn field_type(&self, arg: usize) -> &FieldType {
        match &self.expr[self.roots[arg]] {
            RpnExpressionNode::Constant { field_type, .. }
            | RpnExpressionNode::FnCall { field_type, .. } => field_type,
            RpnExpressionNode::ColumnRef { offset } => &self.schema[*offset],
        }
    }

    fn scalar_value(&self, arg: usize) -> Option<&ScalarValue> {
        match &self.expr[self.roots[arg]] {
            RpnExpressionNode::Constant { value, .. } => Some(value),
            _ => None,
        }
    }

    fn eval(
        &mut self,
        ctx: &mut EvalContext,
        arg: usize,
        positions: &[usize],
    ) -> Result<VectorValue> {
        if positions.is_empty() {
            // `eval_subtree` asserts `output_rows > 0`; an empty request can
            // never enter the child, so return an empty vector of its type.
            let eval_type = EvalType::try_from(self.field_type(arg).as_accessor().tp())
                .map_err(|e| other_err!("lazy child has an invalid field type: {}", e))?;
            return Ok(VectorValue::with_capacity(0, eval_type));
        }
        self.selection.clear();
        self.selection.extend(self.inputs.iter().map(|input| {
            positions
                .iter()
                .map(|&position| input.logical_rows[position])
                .collect()
        }));
        let selected_inputs: Vec<RpnSelectedColumn<'_>> = self
            .inputs
            .iter()
            .zip(&self.selection)
            .map(|(input, logical_rows)| RpnSelectedColumn {
                physical_value: input.physical_value,
                logical_rows,
            })
            .collect();
        // A lazy boundary materializes immediately, so the child's own node
        // stack stays local to this call and never escapes.
        let mut local_stack: Vec<RpnStackNode<'_>> = Vec::new();
        let node = self.expr.eval_subtree::<CHECK_FINITE_REALS>(
            self.roots[arg],
            ctx,
            self.schema,
            &selected_inputs,
            positions.len(),
            &mut local_stack,
        )?;
        let value = match node {
            RpnStackNode::Scalar { value, .. } => VectorValue::from_scalar(value, positions.len()),
            RpnStackNode::Vector { value, .. } => value.take_vector_value()?,
        };
        if CHECK_FINITE_REALS {
            ensure_finite_real_vector(&value, positions.len())?;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

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
        expr::EvalContext,
    };
    use tipb::FieldType;
    use tipb_helper::ExprDefBuilder;

    use super::*;
    use crate::{RpnExpressionBuilder, RpnFnMeta, impl_arithmetic::*, impl_compare::*};

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

    /// Independently ordered input selections remain separate through eager
    /// evaluation, so callers can represent a join pair without copying input
    /// columns into one dense row space.
    #[test]
    fn test_eval_decoded_selected_uses_per_column_rows() {
        #[rpn_fn(nullable)]
        fn add(left: Option<&i64>, right: Option<&i64>) -> Result<Option<i64>> {
            Ok(left.zip(right).map(|(left, right)| left + right))
        }

        let columns = LazyBatchColumnVec::from(vec![
            {
                let mut column = LazyBatchColumn::decoded_with_capacity_and_tp(3, EvalType::Int);
                for value in [1, 2, 3] {
                    column.mut_decoded().push_int(Some(value));
                }
                column
            },
            {
                let mut column = LazyBatchColumn::decoded_with_capacity_and_tp(3, EvalType::Int);
                for value in [10, 20, 30] {
                    column.mut_decoded().push_int(Some(value));
                }
                column
            },
        ]);
        let schema = &[FieldTypeTp::LongLong.into(), FieldTypeTp::LongLong.into()];
        let left_rows = [2, 0];
        let right_rows = [1, 1];
        let inputs = [
            RpnSelectedColumn {
                physical_value: columns[0].decoded(),
                logical_rows: &left_rows,
            },
            RpnSelectedColumn {
                physical_value: columns[1].decoded(),
                logical_rows: &right_rows,
            },
        ];
        let expression = RpnExpressionBuilder::new_for_test()
            .push_column_ref_for_test(0)
            .push_column_ref_for_test(1)
            .push_fn_call_for_test(add_fn_meta(), 2, FieldTypeTp::LongLong)
            .build_for_test();

        let value = expression
            .eval_decoded_selected(&mut EvalContext::default(), schema, &inputs, 2)
            .unwrap();
        assert_eq!(
            value.vector_value().unwrap().as_ref().to_int_vec(),
            [Some(23), Some(21)]
        );
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
        fn prepare_a<T: Evaluable>(_expr: &mut Expr) -> Result<i64> {
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

        fn prepare_b<T: Evaluable>(_expr: &mut Expr) -> Result<String> {
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

        fn prepare_c<T: Evaluable>(_expr: &mut Expr) -> Result<std::marker::PhantomData<T>> {
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
