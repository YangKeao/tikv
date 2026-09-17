// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Borrowed packed and native inputs for the opt-in standalone RPN path.
//!
//! Numeric values are loaded safely into scalar temporaries; byte payloads are
//! borrowed. Packed numeric bytes need no alignment; native numeric slices use
//! ordinary Rust typed references. This module does not implement SQL kernels.

use tidb_query_common::Result;
use tidb_query_datatype::{
    EvalType,
    codec::data_type::{Real, ScalarValue, ScalarValueRef, VectorValue},
    expr::EvalContext,
};

use super::{RpnExpression, RpnExpressionNode, RpnFnCallExtra};

/// Immutable borrowed column storage. Packed numeric bytes are native-endian,
/// and may be unaligned; packed validity uses LSB-first non-NULL bits.
/// Native columns use an optional byte-per-row null map (any nonzero is NULL).
/// Native storage has row_count entries, or exactly one when broadcasting,
/// including when row_count is zero.
#[derive(Clone, Copy, Debug)]
pub enum ColumnRef<'a> {
    Int {
        values: &'a [u8],
        validity: &'a [u8],
    },
    Real {
        values: &'a [u8],
        validity: &'a [u8],
    },
    Bytes {
        values: &'a [u8],
        offsets: &'a [i64],
        validity: &'a [u8],
    },
    NativeInt {
        values: &'a [i64],
        nulls: Option<&'a [u8]>,
        broadcast: bool,
    },
    NativeReal {
        values: &'a [f64],
        nulls: Option<&'a [u8]>,
        broadcast: bool,
    },
    /// Cumulative END offsets include one required trailing NUL per stored row.
    /// Embedded NULs are payload; even NULL rows must have valid storage.
    NativeBytes {
        values: &'a [u8],
        offsets: &'a [u64],
        nulls: Option<&'a [u8]>,
        broadcast: bool,
    },
}

impl<'a> ColumnRef<'a> {
    pub(crate) fn eval_type(self) -> EvalType {
        match self {
            Self::Int { .. } | Self::NativeInt { .. } => EvalType::Int,
            Self::Real { .. } | Self::NativeReal { .. } => EvalType::Real,
            Self::Bytes { .. } | Self::NativeBytes { .. } => EvalType::Bytes,
        }
    }

    fn stored_row(self, row: usize) -> usize {
        match self {
            Self::NativeInt {
                broadcast: true, ..
            }
            | Self::NativeReal {
                broadcast: true, ..
            }
            | Self::NativeBytes {
                broadcast: true, ..
            } => 0,
            _ => row,
        }
    }

    // The caller has already mapped broadcast inputs to their stored row.
    fn is_valid(self, row: usize) -> bool {
        match self {
            Self::Int { validity, .. }
            | Self::Real { validity, .. }
            | Self::Bytes { validity, .. } => validity[row / 8] & (1 << (row % 8)) != 0,
            Self::NativeInt { nulls, .. }
            | Self::NativeReal { nulls, .. }
            | Self::NativeBytes { nulls, .. } => nulls.is_none_or(|nulls| nulls[row] == 0),
        }
    }

    pub(crate) fn validate(self, rows: usize) -> std::result::Result<(), &'static str> {
        let stored_rows = match self {
            Self::Int { validity, .. }
            | Self::Real { validity, .. }
            | Self::Bytes { validity, .. } => {
                let bitmap_len = rows.div_ceil(8);
                if validity.len() < bitmap_len {
                    return Err("borrowed validity bitmap is too short");
                }
                rows
            }
            Self::NativeInt {
                nulls, broadcast, ..
            }
            | Self::NativeReal {
                nulls, broadcast, ..
            }
            | Self::NativeBytes {
                nulls, broadcast, ..
            } => {
                let stored_rows = if broadcast { 1 } else { rows };
                if nulls.is_some_and(|nulls| nulls.len() != stored_rows) {
                    return Err("native null map length differs from stored row count");
                }
                stored_rows
            }
        };
        match self {
            Self::Int { values, .. } | Self::Real { values, .. } => {
                if rows.checked_mul(8) != Some(values.len()) {
                    return Err("borrowed numeric byte length differs from row count");
                }
            }
            Self::Bytes {
                values, offsets, ..
            } => {
                if rows.checked_add(1) != Some(offsets.len()) {
                    return Err("borrowed offsets must contain row_count + 1 entries");
                }
                let mut previous = 0;
                for &offset in offsets {
                    let offset =
                        usize::try_from(offset).map_err(|_| "negative borrowed byte offset")?;
                    if offset < previous || offset > values.len() {
                        return Err("borrowed offsets are unordered or out of bounds");
                    }
                    previous = offset;
                }
            }
            Self::NativeInt { values, .. } => {
                if values.len() != stored_rows {
                    return Err("native integer length differs from stored row count");
                }
            }
            Self::NativeReal { values, .. } => {
                if values.len() != stored_rows {
                    return Err("native real length differs from stored row count");
                }
            }
            Self::NativeBytes {
                values, offsets, ..
            } => {
                if offsets.len() != stored_rows {
                    return Err("native offsets length differs from stored row count");
                }
                let mut previous = 0;
                for &offset in offsets {
                    let end = usize::try_from(offset)
                        .map_err(|_| "native byte offset does not fit usize")?;
                    if end <= previous || end > values.len() {
                        return Err("native end offsets are not strictly increasing or in bounds");
                    }
                    if values[end - 1] != 0 {
                        return Err("native byte row is missing its trailing NUL");
                    }
                    previous = end;
                }
                if previous != values.len() {
                    return Err("native final end offset differs from byte length");
                }
            }
        }
        Ok(())
    }

    pub(crate) fn finite_at(self, row: usize) -> bool {
        let row = self.stored_row(row);
        match self {
            Self::Real { values, .. } if self.is_valid(row) => {
                f64::from_ne_bytes(values[row * 8..row * 8 + 8].try_into().unwrap()).is_finite()
            }
            Self::NativeReal { values, .. } if self.is_valid(row) => values[row].is_finite(),
            _ => true,
        }
    }

    fn scalar(self, row: usize) -> ScalarHolder<'a> {
        let row = self.stored_row(row);
        let valid = self.is_valid(row);
        match self {
            Self::Int { values, .. } => ScalarHolder::Int(
                valid.then(|| i64::from_ne_bytes(values[row * 8..row * 8 + 8].try_into().unwrap())),
            ),
            Self::Real { values, .. } => ScalarHolder::Real(valid.then(|| {
                Real::new(f64::from_ne_bytes(
                    values[row * 8..row * 8 + 8].try_into().unwrap(),
                ))
                .expect("finite borrowed input validated")
            })),
            Self::Bytes {
                values, offsets, ..
            } => ScalarHolder::Ref(ScalarValueRef::Bytes(
                valid.then(|| &values[offsets[row] as usize..offsets[row + 1] as usize]),
            )),
            Self::NativeInt { values, .. } => ScalarHolder::Int(valid.then(|| values[row])),
            Self::NativeReal { values, .. } => ScalarHolder::Real(
                valid.then(|| Real::new(values[row]).expect("finite borrowed input validated")),
            ),
            Self::NativeBytes {
                values, offsets, ..
            } => ScalarHolder::Ref(ScalarValueRef::Bytes(valid.then(|| {
                let start = if row == 0 {
                    0
                } else {
                    offsets[row - 1] as usize
                };
                &values[start..offsets[row] as usize - 1]
            }))),
        }
    }
}

/// Keeps stack-local decoded primitives alive while original scalar kernels
/// borrow their arguments. Non-primitive values always use existing references.
#[derive(Debug)]
pub enum ScalarHolder<'a> {
    Int(Option<i64>),
    Real(Option<Real>),
    Ref(ScalarValueRef<'a>),
}
impl ScalarHolder<'_> {
    pub fn as_scalar_ref(&self) -> ScalarValueRef<'_> {
        match self {
            Self::Int(value) => ScalarValueRef::Int(value.as_ref()),
            Self::Real(value) => ScalarValueRef::Real(value.as_ref()),
            Self::Ref(value) => *value,
        }
    }
}

/// Function arguments on the separate borrowed-input evaluation stack.
#[derive(Debug)]
pub enum BorrowedStackNode<'a> {
    Constant(&'a ScalarValue),
    Input {
        column: ColumnRef<'a>,
        selection: Option<&'a [usize]>,
        start: usize,
    },
    Generated(VectorValue),
}
impl BorrowedStackNode<'_> {
    #[inline]
    pub fn scalar(&self, row: usize) -> ScalarHolder<'_> {
        match self {
            Self::Constant(value) => ScalarHolder::Ref(value.as_scalar_value_ref()),
            Self::Input {
                column,
                selection,
                start,
            } => {
                let logical = start + row;
                column.scalar(selection.map_or(logical, |rows| rows[logical]))
            }
            Self::Generated(value) => ScalarHolder::Ref(value.get_scalar_ref(row)),
        }
    }
}

impl RpnExpression {
    /// Same compiled-node ordering and kernel specialization as eval_decoded,
    /// but with safe borrowed argument loaders. The facade validates all input
    /// shapes/value domains before this private execution entry is called.
    pub(crate) fn eval_borrowed<'a>(
        &'a self,
        ctx: &mut EvalContext,
        columns: &[ColumnRef<'a>],
        selection: Option<&'a [usize]>,
        start: usize,
        rows: usize,
    ) -> Result<BorrowedStackNode<'a>> {
        assert!(rows > 0 && rows <= super::BATCH_MAX_SIZE);
        let mut stack = Vec::with_capacity(self.len());
        for node in self.as_ref() {
            match node {
                RpnExpressionNode::Constant { value, .. } => {
                    stack.push(BorrowedStackNode::Constant(value))
                }
                RpnExpressionNode::ColumnRef { offset } => stack.push(BorrowedStackNode::Input {
                    column: columns[*offset],
                    selection,
                    start,
                }),
                RpnExpressionNode::FnCall {
                    func_meta,
                    args_len,
                    field_type,
                    metadata,
                } => {
                    let begin = stack.len() - args_len;
                    let mut extra = RpnFnCallExtra {
                        ret_field_type: field_type,
                    };
                    let evaluate = func_meta
                        .borrowed_fn_ptr
                        .expect("borrowed function support validated");
                    let result = evaluate(ctx, rows, &stack[begin..], &mut extra, &**metadata)?;
                    stack.truncate(begin);
                    stack.push(BorrowedStackNode::Generated(result));
                }
            }
        }
        assert_eq!(stack.len(), 1);
        Ok(stack.pop().unwrap())
    }
}

/// Generated loader function type. Intermediate outputs retain the existing
/// VectorValue ownership and representation.
pub type BorrowedFn = fn(
    &mut EvalContext,
    usize,
    &[BorrowedStackNode<'_>],
    &mut RpnFnCallExtra<'_>,
    &(dyn std::any::Any + Send),
) -> Result<VectorValue>;
