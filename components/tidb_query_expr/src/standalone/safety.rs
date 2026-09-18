// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Preflight for trusted kernels with value-sensitive panic preconditions.
use codec::prelude::NumberDecoder;
use tidb_query_datatype::FieldTypeAccessor;
use tipb::{Expr, ExprType, ScalarFuncSig};

use super::{Column, Error};

#[derive(Debug)]
pub(super) struct DigitColumn {
    offset: usize,
    unsigned: bool,
}

fn check_digit(value: i64, unsigned: bool) -> Result<(), Error> {
    if (unsigned && value as u64 > 308) || (!unsigned && !(-308..=308).contains(&value)) {
        return Err(Error::invalid(
            "ROUND/TRUNCATE fractional digits must be within -308..=308",
        ));
    }
    Ok(())
}

pub(super) fn collect_digit_columns(
    expr: &Expr,
    output: &mut Vec<DigitColumn>,
) -> Result<(), Error> {
    use ScalarFuncSig::*;
    if expr.get_tp() == ExprType::ScalarFunc
        && matches!(
            expr.get_sig(),
            RoundWithFracInt
                | RoundWithFracDec
                | RoundWithFracReal
                | TruncateInt
                | TruncateUint
                | TruncateReal
                | TruncateDecimal
        )
    {
        let digit = expr
            .get_children()
            .get(1)
            .ok_or_else(|| Error::invalid("missing fractional digit argument"))?;
        let unsigned = digit.get_field_type().is_unsigned();
        match digit.get_tp() {
            ExprType::Null => {}
            ExprType::Int64 => check_digit(
                digit
                    .get_val()
                    .read_i64()
                    .map_err(|e| Error::invalid(e.to_string()))?,
                unsigned,
            )?,
            ExprType::Uint64 => check_digit(
                digit
                    .get_val()
                    .read_u64()
                    .map_err(|e| Error::invalid(e.to_string()))? as i64,
                unsigned,
            )?,
            ExprType::ColumnRef => {
                // validate_expr already checked integer type, payload and schema offset.
                let offset = digit
                    .get_val()
                    .read_i64()
                    .map_err(|e| Error::invalid(e.to_string()))?
                    as usize;
                output.push(DigitColumn { offset, unsigned });
            }
            _ => {
                return Err(Error::invalid(
                    "ROUND/TRUNCATE fractional digits require an integer literal or input column",
                ));
            }
        }
    }
    for child in expr.get_children() {
        collect_digit_columns(child, output)?;
    }
    Ok(())
}

pub(super) fn validate_digit_columns(
    guards: &[DigitColumn],
    columns: &[Column],
    row_count: usize,
    selection: Option<&[usize]>,
) -> Result<(), Error> {
    for guard in guards {
        let Column::Int(values) = &columns[guard.offset] else {
            return Err(Error::invalid("fractional digit column must be Int"));
        };
        for logical in 0..selection.map_or(row_count, <[usize]>::len) {
            let physical = selection.map_or(logical, |rows| rows[logical]);
            if let Some(value) = values[physical] {
                check_digit(value, guard.unsigned)?;
            }
        }
    }
    Ok(())
}
