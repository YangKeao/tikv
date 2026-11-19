// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::borrow::Cow;

use tidb_query_common::Result;
pub use tidb_query_datatype::codec::data_type::{
    BATCH_MAX_SIZE, IDENTICAL_LOGICAL_ROWS, LogicalRows,
};
use tidb_query_datatype::{
    codec::{batch::LazyBatchColumnVec, data_type::*},
    expr::{EvalContext, EvalWarnings},
};
use tipb::FieldType;

use super::{
    RpnFnCallExtra,
    expr::{
        ExpressionScalar, ExpressionVector, InvocationMode, OutputOrder, RpnExpression,
        RpnExpressionNode,
    },
};

/// A demanded column read. The evaluation context is not borrowed by results.
pub type InputResult<'a> = Result<RpnStackNode<'a>>;

/// Direct warnings from an SDK kernel to the native statement's owner.
#[derive(Debug)]
pub struct NativeKernelWarnings;

/// Adapters supply data and concrete session capabilities; the loop owns
/// evaluation.
pub trait InputColumns {
    /// Parameter bindings are immutable for the duration of one evaluation.
    fn parameter(&self, _order: usize) -> Result<tidb_datatype::Datum> {
        Err(other_err!("native parameters unavailable"))
    }
    /// Publish a SQL warning raised by a native call to an SDK kernel.
    fn append_kernel_warning(&self, _warning: &tipb::Error) {}

    fn conversion_flags(&self) -> tidb_datatype::ConversionFlags {
        tidb_datatype::STRICT_FLAGS
    }
    fn conversion_location(&self) -> tidb_datatype::ConversionLocation {
        tidb_datatype::ConversionLocation::UTC
    }
    fn conversion_warnings(&self) -> &dyn tidb_datatype::ConversionWarningAppender {
        &tidb_datatype::IgnoreConversionWarnings
    }
    fn date_modes(&self) -> tidb_datatype::DateModes {
        tidb_datatype::DateModes::TIDB_DEFAULT_SQL_MODE
    }
    fn div_precision_increment(&self) -> u32 {
        4
    }
    fn handle_truncate(&self, _message: &str) -> Result<()> {
        Err(other_err!("native truncate policy unavailable"))
    }
    fn handle_division_by_zero(&self) -> Result<()> {
        Err(other_err!("native division policy unavailable"))
    }

    fn get_string_variable(&self, _name: &str) -> Result<Option<Vec<u8>>> {
        Err(other_err!("variable reads unavailable"))
    }
    fn set_string_variable(&self, _name: &str, _value: &[u8]) -> Result<()> {
        Err(other_err!("variable writes unavailable"))
    }

    fn rand_next(&self) -> Option<f64> {
        None
    }

    fn read<'a>(
        &'a self,
        schema: &'a [FieldType],
        offset: usize,
        rows: &'a [usize],
        ctx: &mut EvalContext,
    ) -> InputResult<'a>;

    /// A temporary branch selection may not be borrowed by its output.
    fn read_selected<'a>(
        &'a self,
        schema: &'a [FieldType],
        offset: usize,
        rows: &[usize],
        ctx: &mut EvalContext,
    ) -> InputResult<'a> {
        match self.read(schema, offset, rows, ctx)? {
            RpnStackNode::Scalar { value, field_type } => Ok(RpnStackNode::OwnedScalar(Box::new(
                (value.clone(), field_type.clone()),
            ))),
            RpnStackNode::OwnedScalar(value) => Ok(RpnStackNode::OwnedScalar(value)),
            RpnStackNode::ExpressionVector { value, .. } => Ok(RpnStackNode::ExpressionVector {
                value,
                field_type: &schema[offset],
            }),
            RpnStackNode::Vector { value, field_type } => {
                let schema_type = &schema[offset];
                let field_type = match field_type {
                    RpnVectorFieldType::Borrowed(field_type)
                        if std::ptr::eq(field_type, schema_type) =>
                    {
                        schema_type.into()
                    }
                    RpnVectorFieldType::Borrowed(field_type) => {
                        RpnVectorFieldType::Owned(Box::new(field_type.clone()))
                    }
                    RpnVectorFieldType::Owned(field_type) => RpnVectorFieldType::Owned(field_type),
                };
                Ok(RpnStackNode::Vector {
                    value: RpnStackNodeVectorValue::Generated {
                        physical_value: value.take_vector_value()?,
                    },
                    field_type,
                })
            }
            RpnStackNode::ExpressionScalar { .. } => {
                Err(other_err!("take_vector_value on Scalar variant"))
            }
        }
    }
}

impl InputColumns for LazyBatchColumnVec {
    fn read<'a>(
        &'a self,
        schema: &'a [FieldType],
        offset: usize,
        rows: &'a [usize],
        _ctx: &mut EvalContext,
    ) -> InputResult<'a> {
        Ok(RpnStackNode::Vector {
            value: RpnStackNodeVectorValue::Ref {
                physical_value: self[offset].decoded(),
                logical_rows: rows,
            },
            field_type: (&schema[offset]).into(),
        })
    }
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

/// A PB vector's field declaration, which may outlive a temporary row
/// selection.
#[derive(Debug)]
pub enum RpnVectorFieldType<'a> {
    Borrowed(&'a FieldType),
    Owned(Box<FieldType>),
}

impl<'a> From<&'a FieldType> for RpnVectorFieldType<'a> {
    fn from(field_type: &'a FieldType) -> Self {
        Self::Borrowed(field_type)
    }
}

impl AsRef<FieldType> for RpnVectorFieldType<'_> {
    fn as_ref(&self) -> &FieldType {
        match self {
            Self::Borrowed(field_type) => field_type,
            Self::Owned(field_type) => field_type,
        }
    }
}

/// A type for each node in the RPN evaluation stack. It can be one of a scalar
/// value node or a vector value node. The vector value node can be either an
/// owned vector value or a reference.
#[derive(Debug)]
pub enum RpnStackNode<'a> {
    /// Full-domain native intermediate column.
    ExpressionVector {
        value: ExpressionVector,
        field_type: &'a FieldType,
    },
    /// Full-domain native scalar; no protobuf constructor emits this variant.
    ExpressionScalar {
        value: Cow<'a, ExpressionScalar>,
        field_type: &'a FieldType,
    },
    /// Represents a scalar value. Comes from a constant node in expression
    /// list.
    Scalar {
        value: &'a ScalarValue,
        field_type: &'a FieldType,
    },

    /// A scalar and field declaration retained after a temporary selection.
    OwnedScalar(Box<(ScalarValue, FieldType)>),

    /// Represents a vector value. Comes from a column reference or evaluated
    /// result.
    Vector {
        value: RpnStackNodeVectorValue<'a>,
        field_type: RpnVectorFieldType<'a>,
    },
}

impl RpnStackNode<'_> {
    /// Gets the field type.
    #[inline]
    pub fn field_type(&self) -> &FieldType {
        match self {
            RpnStackNode::Scalar { field_type, .. }
            | RpnStackNode::ExpressionScalar { field_type, .. }
            | RpnStackNode::ExpressionVector { field_type, .. } => field_type,
            RpnStackNode::Vector { field_type, .. } => field_type.as_ref(),
            RpnStackNode::OwnedScalar(value) => &value.1,
        }
    }

    /// Borrows the inner scalar value for `Scalar` variant.
    #[inline]
    pub fn scalar_value(&self) -> Option<&ScalarValue> {
        match self {
            RpnStackNode::Scalar { value, .. } => Some(*value),
            RpnStackNode::OwnedScalar(value) => Some(&value.0),
            RpnStackNode::Vector { .. }
            | RpnStackNode::ExpressionScalar { .. }
            | RpnStackNode::ExpressionVector { .. } => None,
        }
    }

    /// Borrows the inner vector value for `Vector` variant.
    #[inline]
    pub fn vector_value(&self) -> Option<&RpnStackNodeVectorValue<'_>> {
        match self {
            RpnStackNode::Scalar { .. }
            | RpnStackNode::OwnedScalar(_)
            | RpnStackNode::ExpressionScalar { .. }
            | RpnStackNode::ExpressionVector { .. } => None,
            RpnStackNode::Vector { value, .. } => Some(value),
        }
    }

    /// Whether this is a `Scalar` variant.
    #[inline]
    pub fn is_scalar(&self) -> bool {
        matches!(
            self,
            RpnStackNode::Scalar { .. }
                | RpnStackNode::OwnedScalar(_)
                | RpnStackNode::ExpressionScalar { .. }
        )
    }

    /// Whether this is a `Vector` variant.
    #[inline]
    pub fn is_vector(&self) -> bool {
        matches!(
            self,
            RpnStackNode::Vector { .. } | RpnStackNode::ExpressionVector { .. }
        )
    }

    /// Gets the actual vector value.
    pub fn take_vector_value(self) -> Result<VectorValue> {
        match self {
            RpnStackNode::Scalar { .. }
            | RpnStackNode::OwnedScalar(_)
            | RpnStackNode::ExpressionScalar { .. }
            | RpnStackNode::ExpressionVector { .. } => {
                Err(other_err!("take_vector_value on Scalar variant"))
            }
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
            RpnStackNode::OwnedScalar(value) => value.0.as_scalar_value_ref(),
            RpnStackNode::ExpressionScalar { .. } | RpnStackNode::ExpressionVector { .. } => {
                panic!("native scalar requires a native-aware kernel")
            }
        }
    }
}

/// State for one execution of one frozen program. Local instruction ordinals
/// distinguish seeded RAND and literal regexp occurrences; dropping this object
/// ends their state.
pub struct RpnExecution<'p> {
    program: &'p RpnExpression,
    state: ExecutionState,
}

struct ExecutionState {
    seeded: SeededState,
    regexps: std::collections::HashMap<usize, crate::impl_regexp::NativeRegexp>,
}

struct SeededState {
    enabled: bool,
    values: std::collections::HashMap<usize, crate::impl_math::MySqlRng>,
}

impl ExecutionState {
    fn new(supports_seeded_rand: bool) -> Self {
        Self {
            regexps: std::collections::HashMap::new(),
            seeded: SeededState {
                enabled: supports_seeded_rand,
                values: std::collections::HashMap::new(),
            },
        }
    }
}

/// Owns a frozen program and its private state without requiring shared
/// access to the program's function metadata.
pub struct OwnedRpnExecution {
    program: RpnExpression,
    state: ExecutionState,
}

impl OwnedRpnExecution {
    pub fn new(program: RpnExpression, supports_seeded_rand: bool) -> Self {
        Self {
            program,
            state: ExecutionState::new(supports_seeded_rand),
        }
    }

    /// Drop all state from the prior execution while retaining the program.
    pub fn reset(&mut self, supports_seeded_rand: bool) {
        self.state = ExecutionState::new(supports_seeded_rand);
    }

    pub fn borrow_execution(&mut self) -> RpnExecutionRef<'_, '_> {
        RpnExecutionRef {
            program: &self.program,
            state: &mut self.state,
        }
    }
}

/// Short-lived view of a program and its associated execution state.
pub struct RpnExecutionRef<'p, 's> {
    program: &'p RpnExpression,
    state: &'s mut ExecutionState,
}

impl<'p> RpnExecution<'p> {
    pub fn new(program: &'p RpnExpression, supports_seeded_rand: bool) -> Self {
        Self {
            program,
            state: ExecutionState::new(supports_seeded_rand),
        }
    }

    pub fn borrow_execution(&mut self) -> RpnExecutionRef<'p, '_> {
        RpnExecutionRef {
            program: self.program,
            state: &mut self.state,
        }
    }

    pub fn eval_input_mode<'a>(
        &mut self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input: &'a dyn InputColumns,
        rows: &'a [usize],
        output_rows: usize,
        mode: InvocationMode,
    ) -> Result<RpnStackNode<'a>>
    where
        'p: 'a,
    {
        self.borrow_execution()
            .eval_input_mode(ctx, schema, input, rows, output_rows, mode)
    }

    /// Both frontend projections share the same expression loop and traversal.
    pub fn eval_group<'a>(
        programs: &mut [(&mut RpnExecution<'p>, &'a [FieldType], &'a dyn InputColumns)],
        ctx: &mut EvalContext,
        rows: &'a [usize],
        order: OutputOrder,
        mode: InvocationMode,
    ) -> Result<Vec<RpnStackNode<'a>>>
    where
        'p: 'a,
    {
        let mut views: Vec<_> = programs
            .iter_mut()
            .map(|(execution, schema, input)| (execution.borrow_execution(), *schema, *input))
            .collect();
        RpnExecutionRef::eval_group(&mut views, ctx, rows, order, mode)
    }
}

impl<'p> RpnExecutionRef<'p, '_> {
    pub fn program(&self) -> &'p RpnExpression {
        self.program
    }

    /// Evaluate one root while retaining the owning execution's state.
    pub fn eval_input_mode<'a>(
        &mut self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input: &'a dyn InputColumns,
        rows: &'a [usize],
        output_rows: usize,
        mode: InvocationMode,
    ) -> Result<RpnStackNode<'a>>
    where
        'p: 'a,
    {
        self.program.eval_input_mode_with_state(
            ctx,
            schema,
            input,
            rows,
            output_rows,
            mode,
            &mut self.state.seeded,
            &mut self.state.regexps,
        )
    }

    /// Both frontend projections share the same expression loop and traversal.
    pub fn eval_group<'a>(
        programs: &mut [(
            RpnExecutionRef<'p, '_>,
            &'a [FieldType],
            &'a dyn InputColumns,
        )],
        ctx: &mut EvalContext,
        rows: &'a [usize],
        order: OutputOrder,
        mode: InvocationMode,
    ) -> Result<Vec<RpnStackNode<'a>>>
    where
        'p: 'a,
    {
        if rows.is_empty() || rows.len() > BATCH_MAX_SIZE {
            return Err(other_err!("invalid group row count"));
        }
        match order {
            OutputOrder::Columns => programs
                .iter_mut()
                .map(|(program, schema, input)| {
                    if mode == InvocationMode::Vector {
                        return program.eval_input_mode(
                            ctx,
                            schema,
                            *input,
                            rows,
                            rows.len(),
                            InvocationMode::Vector,
                        );
                    }
                    let values = (0..rows.len())
                        .map(|i| {
                            program.eval_input_mode(
                                ctx,
                                schema,
                                *input,
                                &rows[i..i + 1],
                                1,
                                InvocationMode::Scalar,
                            )
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let positions: Vec<_> = (0..rows.len()).map(|i| [i]).collect();
                    merge_selected_values(
                        values.iter().zip(&positions).map(|(v, p)| (v, &p[..])),
                        rows.len(),
                        program.program.ret_field_type(schema),
                    )
                })
                .collect(),
            OutputOrder::Rows => {
                let mut columns: Vec<Vec<RpnStackNode<'a>>> = (0..programs.len())
                    .map(|_| Vec::with_capacity(rows.len()))
                    .collect();
                for i in 0..rows.len() {
                    for (out, (program, schema, input)) in
                        columns.iter_mut().zip(programs.iter_mut())
                    {
                        out.push(program.eval_input_mode(
                            ctx,
                            schema,
                            *input,
                            &rows[i..i + 1],
                            1,
                            InvocationMode::Scalar,
                        )?);
                    }
                }
                let positions: Vec<_> = (0..rows.len()).map(|i| [i]).collect();
                columns
                    .into_iter()
                    .zip(programs)
                    .map(|(values, (program, schema, _))| {
                        merge_selected_values(
                            values.iter().zip(&positions).map(|(v, p)| (v, &p[..])),
                            rows.len(),
                            program.program.ret_field_type(schema),
                        )
                    })
                    .collect()
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    static EVALUATOR_ENTRIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
        self.eval_input(
            ctx,
            schema,
            input_physical_columns,
            input_logical_rows,
            output_rows,
        )
    }

    /// Evaluate with a demanded input adapter using the same instruction loop.
    pub fn eval_input<'a>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input_physical_columns: &'a dyn InputColumns,
        input_logical_rows: &'a [usize],
        output_rows: usize,
    ) -> Result<RpnStackNode<'a>> {
        self.eval_input_mode(
            ctx,
            schema,
            input_physical_columns,
            input_logical_rows,
            output_rows,
            InvocationMode::Vector,
        )
    }

    /// Scalar demand is explicit even for a width-one vector.
    pub fn eval_input_mode<'a>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input_physical_columns: &'a dyn InputColumns,
        input_logical_rows: &'a [usize],
        output_rows: usize,
        mode: InvocationMode,
    ) -> Result<RpnStackNode<'a>> {
        RpnExecution::new(self, false).eval_input_mode(
            ctx,
            schema,
            input_physical_columns,
            input_logical_rows,
            output_rows,
            mode,
        )
    }

    fn eval_input_mode_with_state<'a>(
        &'a self,
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input_physical_columns: &'a dyn InputColumns,
        input_logical_rows: &'a [usize],
        output_rows: usize,
        mode: InvocationMode,
        seeded: &mut SeededState,
        regexps: &mut std::collections::HashMap<usize, crate::impl_regexp::NativeRegexp>,
    ) -> Result<RpnStackNode<'a>> {
        #[cfg(test)]
        EVALUATOR_ENTRIES.with(|entries| entries.set(entries.get() + 1));
        if mode == InvocationMode::Scalar && output_rows != 1 {
            return Err(other_err!("scalar invocation requires one row"));
        }
        assert!(output_rows > 0);
        assert!(output_rows <= BATCH_MAX_SIZE);

        // A single-node expression does not need the temporary RPN stack below.
        // This is especially common for short-circuit arguments such as constants
        // and column references, so evaluate it directly to avoid a heap allocation.
        // TODO: Maybe import SmallVec is better?
        if self.len() == 1 {
            let (_, result) = Self::eval_one_node(
                ctx,
                schema,
                input_physical_columns,
                input_logical_rows,
                Some(input_logical_rows),
                output_rows,
                &self.as_ref()[0],
                &[],
                seeded,
                regexps,
                0,
            )?;
            return Ok(result);
        }

        // TODO: Is this stack can be reused?
        let mut stack = Vec::with_capacity(self.len());

        struct Branch<'a> {
            end: usize,
            otherwise: usize,
            rows: Cow<'a, [usize]>,
            then_positions: Vec<usize>,
            else_positions: Vec<usize>,
            then_value: Option<RpnStackNode<'a>>,
            depth: usize,
            field_type: &'a FieldType,
        }
        struct Ordered<'a> {
            start: usize,
            end: usize,
            rows: Cow<'a, [usize]>,
            mode: InvocationMode,
            values: Vec<RpnStackNode<'a>>,
            depth: usize,
            branch_depth: usize,
            field_type: &'a FieldType,
        }
        struct Repeat<'a> {
            start: usize,
            end: usize,
            remaining: u64,
            depth: usize,
            field_type: &'a FieldType,
        }
        struct Logical<'a> {
            end: usize,
            depth: usize,
            parent_mode: InvocationMode,
            state: crate::impl_op::LogicalState<'a>,
        }
        let mut logicals: Vec<Logical<'a>> = Vec::new();
        let mut repeats: Vec<Repeat<'a>> = Vec::new();
        let mut ordered: Vec<Ordered<'a>> = Vec::new();
        let mut mode = mode;
        let mut branches: Vec<Branch<'a>> = Vec::new();
        let mut rows = Cow::Borrowed(input_logical_rows);
        let mut active_count = output_rows;
        let mut pc = 0;
        loop {
            if repeats.last().is_some_and(|repeat| repeat.end - 1 == pc) {
                let repeat = repeats.last_mut().unwrap();
                if stack.len() != repeat.depth + 1 {
                    return Err(other_err!("invalid repeat stack"));
                }
                stack.pop();
                repeat.remaining -= 1;
                if repeat.remaining != 0 {
                    pc = repeat.start;
                } else {
                    let repeat = repeats.pop().unwrap();
                    stack.push(integer_result(Some(0), repeat.field_type));
                    pc = repeat.end;
                }
                continue;
            }
            if ordered
                .last()
                .is_some_and(|frame| frame.end == pc && frame.branch_depth == branches.len())
            {
                let frame = ordered.last_mut().unwrap();
                if stack.len() != frame.depth + 1 {
                    return Err(other_err!("invalid ordered region stack"));
                }
                frame.values.push(stack.pop().unwrap());
                if frame.values.len() < frame.rows.len() {
                    rows = Cow::Owned(vec![frame.rows[frame.values.len()]]);
                    pc = frame.start;
                    continue;
                }
                let frame = ordered.pop().unwrap();
                let positions = (0..frame.values.len()).map(|i| [i]).collect::<Vec<_>>();
                let inputs = frame
                    .values
                    .iter()
                    .zip(positions.iter())
                    .map(|(value, position)| (value, &position[..]));
                stack.push(merge_selected_values(
                    inputs,
                    frame.rows.len(),
                    frame.field_type,
                )?);
                rows = frame.rows;
                active_count = rows.len();
                mode = frame.mode;
                continue;
            }
            if branches.last().is_some_and(|branch| branch.end == pc) {
                let branch = branches.last_mut().unwrap();
                if stack.len() != branch.depth + 1 {
                    return Err(other_err!("invalid branch stack depth"));
                }
                let value = stack.pop().unwrap();
                if branch.then_value.is_none() {
                    branch.then_value = Some(value);
                    rows = Cow::Owned(
                        branch
                            .else_positions
                            .iter()
                            .map(|i| branch.rows[*i])
                            .collect(),
                    );
                    active_count = rows.len();
                    pc = branch.otherwise;
                    continue;
                }
                let branch = branches.pop().unwrap();
                let merged = merge_branch_values(
                    branch.then_value.unwrap(),
                    value,
                    &branch.then_positions,
                    &branch.else_positions,
                    branch.rows.len(),
                    branch.field_type,
                )?;
                rows = branch.rows;
                active_count = rows.len();
                stack.push(merged);
                continue;
            }
            if pc == self.len() {
                break;
            }
            let node = &self.as_ref()[pc];
            match node {
                RpnExpressionNode::LogicalStart {
                    and,
                    end,
                    preserve_mode,
                } => {
                    if *end <= pc + 4
                        || *end > self.len()
                        || logicals.last().is_some_and(|parent| *end >= parent.end)
                        || !matches!(self[end - 1], RpnExpressionNode::LogicalEnd { .. })
                    {
                        return Err(other_err!("invalid logical region"));
                    }
                    assert!(rows.is_empty() || rows.len() == active_count);
                    logicals.push(Logical {
                        end: *end,
                        depth: stack.len(),
                        parent_mode: mode,
                        state: crate::impl_op::LogicalState::new(*and, active_count),
                    });
                    if !preserve_mode {
                        mode = InvocationMode::Vector;
                    }
                    pc += 1;
                    continue;
                }
                RpnExpressionNode::LogicalNext | RpnExpressionNode::LogicalEnd { .. } => {
                    let frame = logicals
                        .last_mut()
                        .ok_or_else(|| other_err!("missing logical region"))?;
                    let last = matches!(node, RpnExpressionNode::LogicalEnd { .. });
                    if stack.len() != frame.depth + 1 || last != (pc + 1 == frame.end) {
                        return Err(other_err!("invalid logical operand"));
                    }
                    if frame.state.advance(stack.pop().unwrap(), &mut rows, last)? {
                        let frame = logicals.pop().unwrap();
                        let RpnExpressionNode::LogicalEnd { field_type } = &self[frame.end - 1]
                        else {
                            unreachable!()
                        };
                        let (width, physical_value) = frame.state.finish(&mut rows);
                        stack.push(RpnStackNode::Vector {
                            value: RpnStackNodeVectorValue::Generated { physical_value },
                            field_type: field_type.into(),
                        });
                        active_count = width;
                        mode = frame.parent_mode;
                        pc = frame.end;
                    } else {
                        active_count = frame.state.pending_len();
                        pc += 1;
                    }
                    continue;
                }
                RpnExpressionNode::Repeat {
                    end,
                    supported_type,
                    field_type,
                } => {
                    if mode != InvocationMode::Scalar
                        || active_count != 1
                        || *end <= pc + 1
                        || *end > self.len()
                    {
                        return Err(other_err!("invalid repeat invocation"));
                    }
                    let count = stack
                        .pop()
                        .ok_or_else(|| other_err!("missing repeat count"))?;
                    let ScalarValueRef::Int(count) = count.get_logical_scalar_ref(0) else {
                        return Err(other_err!("repeat requires integer count"));
                    };
                    if count.is_none_or(|count| *count < 0) {
                        stack.push(integer_result(None, field_type));
                        pc = *end;
                        continue;
                    }
                    if !supported_type {
                        return Err(tidb_query_common::error::EvaluateError::Custom {
                            code: 1105,
                            msg: "VectorFloat32 is not supported for BENCHMARK()".into(),
                        }
                        .into());
                    }
                    let count = *count.unwrap() as u64;
                    if count == 0 {
                        stack.push(integer_result(Some(0), field_type));
                        pc = *end;
                        continue;
                    }
                    repeats.push(Repeat {
                        start: pc + 1,
                        end: *end,
                        remaining: count,
                        depth: stack.len(),
                        field_type,
                    });
                    pc += 1;
                    continue;
                }
                RpnExpressionNode::OrderedRows { end, field_type } => {
                    if *end <= pc || *end > self.len() {
                        return Err(other_err!("invalid ordered region"));
                    }
                    if mode == InvocationMode::Vector && active_count > 0 {
                        if rows.len() != active_count {
                            return Err(other_err!("ordered region requires input row selection"));
                        }
                        let selected = vec![rows[0]];
                        ordered.push(Ordered {
                            start: pc + 1,
                            end: *end,
                            rows: std::mem::replace(&mut rows, Cow::Owned(selected)),
                            mode,
                            values: Vec::with_capacity(active_count),
                            depth: stack.len(),
                            branch_depth: branches.len(),
                            field_type,
                        });
                        active_count = 1;
                        mode = InvocationMode::Scalar;
                    }
                    pc += 1;
                    continue;
                }
                RpnExpressionNode::RegexpArgument {
                    target,
                    count,
                    field_type,
                } => {
                    if mode != InvocationMode::Scalar
                        || stack.len() < *count
                        || *count == 0
                        || *target <= pc
                        || *target > self.len()
                    {
                        return Err(other_err!("invalid regexp argument guard"));
                    }
                    let ScalarValueRef::Bytes(value) =
                        stack.last().unwrap().get_logical_scalar_ref(0)
                    else {
                        return Err(other_err!("regexp argument requires bytes"));
                    };
                    match value {
                        None => {
                            stack.truncate(stack.len() - count);
                            stack.push(integer_result(None, field_type));
                            pc = *target;
                        }
                        Some(value) => {
                            std::str::from_utf8(value).map_err(|_| {
                                tidb_query_common::error::EvaluateError::Custom {
                                    code: 1105,
                                    msg: "invalid UTF-8 REGEXP_LIKE argument".into(),
                                }
                            })?;
                            pc += 1;
                        }
                    }
                    continue;
                }
                RpnExpressionNode::NullGuard { target, .. } => {
                    if *target <= pc || *target > self.len() {
                        return Err(other_err!("invalid NULL guard"));
                    }
                    let value = stack
                        .last()
                        .ok_or_else(|| other_err!("missing NULL guard operand"))?;
                    if mode == InvocationMode::Scalar && super::native_arithmetic::is_null(value, 0)
                    {
                        pc = *target;
                    } else {
                        pc += 1;
                    }
                    continue;
                }
                RpnExpressionNode::JumpIfNotNull { target, .. } => {
                    if mode != InvocationMode::Scalar || *target <= pc || *target > self.len() {
                        return Err(other_err!("invalid non-NULL jump"));
                    }
                    let value = stack
                        .last()
                        .ok_or_else(|| other_err!("missing non-NULL jump operand"))?;
                    if super::native_arithmetic::is_null(value, 0) {
                        stack.pop();
                        pc += 1;
                    } else {
                        pc = *target;
                    }
                    continue;
                }
                RpnExpressionNode::JumpIfFalse {
                    target,
                    end,
                    field_type,
                } => {
                    let condition = stack
                        .pop()
                        .ok_or_else(|| other_err!("missing IF condition"))?;
                    if *target <= pc || *target >= *end || *end > self.len() {
                        return Err(other_err!("invalid IF target"));
                    }
                    if let Some(ScalarValue::Int(v)) = condition.scalar_value() {
                        pc = if v.is_some_and(|v| v != 0) {
                            pc + 1
                        } else {
                            *target
                        };
                        continue;
                    }
                    if !matches!(condition, RpnStackNode::Vector { .. }) {
                        return Err(other_err!("IF requires integer condition"));
                    }
                    let mut then_positions = Vec::new();
                    let mut else_positions = Vec::new();
                    for i in 0..active_count {
                        let ScalarValueRef::Int(v) = condition.get_logical_scalar_ref(i) else {
                            return Err(other_err!("IF requires integer condition"));
                        };
                        if v.is_some_and(|v| *v != 0) {
                            then_positions.push(i);
                        } else {
                            else_positions.push(i);
                        }
                    }
                    if then_positions.is_empty() {
                        pc = *target;
                        continue;
                    }
                    if else_positions.is_empty() {
                        pc += 1;
                        continue;
                    }
                    if rows.len() != active_count {
                        return Err(other_err!("dynamic IF requires input row selection"));
                    }
                    let selected = then_positions.iter().map(|i| rows[*i]).collect::<Vec<_>>();
                    branches.push(Branch {
                        end: *end,
                        otherwise: *target,
                        rows: std::mem::replace(&mut rows, Cow::Owned(selected)),
                        then_positions,
                        else_positions,
                        then_value: None,
                        depth: stack.len(),
                        field_type,
                    });
                    active_count = rows.len();
                    pc += 1;
                    continue;
                }
                RpnExpressionNode::Jump { target, .. } => {
                    if *target <= pc || *target > self.len() {
                        return Err(other_err!("invalid jump target"));
                    }
                    pc = *target;
                    continue;
                }
                _ => {}
            }
            let borrowed = match &rows {
                Cow::Borrowed(rows) => Some(*rows),
                Cow::Owned(_) => None,
            };
            let (consumed_args, result) = Self::eval_one_node(
                ctx,
                schema,
                input_physical_columns,
                &rows,
                borrowed,
                active_count,
                node,
                &stack,
                seeded,
                regexps,
                pc,
            )?;
            stack.truncate(stack.len() - consumed_args);
            stack.push(result);
            pc += 1;
        }

        assert_eq!(stack.len(), 1);
        Ok(stack.into_iter().next().unwrap())
    }

    #[inline]
    fn eval_one_node<'a>(
        ctx: &mut EvalContext,
        schema: &'a [FieldType],
        input_physical_columns: &'a dyn InputColumns,
        input_logical_rows: &[usize],
        borrowed_rows: Option<&'a [usize]>,
        output_rows: usize,
        node: &'a RpnExpressionNode,
        stack: &[RpnStackNode<'a>],
        seeded_state: &mut SeededState,
        regexps: &mut std::collections::HashMap<usize, crate::impl_regexp::NativeRegexp>,
        node_index: usize,
    ) -> Result<(usize, RpnStackNode<'a>)> {
        match node {
            RpnExpressionNode::UserVariable {
                name,
                write,
                field_type,
            } => {
                let mut values =
                    VectorValue::with_capacity(output_rows, tidb_query_datatype::EvalType::Bytes);
                for row in 0..output_rows {
                    let value = if *write {
                        let ScalarValueRef::Bytes(value) = stack
                            .last()
                            .ok_or_else(|| other_err!("missing variable value"))?
                            .get_logical_scalar_ref(row)
                        else {
                            return Err(other_err!("variable value requires bytes"));
                        };
                        if let Some(value) = value {
                            input_physical_columns.set_string_variable(name, value)?;
                        }
                        value.map(|v| v.to_vec())
                    } else {
                        input_physical_columns.get_string_variable(name)?
                    };
                    values.push_bytes(value);
                }
                Ok((
                    usize::from(*write),
                    RpnStackNode::Vector {
                        value: RpnStackNodeVectorValue::Generated {
                            physical_value: values,
                        },
                        field_type: field_type.into(),
                    },
                ))
            }
            RpnExpressionNode::Rand {
                seeded,
                constant_seed,
                field_type,
            } => {
                let mut values = Vec::with_capacity(output_rows);
                for row in 0..output_rows {
                    let value = if *seeded {
                        let seed = match stack
                            .last()
                            .ok_or_else(|| other_err!("missing RAND seed"))?
                            .get_logical_scalar_ref(row)
                        {
                            ScalarValueRef::Int(seed) => seed.copied().unwrap_or(0),
                            _ => return Err(other_err!("RAND seed requires integer")),
                        };
                        if *constant_seed && seeded_state.enabled {
                            seeded_state
                                .values
                                .entry(node_index)
                                .or_insert_with(|| crate::impl_math::MySqlRng::new_with_seed(seed))
                                .gen()
                        } else {
                            crate::impl_math::MySqlRng::new_with_seed(seed).gen()
                        }
                    } else {
                        input_physical_columns.rand_next().ok_or_else(|| {
                            tidb_query_common::error::EvaluateError::Custom {
                                code: 1105,
                                msg: "RAND requires a session".into(),
                            }
                        })?
                    };
                    values.push(Some(value));
                }
                Ok((
                    usize::from(*seeded),
                    RpnStackNode::ExpressionVector {
                        value: ExpressionVector::Real(values),
                        field_type,
                    },
                ))
            }
            RpnExpressionNode::LogicalStart { .. }
            | RpnExpressionNode::LogicalNext
            | RpnExpressionNode::LogicalEnd { .. }
            | RpnExpressionNode::JumpIfFalse { .. }
            | RpnExpressionNode::Jump { .. }
            | RpnExpressionNode::RegexpArgument { .. }
            | RpnExpressionNode::NullGuard { .. }
            | RpnExpressionNode::JumpIfNotNull { .. }
            | RpnExpressionNode::OrderedRows { .. }
            | RpnExpressionNode::Repeat { .. }
            | RpnExpressionNode::RepeatEnd { .. } => {
                Err(other_err!("control edge outside RPN loop"))
            }
            RpnExpressionNode::NativeParameterDecimal { order, field_type } => Ok((
                0,
                super::native_arithmetic::parameter_decimal(
                    *order,
                    field_type,
                    input_physical_columns,
                )?,
            )),
            RpnExpressionNode::NativeRegexpLike {
                args_len,
                field_type,
            } => {
                let args = &stack[stack.len() - args_len..];
                let state = regexps.entry(node_index).or_default();
                let mut values =
                    VectorValue::with_capacity(output_rows, tidb_query_datatype::EvalType::Int);
                for row in 0..output_rows {
                    let args: Vec<_> = args
                        .iter()
                        .map(|arg| arg.get_logical_scalar_ref(row))
                        .collect();
                    values.push_int(state.evaluate(&args)?);
                }
                Ok((
                    *args_len,
                    RpnStackNode::Vector {
                        value: RpnStackNodeVectorValue::Generated {
                            physical_value: values,
                        },
                        field_type: field_type.into(),
                    },
                ))
            }
            RpnExpressionNode::NativeDate { field_type } => {
                let source = stack
                    .last()
                    .ok_or_else(|| other_err!("missing DATE operand"))?;
                let policy =
                    tidb_datatype::DateValuePolicy::native(input_physical_columns.date_modes());
                let mut values = Vec::with_capacity(output_rows);
                for row in 0..output_rows {
                    let value = match native_time(source, row)? {
                        None => None,
                        Some(time) => match time.date_value(policy) {
                            Some(value) => Some(value),
                            None => {
                                input_physical_columns.handle_truncate(&format!(
                                    "Incorrect datetime value: '{time}'"
                                ))?;
                                None
                            }
                        },
                    };
                    values.push(value);
                }
                Ok((
                    1,
                    RpnStackNode::ExpressionVector {
                        value: ExpressionVector::Time(values),
                        field_type,
                    },
                ))
            }
            RpnExpressionNode::NativeCastReal { field_type } => Ok((
                1,
                super::native_arithmetic::cast_real(
                    stack
                        .last()
                        .ok_or_else(|| other_err!("missing string cast operand"))?,
                    field_type,
                    output_rows,
                    input_physical_columns,
                )?,
            )),
            RpnExpressionNode::NativeArithmetic {
                op,
                overflow,
                diagnostic,
                field_type,
            } => {
                if stack.len() < 2 {
                    return Err(other_err!("missing native arithmetic operands"));
                }
                Ok((
                    2,
                    super::native_arithmetic::evaluate(
                        *op,
                        field_type,
                        overflow,
                        diagnostic,
                        &stack[stack.len() - 2..],
                        output_rows,
                        input_physical_columns,
                    )?,
                ))
            }
            RpnExpressionNode::ExpressionConstant { value, field_type } => Ok((
                0,
                RpnStackNode::ExpressionScalar {
                    value: Cow::Borrowed(value),
                    field_type,
                },
            )),
            RpnExpressionNode::Constant { value, field_type } => {
                Ok((0, RpnStackNode::Scalar { value, field_type }))
            }
            RpnExpressionNode::ColumnRef { offset } => {
                assert_eq!(input_logical_rows.len(), output_rows);
                Ok((
                    0,
                    match borrowed_rows {
                        Some(rows) => input_physical_columns.read(schema, *offset, rows, ctx)?,
                        None => input_physical_columns.read_selected(
                            schema,
                            *offset,
                            input_logical_rows,
                            ctx,
                        )?,
                    },
                ))
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
                if stack_slice.iter().any(|arg| {
                    matches!(
                        arg,
                        RpnStackNode::ExpressionScalar { .. }
                            | RpnStackNode::ExpressionVector { .. }
                    )
                }) && !metadata.is::<super::native_arithmetic::NativeComparisonArgs>()
                    && !metadata.is::<crate::impl_time::CalendarPart>()
                {
                    return Err(other_err!(
                        "kernel {} does not yet admit native scalar arguments",
                        func_meta.name
                    ));
                }
                let mut call_extra = RpnFnCallExtra { ret_field_type };
                let prior_warnings = metadata
                    .is::<NativeKernelWarnings>()
                    .then(|| std::mem::replace(&mut ctx.warnings, EvalWarnings::without_limit()));
                let ret =
                    (func_meta.fn_ptr)(ctx, output_rows, stack_slice, &mut call_extra, &**metadata);
                if let Some(prior_warnings) = prior_warnings {
                    let warnings = std::mem::replace(&mut ctx.warnings, prior_warnings);
                    for warning in &warnings.warnings {
                        input_physical_columns.append_kernel_warning(warning);
                    }
                }
                let ret = ret?;
                Ok((
                    *args_len,
                    RpnStackNode::Vector {
                        value: RpnStackNodeVectorValue::Generated {
                            physical_value: ret,
                        },
                        field_type: ret_field_type.into(),
                    },
                ))
            }
        }
    }
}

fn integer_result(value: Option<i64>, field_type: &FieldType) -> RpnStackNode<'_> {
    RpnStackNode::Vector {
        value: RpnStackNodeVectorValue::Generated {
            physical_value: VectorValue::Int(ChunkedVecSized::from(vec![value])),
        },
        field_type: field_type.into(),
    }
}

fn merge_branch_values<'a>(
    then_value: RpnStackNode<'a>,
    else_value: RpnStackNode<'a>,
    then_positions: &[usize],
    else_positions: &[usize],
    count: usize,
    field_type: &'a FieldType,
) -> Result<RpnStackNode<'a>> {
    merge_selected_values(
        [(&then_value, then_positions), (&else_value, else_positions)],
        count,
        field_type,
    )
}

/// Build a typed calendar extract that reads the native Time carrier as-is.
pub fn native_calendar_part_node(part: crate::impl_time::CalendarPart) -> RpnExpressionNode {
    use crate::impl_time::CalendarPart;

    RpnExpressionNode::FnCall {
        func_meta: super::RpnFnMeta {
            name: "native_calendar_part",
            fn_ptr: |_, rows, args, _, metadata| {
                let [source] = args else {
                    return Err(other_err!("calendar part requires one time operand"));
                };
                let part = metadata
                    .downcast_ref::<CalendarPart>()
                    .ok_or_else(|| other_err!("missing calendar part"))?;
                let mut values = Vec::with_capacity(rows);
                for row in 0..rows {
                    values
                        .push(native_time(source, row)?.map(|time| part.extract(time.core_time())));
                }
                Ok(VectorValue::Int(ChunkedVecSized::from(values)))
            },
            ..crate::impl_time::month_fn_meta()
        },
        args_len: 1,
        field_type: tidb_query_datatype::FieldTypeTp::LongLong.into(),
        metadata: Box::new(part),
    }
}

fn native_time(source: &RpnStackNode<'_>, row: usize) -> Result<Option<tidb_datatype::Time>> {
    match source {
        RpnStackNode::ExpressionScalar { value, .. } => match value.as_ref() {
            ExpressionScalar::Time(value) => Ok(*value),
            _ => Err(other_err!("native time result mismatch")),
        },
        RpnStackNode::ExpressionVector {
            value: ExpressionVector::Time(values),
            ..
        } => Ok(values[row]),
        _ => Err(other_err!("native time result mismatch")),
    }
}

fn merge_selected_values<'a, 'b>(
    inputs: impl IntoIterator<Item = (&'b RpnStackNode<'a>, &'b [usize])>,
    count: usize,
    field_type: &'a FieldType,
) -> Result<RpnStackNode<'a>>
where
    'a: 'b,
{
    use tidb_query_datatype::{EvalType, FieldTypeAccessor};
    match EvalType::try_from(field_type.as_accessor().tp()).map_err(|e| other_err!("{}", e))? {
        EvalType::Int => {
            let mut values = vec![None; count];
            for (source, positions) in inputs {
                for (i, out) in positions.iter().enumerate() {
                    let ScalarValueRef::Int(value) = source.get_logical_scalar_ref(i) else {
                        return Err(other_err!("IF integer result mismatch"));
                    };
                    values[*out] = value.copied();
                }
            }
            Ok(RpnStackNode::Vector {
                value: RpnStackNodeVectorValue::Generated {
                    physical_value: VectorValue::Int(ChunkedVecSized::from(values)),
                },
                field_type: field_type.into(),
            })
        }
        EvalType::Bytes => {
            let mut values = vec![None; count];
            for (source, positions) in inputs {
                for (i, out) in positions.iter().enumerate() {
                    let ScalarValueRef::Bytes(value) = source.get_logical_scalar_ref(i) else {
                        return Err(other_err!("byte result mismatch"));
                    };
                    values[*out] = value.map(|v| v.to_vec());
                }
            }
            let mut result = VectorValue::with_capacity(count, EvalType::Bytes);
            for value in values {
                result.push_bytes(value);
            }
            Ok(RpnStackNode::Vector {
                value: RpnStackNodeVectorValue::Generated {
                    physical_value: result,
                },
                field_type: field_type.into(),
            })
        }
        EvalType::Real => {
            let mut values = vec![None; count];
            for (source, positions) in inputs {
                for (i, out) in positions.iter().enumerate() {
                    values[*out] = super::native_arithmetic::real(source, i)?;
                }
            }
            Ok(RpnStackNode::ExpressionVector {
                value: ExpressionVector::Real(values),
                field_type,
            })
        }
        EvalType::DateTime => {
            let mut values = vec![None; count];
            for (source, positions) in inputs {
                for (i, out) in positions.iter().enumerate() {
                    values[*out] = native_time(source, i)?;
                }
            }
            Ok(RpnStackNode::ExpressionVector {
                value: ExpressionVector::Time(values),
                field_type,
            })
        }
        EvalType::Decimal => {
            let mut values = vec![None; count];
            for (source, positions) in inputs {
                for (i, out) in positions.iter().enumerate() {
                    values[*out] = match source {
                        RpnStackNode::ExpressionScalar { value, .. } => match value.as_ref() {
                            ExpressionScalar::Decimal(value) => value.clone(),
                            _ => return Err(other_err!("IF decimal result mismatch")),
                        },
                        RpnStackNode::ExpressionVector {
                            value: ExpressionVector::Decimal(values),
                            ..
                        } => values[i].clone(),
                        _ => return Err(other_err!("IF decimal result mismatch")),
                    };
                }
            }
            Ok(RpnStackNode::ExpressionVector {
                value: ExpressionVector::Decimal(values),
                field_type,
            })
        }
        EvalType::VectorFloat32 => {
            let mut values = vec![None; count];
            for (source, positions) in inputs {
                for (i, out) in positions.iter().enumerate() {
                    let ScalarValueRef::VectorFloat32(value) = source.get_logical_scalar_ref(i)
                    else {
                        return Err(other_err!("vector result mismatch"));
                    };
                    values[*out] = value.map(|value| value.to_owned());
                }
            }
            Ok(RpnStackNode::Vector {
                value: RpnStackNodeVectorValue::Generated {
                    physical_value: VectorValue::VectorFloat32(ChunkedVecVectorFloat32::from_vec(
                        values,
                    )),
                },
                field_type: field_type.into(),
            })
        }
        _ => Err(other_err!("IF result family is not admitted")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use std::{
        borrow::Cow,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
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

    static SHORT_CIRCUIT_RHS_EVAL_COUNT: AtomicUsize = AtomicUsize::new(0);

    fn short_circuit_context() -> EvalContext {
        EvalContext::new(Arc::new(EvalConfig::from_flag(
            Flag::ENABLE_SHORT_CIRCUIT_EXPRESSION,
        )))
    }

    #[test]
    fn test_public_logical_input_preserves_vector_declaration() {
        use tidb_query_datatype::{FieldTypeFlag, codec::mysql::Duration};

        struct Input {
            columns: LazyBatchColumnVec,
            hour_type: FieldType,
            reads: std::cell::RefCell<Vec<(usize, Vec<usize>)>>,
        }
        impl InputColumns for Input {
            fn read<'a>(
                &'a self,
                schema: &'a [FieldType],
                offset: usize,
                rows: &'a [usize],
                ctx: &mut EvalContext,
            ) -> InputResult<'a> {
                self.reads.borrow_mut().push((offset, rows.to_vec()));
                if offset == 0 {
                    return self.columns.read(schema, offset, rows, ctx);
                }
                Ok(RpnStackNode::Vector {
                    value: RpnStackNodeVectorValue::Ref {
                        physical_value: self.columns[offset].decoded(),
                        logical_rows: rows,
                    },
                    field_type: (&self.hour_type).into(),
                })
            }
        }

        let integer: FieldType = FieldTypeTp::LongLong.into();
        let mut unsigned = integer.clone();
        unsigned.as_mut_accessor().set_flag(FieldTypeFlag::UNSIGNED);
        let schema = [integer.clone(), unsigned.clone()];
        let mut duration: FieldType = FieldTypeTp::Duration.into();
        duration.set_decimal(0);
        let make_time = ExprDefBuilder::scalar_func(ScalarFuncSig::MakeTime, duration)
            .push_child(ExprDefBuilder::column_ref(1, unsigned.clone()))
            .push_child(ExprDefBuilder::constant_int(0))
            .push_child(ExprDefBuilder::constant_real(0.0));
        let rhs = ExprDefBuilder::scalar_func(ScalarFuncSig::LtDuration, integer.clone())
            .push_child(make_time)
            .push_child(ExprDefBuilder::constant_duration(Duration::zero()));
        let expression = ExprDefBuilder::scalar_func(ScalarFuncSig::LogicalOr, integer.clone())
            .push_child(ExprDefBuilder::column_ref(0, integer.clone()))
            .push_child(rhs)
            .build();
        let mut ctx = short_circuit_context();
        let program = RpnExpressionBuilder::build_from_expr_tree(expression, &mut ctx, 2).unwrap();
        let rows = [2, 0, 1, 2];
        let columns = LazyBatchColumnVec::from(vec![
            VectorValue::Int(vec![Some(1), Some(0), None].into()),
            VectorValue::Int(vec![Some(-1), Some(-1), Some(-1)].into()),
        ]);
        let mut signed = integer;
        signed.set_flen(19);
        let mut observations = Vec::new();
        for (hour_type, expected) in [
            (signed, [Some(1), Some(1), Some(1), Some(1)]),
            (unsigned, [None, Some(1), Some(0), None]),
        ] {
            let input = Input {
                columns: columns.clone(),
                hour_type,
                reads: Default::default(),
            };
            let selected = input
                .read_selected(&schema, 1, &[2, 1, 2], &mut ctx)
                .unwrap();
            assert_eq!(selected.field_type(), &input.hour_type);
            let selected_vector = selected.vector_value().unwrap();
            assert_eq!(selected_vector.logical_rows(), &[0, 1, 2]);
            assert_eq!(selected_vector.as_ref().to_int_vec(), &[Some(-1); 3]);
            input.reads.borrow_mut().clear();
            let result = program
                .eval_input(&mut ctx, &schema, &input, &rows, rows.len())
                .unwrap();
            assert!(result.is_vector());
            let values = result.vector_value().unwrap().as_ref().to_int_vec();
            let reads = input.reads.borrow();
            eprintln!(
                "unsigned={}, reads={reads:?}, values={values:?}",
                input.hour_type.is_unsigned()
            );
            assert_eq!(*reads, [(0, rows.to_vec()), (1, vec![2, 1, 2])]);
            observations.push((values.to_vec(), expected));
        }
        let selected = columns
            .read_selected(&schema, 1, &[2, 1, 2], &mut ctx)
            .unwrap();
        assert!(std::ptr::eq(selected.field_type(), &schema[1]));
        let stored = program
            .eval_decoded(&mut ctx, &schema, &columns, &rows, rows.len())
            .unwrap();
        assert_eq!(
            stored.vector_value().unwrap().as_ref().to_int_vec(),
            &[None, Some(1), Some(0), None],
        );
        for (actual, expected) in observations {
            assert_eq!(actual, expected);
        }
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

    fn literal_regexp_program(pattern: &[u8], flags: Option<&[u8]>) -> RpnExpression {
        let integer: FieldType = FieldTypeTp::LongLong.into();
        let end = if flags.is_some() { 8 } else { 6 };
        let mut nodes = vec![
            RpnExpressionNode::OrderedRows {
                end,
                field_type: integer.clone(),
            },
            RpnExpressionNode::ColumnRef { offset: 0 },
            RpnExpressionNode::RegexpArgument {
                target: end,
                count: 1,
                field_type: integer.clone(),
            },
            RpnExpressionNode::Constant {
                value: ScalarValue::Bytes(Some(pattern.to_vec())),
                field_type: FieldTypeTp::VarString.into(),
            },
            RpnExpressionNode::RegexpArgument {
                target: end,
                count: 2,
                field_type: integer.clone(),
            },
        ];
        if let Some(flags) = flags {
            nodes.push(RpnExpressionNode::Constant {
                value: ScalarValue::Bytes(Some(flags.to_vec())),
                field_type: FieldTypeTp::VarString.into(),
            });
            nodes.push(RpnExpressionNode::RegexpArgument {
                target: end,
                count: 3,
                field_type: integer.clone(),
            });
        }
        nodes.push(RpnExpressionNode::NativeRegexpLike {
            args_len: if flags.is_some() { 3 } else { 2 },
            field_type: integer.clone(),
        });
        RpnExpression::finish(nodes, &[FieldTypeTp::VarString.into()], integer).unwrap()
    }

    struct RegexpInput(ScalarValue, std::cell::Cell<usize>);
    impl InputColumns for RegexpInput {
        fn read<'a>(
            &'a self,
            schema: &'a [FieldType],
            _: usize,
            rows: &'a [usize],
            _: &mut EvalContext,
        ) -> InputResult<'a> {
            self.1.set(self.1.get() + 1);
            let mut values =
                VectorValue::with_capacity(rows.len(), tidb_query_datatype::EvalType::Bytes);
            for _ in rows {
                values.push_bytes(self.0.as_scalar_value_ref().as_bytes().map(|v| v.to_vec()));
            }
            Ok(RpnStackNode::Vector {
                value: RpnStackNodeVectorValue::Generated {
                    physical_value: values,
                },
                field_type: (&schema[0]).into(),
            })
        }
    }

    #[test]
    fn test_execution_literal_regexp_prepares_once_across_batches() {
        let program = literal_regexp_program(b"^a", Some(b"i"));
        let schema = [FieldTypeTp::VarString.into()];
        let input = RegexpInput(
            ScalarValue::Bytes(Some(b"Abc".to_vec())),
            Default::default(),
        );
        crate::impl_regexp::NATIVE_PREPARATIONS.with(|count| count.set(0));
        // Regex preparation is independent of seeded RAND capabilities.
        let mut first = RpnExecution::new(&program, false);
        let mut second = RpnExecution::new(&program, false);
        assert!(first.state.regexps.is_empty() && second.state.regexps.is_empty());
        for _ in 0..2 {
            for execution in [&mut first, &mut second] {
                let value = execution
                    .eval_input_mode(
                        &mut EvalContext::default(),
                        &schema,
                        &input,
                        &[2, 0, 2],
                        3,
                        InvocationMode::Vector,
                    )
                    .unwrap();
                for row in 0..3 {
                    assert_eq!(value.get_logical_scalar_ref(row).as_int(), Some(&1));
                }
                assert_eq!(execution.state.regexps.len(), 1);
            }
        }
        assert_eq!(
            crate::impl_regexp::NATIVE_PREPARATIONS.with(|count| count.get()),
            2
        );
        assert_eq!(input.1.get(), 12, "a hit still demands the subject");
        let fresh = RpnExecution::new(&program, false);
        assert!(fresh.state.regexps.is_empty());
        println!(
            "RpnExecution static bytes={}",
            std::mem::size_of::<RpnExecution<'_>>()
        );
    }

    #[test]
    fn test_owned_execution_resets_literal_regexp_state() {
        let program = literal_regexp_program(b"^a", Some(b"i"));
        let mut execution = OwnedRpnExecution::new(program, false);
        let schema = [FieldTypeTp::VarString.into()];
        let input = RegexpInput(
            ScalarValue::Bytes(Some(b"Abc".to_vec())),
            Default::default(),
        );
        crate::impl_regexp::NATIVE_PREPARATIONS.with(|count| count.set(0));
        for opens in 1..=2 {
            execution.reset(false);
            assert!(execution.state.regexps.is_empty());
            for _ in 0..2 {
                let root = (
                    execution.borrow_execution(),
                    &schema[..],
                    &input as &dyn InputColumns,
                );
                let output = RpnExecutionRef::eval_group(
                    &mut [root],
                    &mut EvalContext::default(),
                    &[2, 0, 2],
                    OutputOrder::Columns,
                    InvocationMode::Vector,
                )
                .unwrap();
                for row in 0..3 {
                    assert_eq!(output[0].get_logical_scalar_ref(row).as_int(), Some(&1));
                }
            }
            assert_eq!(
                crate::impl_regexp::NATIVE_PREPARATIONS.with(|count| count.get()),
                opens
            );
        }
    }

    #[test]
    fn test_execution_literal_regexp_error_replay_and_guards() {
        let schema = [FieldTypeTp::VarString.into()];
        for (pattern, flags, expected) in [
            (&b"["[..], None, "invalid regular expression pattern"),
            (&b""[..], None, "empty regular expression pattern"),
            (&b"a"[..], Some(&b"n"[..]), "Invalid match type"),
        ] {
            let program = literal_regexp_program(pattern, flags);
            let mut execution = RpnExecution::new(&program, false);
            crate::impl_regexp::NATIVE_PREPARATIONS.with(|count| count.set(0));
            for mode in [InvocationMode::Scalar, InvocationMode::Vector] {
                for value in [
                    None,
                    Some(vec![0xff]),
                    Some(b"abc".to_vec()),
                    None,
                    Some(vec![0xff]),
                    Some(b"abc".to_vec()),
                ] {
                    let input = RegexpInput(ScalarValue::Bytes(value.clone()), Default::default());
                    let result = execution.eval_input_mode(
                        &mut EvalContext::default(),
                        &schema,
                        &input,
                        &[0],
                        1,
                        mode,
                    );
                    if value.is_none() {
                        assert!(result.unwrap().get_logical_scalar_ref(0).as_int().is_none());
                    } else {
                        let error = result.unwrap_err();
                        let tidb_query_common::error::ErrorInner::Evaluate(
                            tidb_query_common::error::EvaluateError::Custom { code, msg },
                        ) = error.0.as_ref()
                        else {
                            panic!("{error:?}")
                        };
                        assert_eq!(*code, 1105);
                        assert_eq!(
                            msg,
                            if value == Some(vec![0xff]) {
                                "invalid UTF-8 REGEXP_LIKE argument"
                            } else {
                                expected
                            }
                        );
                    }
                }
            }
            assert_eq!(
                crate::impl_regexp::NATIVE_PREPARATIONS.with(|count| count.get()),
                1
            );
            assert_eq!(execution.state.regexps.len(), 1);
        }
        // Invalid argument encoding never reaches preparation.
        let program = literal_regexp_program(&[0xff], None);
        let mut execution = RpnExecution::new(&program, false);
        let input = RegexpInput(
            ScalarValue::Bytes(Some(b"abc".to_vec())),
            Default::default(),
        );
        assert!(
            execution
                .eval_input_mode(
                    &mut EvalContext::default(),
                    &schema,
                    &input,
                    &[0],
                    1,
                    InvocationMode::Scalar
                )
                .is_err()
        );
        assert!(execution.state.regexps.is_empty());
    }

    #[test]
    fn test_execution_literal_regexp_dead_branch_has_no_state() {
        let integer: FieldType = FieldTypeTp::LongLong.into();
        let mut nodes = vec![
            RpnExpressionNode::Constant {
                value: ScalarValue::Int(Some(0)),
                field_type: integer.clone(),
            },
            RpnExpressionNode::JumpIfFalse {
                target: 9,
                end: 10,
                field_type: integer.clone(),
            },
        ];
        let mut body = literal_regexp_program(b"[", None).into_inner();
        for node in &mut body {
            match node {
                RpnExpressionNode::OrderedRows { end, .. } => *end += 2,
                RpnExpressionNode::RegexpArgument { target, .. } => *target += 2,
                _ => (),
            }
        }
        nodes.extend(body);
        nodes.push(RpnExpressionNode::Jump {
            target: 10,
            field_type: integer.clone(),
        });
        nodes.push(RpnExpressionNode::Constant {
            value: ScalarValue::Int(Some(7)),
            field_type: integer.clone(),
        });
        let schema = [FieldTypeTp::VarString.into()];
        let program = RpnExpression::finish(nodes, &schema, integer).unwrap();
        let input = RegexpInput(
            ScalarValue::Bytes(Some(b"abc".to_vec())),
            Default::default(),
        );
        let mut execution = RpnExecution::new(&program, false);
        crate::impl_regexp::NATIVE_PREPARATIONS.with(|count| count.set(0));
        for _ in 0..2 {
            assert_eq!(
                execution
                    .eval_input_mode(
                        &mut EvalContext::default(),
                        &schema,
                        &input,
                        &[2, 0, 2],
                        3,
                        InvocationMode::Vector
                    )
                    .unwrap()
                    .get_logical_scalar_ref(0)
                    .as_int(),
                Some(&7)
            );
        }
        assert!(execution.state.regexps.is_empty());
        assert_eq!(input.1.get(), 0);
        assert_eq!(
            crate::impl_regexp::NATIVE_PREPARATIONS.with(|count| count.get()),
            0
        );
    }

    #[test]
    fn test_execution_literal_regexp_repeat_and_node_isolation() {
        let integer: FieldType = FieldTypeTp::LongLong.into();
        let mut nodes = vec![
            RpnExpressionNode::Constant {
                value: ScalarValue::Int(Some(3)),
                field_type: integer.clone(),
            },
            RpnExpressionNode::Repeat {
                end: 16,
                supported_type: true,
                field_type: integer.clone(),
            },
        ];
        // Two occurrences in one root. A wrapping addition consumes both results.
        for start in [2, 8] {
            let mut occurrence = literal_regexp_program(b"a", None).into_inner();
            for node in &mut occurrence {
                match node {
                    RpnExpressionNode::OrderedRows { end, .. } => *end += start,
                    RpnExpressionNode::RegexpArgument { target, .. } => *target += start,
                    _ => (),
                }
            }
            nodes.extend(occurrence);
        }
        nodes.push(RpnExpressionNode::FnCall {
            func_meta: arithmetic_fn_meta::<IntIntPlus>(),
            args_len: 2,
            field_type: integer.clone(),
            metadata: Box::new(()),
        });
        nodes.push(RpnExpressionNode::RepeatEnd {
            field_type: integer.clone(),
        });
        let schema = [FieldTypeTp::VarString.into()];
        let program = RpnExpression::finish(nodes, &schema, integer).unwrap();
        let input = RegexpInput(
            ScalarValue::Bytes(Some(b"abc".to_vec())),
            Default::default(),
        );
        let mut execution = RpnExecution::new(&program, false);
        crate::impl_regexp::NATIVE_PREPARATIONS.with(|count| count.set(0));
        for _ in 0..2 {
            let result = execution
                .eval_input_mode(
                    &mut EvalContext::default(),
                    &schema,
                    &input,
                    &[0],
                    1,
                    InvocationMode::Scalar,
                )
                .unwrap();
            assert_eq!(result.get_logical_scalar_ref(0).as_int(), Some(&0));
        }
        assert_eq!(input.1.get(), 12);
        assert_eq!(execution.state.regexps.len(), 2);
        assert_eq!(
            crate::impl_regexp::NATIVE_PREPARATIONS.with(|count| count.get()),
            2
        );
    }

    #[test]
    fn test_execution_seeded_repeat_state_is_local_and_continues() {
        let integer = tidb_query_datatype::builder::FieldTypeBuilder::new()
            .tp(FieldTypeTp::LongLong)
            .build();
        let real = tidb_query_datatype::builder::FieldTypeBuilder::new()
            .tp(FieldTypeTp::Double)
            .build();
        let literal = |value| RpnExpressionNode::Constant {
            value: ScalarValue::Int(Some(value)),
            field_type: integer.clone(),
        };
        let program = RpnExpression::finish(
            vec![
                literal(3),
                RpnExpressionNode::Repeat {
                    end: 5,
                    supported_type: true,
                    field_type: integer.clone(),
                },
                literal(7),
                RpnExpressionNode::Rand {
                    seeded: true,
                    constant_seed: true,
                    field_type: real,
                },
                RpnExpressionNode::RepeatEnd {
                    field_type: integer.clone(),
                },
            ],
            &[],
            integer,
        )
        .unwrap();
        let input = LazyBatchColumnVec::empty();
        let mut execution = RpnExecution::new(&program, true);
        let mut expected = crate::impl_math::MySqlRng::new_with_seed(7);
        for _ in 0..2 {
            execution
                .eval_input_mode(
                    &mut EvalContext::default(),
                    &[],
                    &input,
                    &[0],
                    1,
                    InvocationMode::Scalar,
                )
                .unwrap();
            for _ in 0..3 {
                expected.gen();
            }
            // Inspect state without consuming it; the body output is discarded.
            assert_eq!(execution.state.seeded.values.len(), 1);
        }
        assert_eq!(
            execution.state.seeded.values.get_mut(&3).unwrap().gen(),
            expected.gen()
        );
        let fresh = RpnExecution::new(&program, true);
        assert!(fresh.state.seeded.values.is_empty());
        let mut unsupported = RpnExecution::new(&program, false);
        unsupported
            .eval_input_mode(
                &mut EvalContext::default(),
                &[],
                &input,
                &[0],
                1,
                InvocationMode::Scalar,
            )
            .unwrap();
        assert!(unsupported.state.seeded.values.is_empty());
    }

    #[test]
    fn test_pb_if_remains_eager_with_short_circuit_flag() {
        for flag in [Flag::empty(), Flag::ENABLE_SHORT_CIRCUIT_EXPRESSION] {
            let expression =
                ExprDefBuilder::scalar_func(ScalarFuncSig::IfInt, FieldTypeTp::LongLong)
                    .push_child(ExprDefBuilder::constant_int(1))
                    .push_child(ExprDefBuilder::constant_int(7))
                    .push_child(
                        ExprDefBuilder::scalar_func(
                            ScalarFuncSig::UnaryMinusInt,
                            FieldTypeTp::LongLong,
                        )
                        .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong)),
                    )
                    .build();
            let mut context = EvalContext::new(Arc::new(EvalConfig::from_flag(flag)));
            let program =
                RpnExpressionBuilder::build_from_expr_tree(expression, &mut context, 1).unwrap();
            let mut column = LazyBatchColumn::decoded_with_capacity_and_tp(2, EvalType::Int);
            column.mut_decoded().push_int(Some(i64::MIN));
            column.mut_decoded().push_int(Some(0));
            let mut columns = LazyBatchColumnVec::from(vec![column]);
            assert!(
                program
                    .eval(
                        &mut context,
                        &[FieldTypeTp::LongLong.into()],
                        &mut columns,
                        &[0, 1],
                        2
                    )
                    .is_err()
            );
        }
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
    fn test_logical_short_circuit_skips_rhs_rows() {
        use tipb::{Expr, ScalarFuncSig};

        fn fn_mapper(expr: &Expr) -> Result<RpnFnMeta> {
            if expr.get_sig() == ScalarFuncSig::CastIntAsInt {
                return Ok(short_circuit_counted_identity_fn_meta());
            }
            crate::map_expr_node_to_rpn_func(expr)
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

        fn fn_mapper(expr: &Expr) -> Result<RpnFnMeta> {
            if expr.get_sig() == ScalarFuncSig::CastIntAsInt {
                return Ok(short_circuit_unreachable_fn_meta());
            }
            crate::map_expr_node_to_rpn_func(expr)
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
        let columns = LazyBatchColumnVec::from(vec![
            VectorValue::Int(vec![Some(1), Some(0), None, Some(0)].into()),
            VectorValue::Int(vec![Some(0), Some(0), Some(0), None].into()),
            VectorValue::Int(vec![Some(1), Some(1), Some(0), Some(1)].into()),
        ]);
        let schema = &[
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
            FieldTypeTp::LongLong.into(),
        ];
        struct ObservedInput {
            columns: LazyBatchColumnVec,
            reads: std::cell::RefCell<Vec<(usize, Vec<usize>)>>,
        }
        impl InputColumns for ObservedInput {
            fn read<'a>(
                &'a self,
                schema: &'a [FieldType],
                offset: usize,
                rows: &'a [usize],
                ctx: &mut EvalContext,
            ) -> InputResult<'a> {
                self.reads.borrow_mut().push((offset, rows.to_vec()));
                self.columns.read(schema, offset, rows, ctx)
            }
        }
        let input = ObservedInput {
            columns,
            reads: Default::default(),
        };
        let rows = [3, 1, 2, 0, 3, 1];
        EVALUATOR_ENTRIES.with(|entries| entries.set(0));
        let result = exp
            .eval_input(&mut ctx, schema, &input, &rows, rows.len())
            .unwrap();
        let entries = EVALUATOR_ENTRIES.with(|entries| entries.get());
        let values = result.vector_value().unwrap().as_ref().to_int_vec();
        eprintln!(
            "entries={entries}, reads={:?}, values={values:?}",
            input.reads.borrow()
        );
        assert_eq!(
            *input.reads.borrow(),
            [
                (0, rows.to_vec()),
                (1, vec![3, 1, 2, 3, 1]),
                (2, vec![3, 2, 0, 3]),
            ]
        );
        assert_eq!(values, &[None, Some(0), Some(0), Some(1), None, Some(0)]);
        assert_eq!(entries, 1, "one evaluator entry per root");
    }

    #[test]
    fn test_public_logical_input_broadcasts_scalar_after_compaction() {
        struct Input {
            left: LazyBatchColumnVec,
            right: ScalarValue,
            right_type: FieldType,
            reads: std::cell::RefCell<Vec<(usize, Vec<usize>)>>,
        }
        impl InputColumns for Input {
            fn read<'a>(
                &'a self,
                schema: &'a [FieldType],
                offset: usize,
                rows: &'a [usize],
                ctx: &mut EvalContext,
            ) -> InputResult<'a> {
                self.reads.borrow_mut().push((offset, rows.to_vec()));
                if offset == 0 {
                    self.left.read(schema, offset, rows, ctx)
                } else {
                    Ok(RpnStackNode::Scalar {
                        value: &self.right,
                        field_type: &self.right_type,
                    })
                }
            }
        }
        let expression =
            ExprDefBuilder::scalar_func(ScalarFuncSig::LogicalOr, FieldTypeTp::LongLong)
                .push_child(
                    ExprDefBuilder::scalar_func(ScalarFuncSig::CastIntAsInt, FieldTypeTp::LongLong)
                        .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong)),
                )
                .push_child(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong))
                .build();
        let mut ctx = short_circuit_context();
        let program = RpnExpressionBuilder::build_from_expr_tree(expression, &mut ctx, 2).unwrap();
        let schema: [FieldType; 2] = [FieldTypeTp::LongLong.into(), FieldTypeTp::LongLong.into()];
        let mut right_type = schema[1].clone();
        right_type.set_flen(19);
        let mut observations = Vec::new();
        for (right, expected) in [
            (Some(1), [Some(1), Some(1), Some(1), Some(1)]),
            (Some(0), [None, Some(1), Some(0), None]),
            (None, [None, Some(1), None, None]),
        ] {
            let input = Input {
                left: LazyBatchColumnVec::from(vec![VectorValue::Int(
                    vec![Some(1), Some(0), None].into(),
                )]),
                right: ScalarValue::Int(right),
                right_type: right_type.clone(),
                reads: Default::default(),
            };
            let scalar = input
                .read_selected(&schema, 1, &[2, 1, 2], &mut ctx)
                .unwrap();
            assert!(scalar.is_scalar());
            assert_eq!(scalar.scalar_value().unwrap().as_int().copied(), right);
            assert_eq!(scalar.field_type(), &right_type);
            drop(scalar);
            input.reads.borrow_mut().clear();
            let result = program
                .eval_input(&mut ctx, &schema, &input, &[2, 0, 1, 2], 4)
                .map(|result| {
                    result
                        .vector_value()
                        .unwrap()
                        .as_ref()
                        .to_int_vec()
                        .to_vec()
                });
            let reads = input.reads.into_inner();
            eprintln!("rhs={right:?}, reads={reads:?}, result={result:?}");
            observations.push((reads, result, expected));
        }
        for (reads, result, expected) in observations {
            assert_eq!(reads, [(0, vec![2, 0, 1, 2]), (1, vec![2, 1, 2])]);
            assert_eq!(result.unwrap(), expected);
        }
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
        let field_type: FieldType = FieldTypeTp::Double.into();
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
            field_type: (&field_type).into(),
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
            field_type: (&field_type).into(),
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
    use std::borrow::Cow;

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
