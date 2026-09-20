// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Copying, in-process embedding of the existing TiKV RPN engine.
//!
//! The wire boundary uses serialized tipb messages, not protobuf Rust types, so
//! callers using prost can embed this rust-protobuf engine. Every engine eval
//! type, including the input-only `Set`, and the existing builder's scalar
//! signatures are available. This preserves TiKV's eager RPN semantics,
//! including conditional children. It does not run a server, perform storage
//! IO, or implement SQL kernels. Compilation builds RPN once. Evaluation copies
//! selected rows into decoded TiKV columns and copies results back, splitting
//! batches at the engine's batch limit.

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, OnceLock},
};

use codec::prelude::NumberDecoder;
use tidb_query_common::error::ErrorInner;
pub use tidb_query_datatype::codec::data_type::{
    DateTime, Decimal, Duration, Enum, Json, Set, VectorFloat32,
};
use tidb_query_datatype::{
    EvalType, FieldTypeTp,
    codec::{
        batch::LazyBatchColumnVec,
        data_type::{ChunkedVec, Real, ScalarValueRef, VectorFloat32Ref, VectorValue},
    },
    expr::{EvalConfig, EvalContext, Flag, SqlMode},
};
use tipb::{Expr, ExprType, FieldType, ScalarFuncSig};

use crate::{BATCH_MAX_SIZE, RpnExpression, RpnExpressionBuilder, RpnExpressionNode};
mod safety;
mod values;
pub use values::{
    date_time_from_chunk, date_time_to_chunk, decimal_from_chunk, decimal_to_chunk,
    json_from_binary, json_to_binary,
};

/// Resolve a name in the engine's tipb enum, not a promise of RPN support.
/// Compile the complete expression to check its signature, types and metadata.
pub fn scalar_function_signature(name: &str) -> Option<i32> {
    use protobuf::ProtobufEnum;
    static SIGNATURES: OnceLock<HashMap<String, i32>> = OnceLock::new();
    SIGNATURES
        .get_or_init(|| {
            ScalarFuncSig::values()
                .iter()
                .map(|sig| (format!("{sig:?}"), sig.value()))
                .collect()
        })
        .get(name)
        .copied()
}

/// Owned nullable values. Unsigned integers use their `i64` bit representation;
/// unsignedness, decimal scale, charset and collation live in the input schema.
#[derive(Clone, Debug, PartialEq)]
pub enum Column {
    Int(Vec<Option<i64>>),
    Real(Vec<Option<f64>>),
    Bytes(Vec<Option<Vec<u8>>>),
    Decimal(Vec<Option<Decimal>>),
    DateTime(Vec<Option<DateTime>>),
    Duration(Vec<Option<Duration>>),
    Json(Vec<Option<Json>>),
    Enum(Vec<Option<Enum>>),
    /// A MySQL `SET` value: a `u64` bit mask plus the comma-joined names of the
    /// selected elements. Like `Enum` this is input-only at the wire boundary;
    /// the name bytes are preserved verbatim (element names need not be UTF-8).
    Set(Vec<Option<Set>>),
    VectorFloat32(Vec<Option<VectorFloat32>>),
}

impl Column {
    /// Number of physical rows, including NULLs.
    pub fn len(&self) -> usize {
        match self {
            Self::Int(v) => v.len(),
            Self::Real(v) => v.len(),
            Self::Bytes(v) => v.len(),
            Self::Decimal(v) => v.len(),
            Self::DateTime(v) => v.len(),
            Self::Duration(v) => v.len(),
            Self::Json(v) => v.len(),
            Self::Enum(v) => v.len(),
            Self::Set(v) => v.len(),
            Self::VectorFloat32(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn eval_type(&self) -> EvalType {
        match self {
            Self::Int(_) => EvalType::Int,
            Self::Real(_) => EvalType::Real,
            Self::Bytes(_) => EvalType::Bytes,
            Self::Decimal(_) => EvalType::Decimal,
            Self::DateTime(_) => EvalType::DateTime,
            Self::Duration(_) => EvalType::Duration,
            Self::Json(_) => EvalType::Json,
            Self::Enum(_) => EvalType::Enum,
            Self::Set(_) => EvalType::Set,
            Self::VectorFloat32(_) => EvalType::VectorFloat32,
        }
    }

    fn empty(tp: EvalType) -> Self {
        match tp {
            EvalType::Int => Self::Int(Vec::new()),
            EvalType::Real => Self::Real(Vec::new()),
            EvalType::Bytes => Self::Bytes(Vec::new()),
            EvalType::Decimal => Self::Decimal(Vec::new()),
            EvalType::DateTime => Self::DateTime(Vec::new()),
            EvalType::Duration => Self::Duration(Vec::new()),
            EvalType::Json => Self::Json(Vec::new()),
            EvalType::Enum => Self::Enum(Vec::new()),
            EvalType::Set => Self::Set(Vec::new()),
            EvalType::VectorFloat32 => Self::VectorFloat32(Vec::new()),
        }
    }

    fn copy_rows(&self, rows: &[usize]) -> Result<VectorValue, Error> {
        let mut out = VectorValue::with_capacity(rows.len(), self.eval_type());
        match (self, &mut out) {
            (Self::Int(src), VectorValue::Int(dst)) => {
                for &row in rows {
                    dst.push(src[row]);
                }
            }
            (Self::Real(src), VectorValue::Real(dst)) => {
                for &row in rows {
                    // Real::new rejects NaN but accepts infinity. NotNan's
                    // arithmetic can panic on Inf * 0 or Inf - Inf, so guard
                    // all selected nonfinite inputs before entering kernels.
                    if src[row].is_some_and(|value| !value.is_finite()) {
                        return Err(Error::invalid("nonfinite real inputs are not supported"));
                    }
                    dst.push(
                        src[row]
                            .map(Real::new)
                            .transpose()
                            .map_err(|e| Error::invalid(e.to_string()))?,
                    );
                }
            }
            (Self::Bytes(src), VectorValue::Bytes(dst)) => {
                for &row in rows {
                    dst.push(src[row].clone());
                }
            }
            (Self::Decimal(src), VectorValue::Decimal(dst)) => {
                for &row in rows {
                    dst.push(src[row]);
                }
            }
            (Self::DateTime(src), VectorValue::DateTime(dst)) => {
                for &row in rows {
                    if let Some(value) = src[row] {
                        values::validate_time(value)?;
                    }
                    dst.push(src[row]);
                }
            }
            (Self::Duration(src), VectorValue::Duration(dst)) => {
                for &row in rows {
                    dst.push(src[row]);
                }
            }
            (Self::Json(src), VectorValue::Json(dst)) => {
                for &row in rows {
                    if let Some(value) = &src[row] {
                        values::validate_json(value)?;
                    }
                    dst.push(src[row].clone());
                }
            }
            (Self::Enum(src), VectorValue::Enum(dst)) => {
                for &row in rows {
                    dst.push(src[row].clone());
                }
            }
            (Self::Set(src), VectorValue::Set(dst)) => {
                for &row in rows {
                    dst.push(src[row].clone());
                }
            }
            (Self::VectorFloat32(src), VectorValue::VectorFloat32(dst)) => {
                for &row in rows {
                    if let Some(value) = &src[row] {
                        VectorFloat32Ref::new(&value.value)?;
                    }
                    dst.push(src[row].clone());
                }
            }
            _ => unreachable!("matching column type"),
        }
        Ok(out)
    }

    fn push_result(&mut self, value: ScalarValueRef<'_>) -> Result<(), Error> {
        match (self, value) {
            (Self::Int(dst), ScalarValueRef::Int(v)) => dst.push(v.copied()),
            (Self::Real(dst), ScalarValueRef::Real(v)) => dst.push(v.map(|v| v.into_inner())),
            (Self::Bytes(dst), ScalarValueRef::Bytes(v)) => dst.push(v.map(<[u8]>::to_vec)),
            (Self::Decimal(dst), ScalarValueRef::Decimal(v)) => dst.push(v.copied()),
            (Self::DateTime(dst), ScalarValueRef::DateTime(v)) => dst.push(v.copied()),
            (Self::Duration(dst), ScalarValueRef::Duration(v)) => dst.push(v.copied()),
            (Self::Json(dst), ScalarValueRef::Json(v)) => dst.push(v.map(|v| v.to_owned())),
            (Self::Enum(dst), ScalarValueRef::Enum(v)) => dst.push(v.map(|v| v.to_owned())),
            (Self::Set(dst), ScalarValueRef::Set(v)) => dst.push(v.map(|v| v.to_owned())),
            (Self::VectorFloat32(dst), ScalarValueRef::VectorFloat32(v)) => {
                dst.push(v.map(|v| v.to_owned()))
            }
            _ => return Err(Error::invalid("RPN output type differs from declared type")),
        }
        Ok(())
    }
}

/// Statement semantics, fixed at compilation. Unknown flag and SQL-mode bits
/// are ignored exactly as in TiKV's DAG request conversion. A nonempty timezone
/// name takes precedence over the offset (seconds east of UTC).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub flags: u64,
    pub sql_mode: u64,
    pub time_zone_name: Option<String>,
    pub time_zone_offset: i64,
    pub div_precision_increment: u8,
    pub max_warning_count: usize,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            flags: 0,
            sql_mode: 0,
            time_zone_name: None,
            time_zone_offset: 0,
            div_precision_increment: 4,
            max_warning_count: 64,
        }
    }
}

impl Context {
    /// Encode a typed temporal value using the engine's session timezone rules.
    /// The returned MysqlTime payload is packed UTC for TIMESTAMP, wall fields
    /// otherwise. Reject warnings or any loss of wall fields, kind or FSP on
    /// decode. This preserves values, not a host dialect's DST-fold choice;
    /// callers with different fold semantics must constrain their admission.
    pub fn pack_time_literal(&self, value: &DateTime) -> Result<u64, Error> {
        values::validate_time(*value)?;
        let mut ctx = EvalContext::new(self.config()?);
        let packed = value.to_packed_u64(&mut ctx)?;
        let decoded =
            DateTime::from_packed_u64(&mut ctx, packed, value.get_time_type(), value.fsp() as i8)?;
        if ctx.warnings.warning_cnt != 0
            || date_time_to_chunk(&decoded)? != date_time_to_chunk(value)?
        {
            return Err(Error::invalid(
                "time literal cannot round-trip without warnings",
            ));
        }
        Ok(packed)
    }

    fn config(&self) -> Result<Arc<EvalConfig>, Error> {
        if self.div_precision_increment > 30 {
            return Err(Error::invalid("div_precision_increment must be in 0..=30"));
        }
        // EvalWarnings eagerly reserves this many slots. Bound untrusted input.
        if self.max_warning_count > 65535 {
            return Err(Error::invalid("max_warning_count must be at most 65535"));
        }
        let mut cfg = EvalConfig::from_flag(Flag::from_bits_truncate(self.flags));
        cfg.set_sql_mode(SqlMode::from_bits_truncate(self.sql_mode));
        cfg.set_div_precision_incr(self.div_precision_increment);
        cfg.set_max_warning_cnt(self.max_warning_count);
        if let Some(name) = self.time_zone_name.as_deref().filter(|s| !s.is_empty()) {
            cfg.set_time_zone_by_name(name)?;
        } else {
            // Tz::from_offset currently casts i64 to i32. Reject truncation at
            // this public boundary rather than accepting a wrapped timezone.
            if i32::try_from(self.time_zone_offset).is_err() {
                return Err(tidb_query_datatype::codec::Error::invalid_timezone(format!(
                    "offset {}s",
                    self.time_zone_offset
                ))
                .into());
            }
            cfg.set_time_zone_by_offset(self.time_zone_offset)?;
        }
        Ok(Arc::new(cfg))
    }
}

/// MySQL error code and message without an engine-specific wrapper prefix.
/// Facade admission/shape errors use MySQL's generic error code 1105;
/// errors from the existing builder/evaluator retain their original code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub code: i32,
    pub message: String,
}

impl Error {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: 1105,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (MySQL {})", self.message, self.code)
    }
}
impl std::error::Error for Error {}

impl From<tidb_query_common::Error> for Error {
    fn from(error: tidb_query_common::Error) -> Self {
        match *error.0 {
            ErrorInner::Evaluate(e) => Self {
                code: e.code(),
                message: e.to_string(),
            },
            ErrorInner::Storage(e) => Self::invalid(e.to_string()),
        }
    }
}

impl From<tidb_query_datatype::codec::Error> for Error {
    fn from(error: tidb_query_datatype::codec::Error) -> Self {
        // The engine's conversion preserves the native MySQL code and message.
        Self::from(tidb_query_common::Error::from(error))
    }
}

/// One retained warning occurrence, in engine evaluation order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    pub code: i32,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EvalOutput {
    /// Dense output in selection order (or physical order without a selection).
    pub column: Column,
    /// At most `max_warning_count` details across all internal batches.
    pub warnings: Vec<Warning>,
    /// Total occurrences, including details dropped by the warning limit.
    pub warning_count: usize,
}

/// Mutable per-call execution state for a [`PreparedExpression`].
///
/// The compiled program is immutable and shareable; this struct carries the
/// scratch that changes during one evaluation: the [`EvalContext`] (which owns
/// the warning buffer) and the reusable selection/dense row buffers. The RPN
/// stack, decoded input columns and the output column are allocated inside each
/// call because they borrow call-scoped inputs; they are never stored in the
/// program.
///
/// A single state must not be used by two evaluations at once, but one compiled
/// program can serve any number of states (for example one per worker thread).
#[derive(Debug)]
pub struct ExecutionState {
    ctx: EvalContext,
    selection_scratch: Vec<usize>,
    dense_scratch: Vec<usize>,
}

/// The compiled program is immutable after compilation, and evaluation only
/// needs `&self` plus caller-owned scratch. This fails to compile if a future
/// change reintroduces per-execution or thread-affine state into it.
static_assertions::assert_impl_all!(PreparedExpression: Send, Sync);

/// An RPN program and its fixed schema/context. No kernels are reimplemented.
#[derive(Debug)]
pub struct PreparedExpression {
    expression: RpnExpression,
    schema: Vec<FieldType>,
    input_types: Vec<EvalType>,
    output_type: EvalType,
    config: Arc<EvalConfig>,
    fractional_digit_columns: Vec<safety::DigitColumn>,
}

impl PreparedExpression {
    /// Parses and validates wire messages, then invokes the existing RPN
    /// builder. Only admitted signatures are accepted; unsupported input is
    /// an error, never an implicit fallback. Callers may choose fallback at
    /// this boundary.
    pub fn compile(expr_bytes: &[u8], schema: &[Vec<u8>], context: Context) -> Result<Self, Error> {
        let tree = protobuf::parse_from_bytes::<Expr>(expr_bytes)
            .map_err(|e| Error::invalid(e.to_string()))?;
        let schema: Vec<FieldType> = schema
            .iter()
            .map(|bytes| {
                protobuf::parse_from_bytes::<FieldType>(bytes)
                    .map_err(|e| Error::invalid(e.to_string()))
            })
            .collect::<Result<_, _>>()?;
        let input_types = schema.iter().map(field_type).collect::<Result<_, _>>()?;
        let output_type = validate_expr(&tree, &schema, 0)?;
        let mut fractional_digit_columns = Vec::new();
        safety::collect_digit_columns(&tree, &mut fractional_digit_columns)?;
        let config = context.config()?;
        let mut ctx = EvalContext::new(config.clone());
        let expression = RpnExpressionBuilder::build_from_expr_tree(tree, &mut ctx, schema.len())?;
        // Admitted constants do not require lossy decoding. Do not silently lose
        // compile-time warnings if the admitted set is expanded in the future.
        if ctx.warnings.warning_cnt != 0 {
            return Err(Error::invalid("constant decoding produced warnings"));
        }
        Ok(Self {
            expression,
            schema,
            input_types,
            output_type,
            config,
            fractional_digit_columns,
        })
    }

    /// Whether any node of the compiled program is a kernel registered with a
    /// lazy implementation.
    ///
    /// This answers "is this program lazy at all", not "is this program *safe*
    /// to run under MySQL's short-circuit semantics": a program can contain one
    /// lazy node and one eager node of a different lazy-sensitive family. Use
    /// [`Self::eager_lazy_risk`] for the sound admission test.
    pub fn has_lazy_nodes(&self) -> bool {
        self.expression.as_ref().iter().any(|node| match node {
            RpnExpressionNode::FnCall { func_meta, .. } => func_meta.lazy_fn_ptr.is_some(),
            _ => false,
        })
    }

    /// Kernel names of nodes that implement a lazy-sensitive SQL construct but
    /// were dispatched to a kernel without a lazy implementation.
    ///
    /// An empty result is the sound signal that the embedder may relax its
    /// shape gate: every lazy-sensitive node in the program (if any) is
    /// actually lazy. A nonempty result lists the offending kernel names in
    /// first-occurrence order, without duplicates, so the caller can record a
    /// fallback reason. Constants and column references can never contribute.
    pub fn eager_lazy_risk(&self) -> Vec<&'static str> {
        let mut risk = Vec::new();
        for node in self.expression.as_ref() {
            if let RpnExpressionNode::FnCall { func_meta, .. } = node {
                if func_meta.lazy_fn_ptr.is_none()
                    && crate::LAZY_SENSITIVE_KERNELS.contains(&func_meta.name)
                    && !risk.contains(&func_meta.name)
                {
                    risk.push(func_meta.name);
                }
            }
        }
        risk
    }

    /// Creates fresh execution state bound to this program's fixed
    /// configuration. Reuse it across calls on one thread to avoid reallocating
    /// the warning and selection scratch; create one per concurrent evaluator.
    pub fn execution_state(&self) -> ExecutionState {
        ExecutionState {
            ctx: EvalContext::new(self.config.clone()),
            selection_scratch: Vec::new(),
            dense_scratch: Vec::new(),
        }
    }

    /// Evaluates one independent batch with fresh warnings, allocating fresh
    /// execution state per call. Kept for callers that hold the program
    /// mutably; prefer [`Self::eval_shared`] or [`Self::eval_with_state`]
    /// when the compiled program is shared.
    pub fn eval(
        &mut self,
        columns: &[Column],
        row_count: usize,
        selection: Option<&[usize]>,
    ) -> Result<EvalOutput, Error> {
        self.eval_shared(columns, row_count, selection)
    }

    /// Evaluates one independent batch through `&self`, so the same compiled
    /// program can run on several threads concurrently. Each call owns fresh
    /// scratch. Every physical column must contain exactly `row_count` values,
    /// even if not referenced. Selection indices may repeat or be unordered;
    /// only selected values are converted. Empty selection/batch returns an
    /// empty typed column without invoking the engine. Errors discard partial
    /// output, without fallback.
    pub fn eval_shared(
        &self,
        columns: &[Column],
        row_count: usize,
        selection: Option<&[usize]>,
    ) -> Result<EvalOutput, Error> {
        let mut state = self.execution_state();
        self.eval_with_state(&mut state, columns, row_count, selection)
    }

    /// Evaluates one independent batch through `&self` using caller-owned
    /// execution state. The warning state is reset at the start of every call,
    /// so reusing a state never leaks warnings across batches. See
    /// [`Self::eval_shared`] for the input contract.
    pub fn eval_with_state(
        &self,
        state: &mut ExecutionState,
        columns: &[Column],
        row_count: usize,
        selection: Option<&[usize]>,
    ) -> Result<EvalOutput, Error> {
        if columns.len() != self.schema.len() {
            return Err(Error::invalid("column count differs from compiled schema"));
        }
        for (column, tp) in columns.iter().zip(&self.input_types) {
            if column.len() != row_count || column.eval_type() != *tp {
                return Err(Error::invalid(
                    "column type or row count differs from compiled schema",
                ));
            }
        }
        if selection.is_some_and(|rows| rows.iter().any(|&row| row >= row_count)) {
            return Err(Error::invalid("selection index out of bounds"));
        }
        safety::validate_digit_columns(
            &self.fractional_digit_columns,
            columns,
            row_count,
            selection,
        )?;
        // Reusing caller state must not carry warnings into this call. When the
        // state already belongs to this program, clear the warning buffer in
        // place; otherwise rebind the context to this program's configuration.
        if Arc::ptr_eq(&state.ctx.cfg, &self.config) {
            state.ctx.warnings.warning_cnt = 0;
            state.ctx.warnings.warnings.clear();
        } else {
            state.ctx = EvalContext::new(self.config.clone());
        }
        let mut output = Column::empty(self.output_type);
        let output_rows = selection.map_or(row_count, <[usize]>::len);
        for start in (0..output_rows).step_by(BATCH_MAX_SIZE) {
            let end = output_rows.min(start.saturating_add(BATCH_MAX_SIZE));
            state.selection_scratch.clear();
            match selection {
                Some(rows) => state.selection_scratch.extend_from_slice(&rows[start..end]),
                None => state.selection_scratch.extend(start..end),
            }
            let rows_len = state.selection_scratch.len();
            let decoded: LazyBatchColumnVec = columns
                .iter()
                .map(|c| c.copy_rows(&state.selection_scratch))
                .collect::<Result<Vec<_>, _>>()?
                .into();
            // The caller batch is copied densely in selection order, so the
            // evaluator's logical rows are the dense indices, not the original
            // physical selection indices.
            state.dense_scratch.clear();
            state.dense_scratch.extend(0..rows_len);
            // The RPN stack borrows the expression and this batch's inputs, so
            // it is call-scoped scratch rather than program state.
            let mut stack = Vec::new();
            let result = self.expression.eval_decoded_with_finite_reals_into(
                &mut state.ctx,
                &self.schema,
                &decoded,
                &state.dense_scratch,
                rows_len,
                &mut stack,
            )?;
            for row in 0..rows_len {
                output.push_result(result.get_logical_scalar_ref(row))?;
            }
        }
        Ok(EvalOutput {
            column: output,
            warning_count: state.ctx.warnings.warning_cnt,
            warnings: state
                .ctx
                .warnings
                .warnings
                .iter()
                .map(|w| Warning {
                    code: w.get_code(),
                    message: w.get_msg().to_owned(),
                })
                .collect(),
        })
    }
}

fn field_type(ft: &FieldType) -> Result<EvalType, Error> {
    // Match raw wire values before using accessors: unknown values must not be
    // silently interpreted as the default MySQL type.
    let raw = FieldTypeTp::from_i32(ft.get_tp())
        .ok_or_else(|| Error::invalid(format!("unknown field type {}", ft.get_tp())))?;
    let tp = EvalType::try_from(raw).map_err(|e| Error::invalid(e.to_string()))?;
    if matches!(tp, EvalType::DateTime | EvalType::Duration)
        && !(-1..=6).contains(&ft.get_decimal())
    {
        return Err(Error::invalid(
            "temporal fractional precision must be -1..=6",
        ));
    }
    if tp == EvalType::Real
        && (!(-1..=254).contains(&ft.get_flen())
            || !(-1..=254).contains(&ft.get_decimal())
            || (ft.get_flen() >= 0 && ft.get_decimal() >= 0 && ft.get_flen() < ft.get_decimal()))
    {
        return Err(Error::invalid("unsafe REAL precision or scale metadata"));
    }
    if tp == EvalType::Decimal
        && (!(-1..=30).contains(&ft.get_decimal()) || !(-1..=65).contains(&ft.get_flen()))
    {
        return Err(Error::invalid("invalid decimal precision or scale"));
    }
    Ok(tp)
}

fn validate_expr(expr: &Expr, schema: &[FieldType], depth: usize) -> Result<EvalType, Error> {
    if depth > 64 || !expr.has_tp() || !expr.has_field_type() {
        return Err(Error::invalid(
            "expression is too deep or missing type metadata",
        ));
    }
    let tp = field_type(expr.get_field_type())?;
    if expr.get_tp() != ExprType::ScalarFunc && !expr.get_children().is_empty() {
        return Err(Error::invalid("non-function expression has children"));
    }
    match expr.get_tp() {
        ExprType::ColumnRef => {
            if expr.get_val().len() != 8 {
                return Err(Error::invalid("column offset must be an encoded i64"));
            }
            let offset = expr
                .get_val()
                .read_i64()
                .map_err(|e| Error::invalid(e.to_string()))?;
            let ft = usize::try_from(offset)
                .ok()
                .and_then(|i| schema.get(i))
                .ok_or_else(|| Error::invalid("column offset out of bounds"))?;
            if ft != expr.get_field_type() {
                return Err(Error::invalid(
                    "column reference metadata differs from schema",
                ));
            }
        }
        ExprType::ScalarFunc => {
            let args = expr
                .get_children()
                .iter()
                .map(|c| validate_expr(c, schema, depth + 1))
                .collect::<Result<Vec<_>, _>>()?;
            validate_builder_safety(expr, &args)?;
            // Keep the mapper and generated/handwritten validators authoritative.
            // Do not initialize metadata here: IN mutates children during build.
            let meta = crate::map_expr_node_to_rpn_func(expr)?;
            (meta.validator_ptr)(expr)?;
        }
        ExprType::Null => {
            if !expr.get_val().is_empty() {
                return Err(Error::invalid("NULL has a payload"));
            }
        }
        ExprType::Int64 | ExprType::Uint64 if tp == EvalType::Int => {
            if expr.get_val().len() != 8 {
                return Err(Error::invalid("integer payload must be eight bytes"));
            }
        }
        ExprType::Float32 | ExprType::Float64 if tp == EvalType::Real => {
            if expr.get_val().len() != 8 {
                return Err(Error::invalid("real payload must be eight bytes"));
            }
            if !expr
                .get_val()
                .read_f64()
                .map_err(|e| Error::invalid(e.to_string()))?
                .is_finite()
            {
                return Err(Error::invalid("nonfinite real constants are not supported"));
            }
        }
        ExprType::MysqlTime if tp == EvalType::DateTime => {
            if expr.get_val().len() != 8 {
                return Err(Error::invalid("time payload must be eight bytes"));
            }
        }
        ExprType::MysqlDuration if tp == EvalType::Duration => {
            if expr.get_val().len() != 8 {
                return Err(Error::invalid("duration payload must be eight bytes"));
            }
        }
        ExprType::MysqlEnum if tp == EvalType::Enum => {
            if expr.get_val().len() != 8 {
                return Err(Error::invalid("enum payload must be eight bytes"));
            }
            let value = expr
                .get_val()
                .read_u64()
                .map_err(|e| Error::invalid(e.to_string()))?;
            if value > expr.get_field_type().get_elems().len() as u64 {
                return Err(Error::invalid(
                    "enum constant index exceeds declared elements",
                ));
            }
        }
        ExprType::MysqlBit if tp == EvalType::Int => {
            if expr.get_val().len() > 8 {
                return Err(Error::invalid("bit payload exceeds eight bytes"));
            }
        }
        ExprType::MysqlJson if tp == EvalType::Json => {
            json_from_binary(expr.get_val())?;
        }
        ExprType::TiDbVectorFloat32 if tp == EvalType::VectorFloat32 => {
            use tidb_query_datatype::codec::mysql::VectorFloat32Decoder;
            let mut data = expr.get_val();
            data.read_vector_float32()?;
            if !data.is_empty() {
                return Err(Error::invalid("trailing vector payload"));
            }
        }
        ExprType::Bytes | ExprType::String if tp == EvalType::Bytes => {}
        ExprType::MysqlDecimal if tp == EvalType::Decimal => {
            // Existing decimal decoder checks the encoded payload itself.
            if expr.get_val().len() < 2 {
                return Err(Error::invalid("missing decimal payload"));
            }
        }
        _ => {
            return Err(Error::invalid(
                "unsupported expression or mismatched constant type",
            ));
        }
    }
    Ok(tp)
}

// Safety gaps in trusted-plan mappers/validators, NOT a signature whitelist.
fn validate_builder_safety(expr: &Expr, args: &[EvalType]) -> Result<(), Error> {
    use EvalType::{Bytes, Int};
    use ScalarFuncSig::*;
    if !expr.has_sig() {
        return Err(Error::invalid("scalar function has no signature"));
    }
    match expr.get_sig() {
        ToBinary if args.len() != 1 => return Err(Error::invalid("ToBinary requires one child")),
        LikeSig if args.len() != 3 => return Err(Error::invalid("LIKE requires three children")),
        RegexpSig | RegexpUtf8Sig | RegexpLikeSig | RegexpSubstrSig | RegexpInStrSig
        | RegexpReplaceSig => {
            // raw_varg's generated validator checks arity but does not check
            // types. These kernels call as_bytes/as_int, which panic on mismatch.
            let sig = expr.get_sig();
            for (i, &actual) in args.iter().enumerate() {
                let expected = match sig {
                    RegexpSig | RegexpUtf8Sig | RegexpLikeSig => Bytes,
                    RegexpSubstrSig => {
                        if i < 2 || i == 4 {
                            Bytes
                        } else {
                            Int
                        }
                    }
                    RegexpInStrSig => {
                        if i < 2 || i == 5 {
                            Bytes
                        } else {
                            Int
                        }
                    }
                    RegexpReplaceSig => {
                        if i < 3 || i == 5 {
                            Bytes
                        } else {
                            Int
                        }
                    }
                    _ => unreachable!(),
                };
                if actual != expected {
                    return Err(Error::invalid("invalid regexp argument type"));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

mod borrowed;
pub use borrowed::{Diagnostics, ScalarRef};

pub use crate::types::borrowed::{ColumnRef, SelectedColumnRef};

#[cfg(test)]
mod coverage_tests;
#[cfg(test)]
mod safety_tests;
#[cfg(test)]
mod tests;
