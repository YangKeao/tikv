// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use tidb_query_datatype::{EvalType, codec::data_type::ScalarValueRef, expr::EvalContext};

use super::{ColumnRef, Error, PreparedExpression, Warning};
use crate::{BATCH_MAX_SIZE, RpnExpressionNode};

/// Output values are borrowed only for the callback invocation. Numeric values
/// are passed by value and bytes remain a slice of the engine/input storage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScalarRef<'a> {
    Null,
    Int(i64),
    Real(f64),
    Bytes(&'a [u8]),
}

/// Fresh warning state for one completed borrowed evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostics {
    pub warnings: Vec<Warning>,
    pub warning_count: usize,
}

fn supported(tp: EvalType) -> bool {
    matches!(tp, EvalType::Int | EvalType::Real | EvalType::Bytes)
}

impl PreparedExpression {
    /// Whether the compiled program can use packed borrowed input loaders.
    /// Unsupported kernels/types remain available through the original copying
    /// API; callers must choose their fallback before invoking evaluation.
    pub fn supports_borrowed(&self) -> bool {
        self.fractional_digit_columns.is_empty()
            && supported(self.output_type)
            && self.input_types.iter().copied().all(supported)
            && self.expression.as_ref().iter().all(|node| match node {
                RpnExpressionNode::Constant { value, .. } => supported(value.eval_type()),
                RpnExpressionNode::ColumnRef { .. } => true,
                RpnExpressionNode::FnCall {
                    func_meta,
                    field_type,
                    ..
                } => {
                    func_meta.borrowed_fn_ptr.is_some()
                        && super::field_type(field_type).is_ok_and(supported)
                }
            })
    }

    /// Evaluate borrowed native-endian packed columns without bulk input
    /// materialization. Shapes, selection and all selected REAL values are
    /// validated BEFORE entering any kernel or invoking the sink. Unselected
    /// nonfinite values are not touched. Empty inputs produce no callback.
    ///
    /// Original kernels own their intermediate/output vectors. Values are sent
    /// straight to the sink, with no owned facade output column. The sink
    /// cannot retain references past its call. A later runtime/sink error
    /// may follow earlier successful callbacks; the caller must discard
    /// partial output and must never replay the batch natively. No
    /// references are retained by self.
    pub fn eval_borrowed<F>(
        &mut self,
        columns: &[ColumnRef<'_>],
        row_count: usize,
        selection: Option<&[usize]>,
        mut sink: F,
    ) -> Result<Diagnostics, Error>
    where
        F: for<'a> FnMut(ScalarRef<'a>) -> Result<(), Error>,
    {
        if !self.supports_borrowed() {
            return Err(Error::invalid(
                "compiled expression does not support borrowed evaluation",
            ));
        }
        if columns.len() != self.input_types.len() {
            return Err(Error::invalid("borrowed column count differs from schema"));
        }
        for (column, expected) in columns.iter().zip(&self.input_types) {
            if column.eval_type() != *expected {
                return Err(Error::invalid("borrowed column type differs from schema"));
            }
            column.validate(row_count).map_err(Error::invalid)?;
        }
        if selection.is_some_and(|rows| rows.iter().any(|&row| row >= row_count)) {
            return Err(Error::invalid("borrowed selection index out of bounds"));
        }
        let output_rows = selection.map_or(row_count, <[usize]>::len);
        for column in columns {
            if column.eval_type() == EvalType::Real {
                for logical in 0..output_rows {
                    let physical = selection.map_or(logical, |rows| rows[logical]);
                    if !column.finite_at(physical) {
                        return Err(Error::invalid("nonfinite real inputs are not supported"));
                    }
                }
            }
        }
        let mut ctx = EvalContext::new(self.config.clone());
        for start in (0..output_rows).step_by(BATCH_MAX_SIZE) {
            let rows = BATCH_MAX_SIZE.min(output_rows - start);
            let result = self
                .expression
                .eval_borrowed(&mut ctx, columns, selection, start, rows)?;
            for row in 0..rows {
                let holder = result.scalar(row);
                let value = match holder.as_scalar_ref() {
                    ScalarValueRef::Int(value) => {
                        value.map_or(ScalarRef::Null, |value| ScalarRef::Int(*value))
                    }
                    ScalarValueRef::Real(value) => {
                        value.map_or(ScalarRef::Null, |value| ScalarRef::Real(value.into_inner()))
                    }
                    ScalarValueRef::Bytes(value) => value.map_or(ScalarRef::Null, ScalarRef::Bytes),
                    _ => return Err(Error::invalid("unsupported borrowed output type")),
                };
                sink(value)?;
            }
        }
        Ok(Diagnostics {
            warning_count: ctx.warnings.warning_cnt,
            warnings: ctx
                .warnings
                .warnings
                .into_iter()
                .map(|warning| Warning {
                    code: warning.get_code(),
                    message: warning.get_msg().to_owned(),
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests;
