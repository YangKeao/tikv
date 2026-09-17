// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Copying, in-process embedding of the existing TiKV RPN engine.
//!
//! The wire boundary uses serialized tipb messages, not protobuf Rust types, so
//! callers using prost can embed this rust-protobuf engine. This PoC accepts
//! integer, real, byte-string and decimal columns and a deliberately restricted
//! scalar signature set (see `validate_signature`). It does not run a server,
//! perform storage IO, or implement any SQL kernels. Compilation builds RPN
//! once. Evaluation copies selected rows into decoded TiKV columns and copies
//! results back, splitting batches at the engine's batch limit.

use std::{fmt, sync::Arc};

use codec::prelude::NumberDecoder;
use tidb_query_common::error::ErrorInner;
use tidb_query_datatype::{
    EvalType,
    codec::{
        batch::LazyBatchColumnVec,
        data_type::{ChunkedVec, Decimal, Real, ScalarValueRef, VectorValue},
    },
    expr::{EvalConfig, EvalContext, Flag, SqlMode},
};
use tipb::{Expr, ExprType, FieldType, ScalarFuncSig};

use crate::{BATCH_MAX_SIZE, RpnExpression, RpnExpressionBuilder};

/// Owned nullable values. Unsigned integers use their `i64` bit representation;
/// unsignedness, decimal scale, charset and collation live in the input schema.
#[derive(Clone, Debug, PartialEq)]
pub enum Column {
    Int(Vec<Option<i64>>),
    Real(Vec<Option<f64>>),
    Bytes(Vec<Option<Vec<u8>>>),
    Decimal(Vec<Option<String>>),
}

impl Column {
    /// Number of physical rows, including NULLs.
    pub fn len(&self) -> usize {
        match self {
            Self::Int(v) => v.len(),
            Self::Real(v) => v.len(),
            Self::Bytes(v) => v.len(),
            Self::Decimal(v) => v.len(),
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
        }
    }

    fn empty(tp: EvalType) -> Self {
        match tp {
            EvalType::Int => Self::Int(Vec::new()),
            EvalType::Real => Self::Real(Vec::new()),
            EvalType::Bytes => Self::Bytes(Vec::new()),
            EvalType::Decimal => Self::Decimal(Vec::new()),
            _ => unreachable!("validated standalone type"),
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
                    dst.push(
                        src[row]
                            .as_ref()
                            .map(|s| s.parse::<Decimal>())
                            .transpose()
                            .map_err(Error::from)?,
                    );
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
            (Self::Decimal(dst), ScalarValueRef::Decimal(v)) => {
                dst.push(v.map(ToString::to_string))
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

/// An RPN program and its fixed schema/context. No kernels are reimplemented.
#[derive(Debug)]
pub struct PreparedExpression {
    expression: RpnExpression,
    schema: Vec<FieldType>,
    input_types: Vec<EvalType>,
    output_type: EvalType,
    config: Arc<EvalConfig>,
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
        })
    }

    /// Evaluates one independent batch with fresh warnings. Every physical
    /// column must contain exactly `row_count` values, even if not referenced.
    /// Selection indices may repeat or be unordered; only selected values are
    /// converted. Empty selection/batch returns an empty typed column without
    /// invoking the engine. Errors discard partial output, without fallback.
    pub fn eval(
        &mut self,
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
        let mut ctx = EvalContext::new(self.config.clone());
        let mut output = Column::empty(self.output_type);
        let output_rows = selection.map_or(row_count, <[usize]>::len);
        for start in (0..output_rows).step_by(BATCH_MAX_SIZE) {
            let end = output_rows.min(start.saturating_add(BATCH_MAX_SIZE));
            let rows: Vec<usize> = match selection {
                Some(rows) => rows[start..end].to_vec(),
                None => (start..end).collect(),
            };
            let decoded: LazyBatchColumnVec = columns
                .iter()
                .map(|c| c.copy_rows(&rows))
                .collect::<Result<Vec<_>, _>>()?
                .into();
            let dense: Vec<usize> = (0..rows.len()).collect();
            let result = self.expression.eval_decoded(
                &mut ctx,
                &self.schema,
                &decoded,
                &dense,
                rows.len(),
            )?;
            for row in 0..rows.len() {
                output.push_result(result.get_logical_scalar_ref(row))?;
            }
        }
        Ok(EvalOutput {
            column: output,
            warning_count: ctx.warnings.warning_cnt,
            warnings: ctx
                .warnings
                .warnings
                .into_iter()
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
    let tp = match ft.get_tp() {
        1 | 2 | 3 | 8 | 9 | 13 => EvalType::Int,
        4 | 5 => EvalType::Real,
        15 | 249..=254 => EvalType::Bytes,
        246 => EvalType::Decimal,
        other => return Err(Error::invalid(format!("unsupported field type {other}"))),
    };
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
            validate_signature(expr.get_sig(), &args, tp)?;
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

// Admission, not SQL implementation. Checking exact arity and eval types before
// mapping is important: some existing mappers index child metadata directly.
fn validate_signature(
    sig: ScalarFuncSig,
    args: &[EvalType],
    result: EvalType,
) -> Result<(), Error> {
    use EvalType::{Bytes as B, Decimal as D, Int as I, Real as R};
    use ScalarFuncSig::*;
    let (input, output, arity): (EvalType, EvalType, usize) = match sig {
        PlusInt | MinusInt | MultiplyInt | ModInt | IntDivideInt => (I, I, 2),
        PlusReal | MinusReal | MultiplyReal | DivideReal | ModReal => (R, R, 2),
        PlusDecimal | MinusDecimal | MultiplyDecimal | DivideDecimal | ModDecimal => (D, D, 2),
        LtInt | LeInt | GtInt | GeInt | EqInt | NeInt | NullEqInt => (I, I, 2),
        LtReal | LeReal | GtReal | GeReal | EqReal | NeReal | NullEqReal => (R, I, 2),
        LtDecimal | LeDecimal | GtDecimal | GeDecimal | EqDecimal | NeDecimal | NullEqDecimal => {
            (D, I, 2)
        }
        AbsInt | AbsUInt | UnaryMinusInt | UnaryNotInt | IntIsNull => (I, I, 1),
        AbsReal | UnaryMinusReal => (R, R, 1),
        AbsDecimal | UnaryMinusDecimal => (D, D, 1),
        RealIsNull => (R, I, 1),
        DecimalIsNull => (D, I, 1),
        Length | BitLength | Ascii | StringIsNull => (B, I, 1),
        Concat if !args.is_empty() => (B, B, args.len()),
        _ => {
            return Err(Error::invalid(format!(
                "unsupported standalone signature {sig:?}"
            )));
        }
    };
    if args.len() != arity || args.iter().any(|tp| *tp != input) || result != output {
        return Err(Error::invalid(format!(
            "invalid standalone signature {sig:?}: argument or return type/arity"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
