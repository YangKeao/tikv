// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Additive native borrowed ABI. Only descriptors/diagnostics are allocated
//! here; foreign values go through the original generated borrowed kernel
//! loaders.
use tidb_query_expr::standalone::{ColumnRef, Diagnostics, ScalarRef};

use super::*;

pub const BORROWED_ABI_VERSION: u32 = 1;
pub const UINT8: u32 = 4;
pub const UINT64: u32 = 5;
pub const BROADCAST: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BorrowedColumn {
    pub struct_size: u32,
    pub type_: u32,
    pub flags: u32,
    pub reserved: u32,
    pub len: usize,
    pub nulls: *const u8,
    pub nulls_len: usize,
    pub ints: *const i64,
    pub reals: *const f64,
    pub chars: *const u8,
    pub chars_len: usize,
    pub offsets: *const u64,
    pub offsets_len: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BorrowedOutput {
    pub struct_size: u32,
    pub type_: u32,
    pub capacity: usize,
    pub nulls: *mut u8,
    pub nulls_capacity: usize,
    pub ints: *mut i64,
    pub reals: *mut f64,
    pub uint8s: *mut u8,
    pub uint64s: *mut u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DiagnosticsView {
    pub warnings: *const WarningView,
    pub warnings_len: usize,
    pub warning_count: usize,
}

pub struct DiagnosticsHandle {
    diagnostics: Diagnostics,
    warnings: Vec<WarningView>,
}

impl DiagnosticsHandle {
    fn new(diagnostics: Diagnostics) -> Self {
        let warnings = diagnostics
            .warnings
            .iter()
            .map(|warning| WarningView {
                mysql_code: warning.code,
                message: Bytes::from_slice(warning.message.as_bytes()),
            })
            .collect();
        Self {
            diagnostics,
            warnings,
        }
    }
}

#[derive(Clone, Copy)]
struct Range {
    begin: usize,
    end: usize,
}
impl Range {
    fn of<T>(data: *const T, len: usize) -> Fallible<Self> {
        check_ptr(data, len)?;
        // Zero-length ranges never overlap, irrespective of their pointer.
        if len == 0 {
            return Ok(Self { begin: 0, end: 0 });
        }
        Ok(Self {
            begin: data as usize,
            end: data as usize + check_count::<T>(len)?,
        })
    }
    fn overlaps(self, other: Self) -> bool {
        self.begin < other.end && other.begin < self.end
    }
}

unsafe fn column<'a>(
    desc: &BorrowedColumn,
    rows: usize,
    expected: u32,
    ranges: &mut Vec<Range>,
) -> Fallible<ColumnRef<'a>> {
    let broadcast = desc.flags == BROADCAST;
    if desc.struct_size != size_of::<BorrowedColumn>() as u32
        || desc.type_ != expected
        || desc.flags & !BROADCAST != 0
        || desc.reserved != 0
        || desc.len != if broadcast { 1 } else { rows }
    {
        return Err(ErrorHandle::invalid(
            "borrowed column size/type/flags/length mismatch",
        ));
    }
    let nulls = if desc.nulls.is_null() {
        if desc.nulls_len != 0 {
            return Err(ErrorHandle::invalid("absent NULL map has nonzero length"));
        }
        None
    } else {
        if desc.nulls_len != desc.len {
            return Err(ErrorHandle::invalid(
                "NULL map length differs from stored rows",
            ));
        }
        ranges.push(Range::of(desc.nulls, desc.nulls_len)?);
        Some(foreign_slice(desc.nulls, desc.nulls_len)?)
    };
    match desc.type_ {
        INT64 | FLOAT64
            if desc.chars.is_null()
                && desc.chars_len == 0
                && desc.offsets.is_null()
                && desc.offsets_len == 0 =>
        {
            if desc.type_ == INT64 && desc.reals.is_null() {
                ranges.push(Range::of(desc.ints, desc.len)?);
                Ok(ColumnRef::NativeInt {
                    values: foreign_slice(desc.ints, desc.len)?,
                    nulls,
                    broadcast,
                })
            } else if desc.type_ == FLOAT64 && desc.ints.is_null() {
                ranges.push(Range::of(desc.reals, desc.len)?);
                Ok(ColumnRef::NativeReal {
                    values: foreign_slice(desc.reals, desc.len)?,
                    nulls,
                    broadcast,
                })
            } else {
                Err(ErrorHandle::invalid("unused numeric pointer is non-NULL"))
            }
        }
        BYTES if desc.ints.is_null() && desc.reals.is_null() => {
            if desc.offsets_len != desc.len {
                return Err(ErrorHandle::invalid(
                    "END offsets length differs from stored rows",
                ));
            }
            ranges.push(Range::of(desc.chars, desc.chars_len)?);
            ranges.push(Range::of(desc.offsets, desc.offsets_len)?);
            Ok(ColumnRef::NativeBytes {
                values: foreign_slice(desc.chars, desc.chars_len)?,
                offsets: foreign_slice(desc.offsets, desc.offsets_len)?,
                nulls,
                broadcast,
            })
        }
        _ => Err(ErrorHandle::invalid(
            "borrowed type or unused pointer/length invalid",
        )),
    }
}

fn output_ranges(output: &BorrowedOutput, rows: usize, expected: u32) -> Fallible<[Range; 2]> {
    if output.struct_size != size_of::<BorrowedOutput>() as u32
        || output.capacity < rows
        || output.nulls_capacity < rows
        || !matches!(
            (expected, output.type_),
            (INT64, INT64 | UINT8 | UINT64) | (FLOAT64, FLOAT64)
        )
    {
        return Err(ErrorHandle::invalid(
            "borrowed output size/type/capacity mismatch",
        ));
    }
    let numeric = match output.type_ {
        INT64 if output.reals.is_null() && output.uint8s.is_null() && output.uint64s.is_null() => {
            Range::of(output.ints, output.capacity)?
        }
        FLOAT64 if output.ints.is_null() && output.uint8s.is_null() && output.uint64s.is_null() => {
            Range::of(output.reals, output.capacity)?
        }
        UINT8 if output.ints.is_null() && output.reals.is_null() && output.uint64s.is_null() => {
            Range::of(output.uint8s, output.capacity)?
        }
        UINT64 if output.ints.is_null() && output.reals.is_null() && output.uint8s.is_null() => {
            Range::of(output.uint64s, output.capacity)?
        }
        _ => return Err(ErrorHandle::invalid("unused output pointer is non-NULL")),
    };
    let nulls = Range::of(output.nulls, output.nulls_capacity)?;
    if numeric.overlaps(nulls) {
        return Err(ErrorHandle::invalid("output buffers overlap"));
    }
    Ok([numeric, nulls])
}

fn supports(program: &Program) -> bool {
    !program.poisoned
        && matches!(program.output_type, INT64 | FLOAT64)
        && program.prepared.supports_borrowed()
}

#[no_mangle]
pub extern "C" fn tikv_expr_borrowed_abi_version() -> u32 {
    BORROWED_ABI_VERSION
}

#[no_mangle]
pub unsafe extern "C" fn tikv_expr_program_supports_borrowed(program: *const Program) -> u32 {
    guard(|| Ok(u32::from(supports(foreign_ref(program)?)))).unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn tikv_expr_eval_borrowed(
    program: *mut Program,
    columns: *const BorrowedColumn,
    columns_len: usize,
    row_count: usize,
    selection: *const Selection,
    output: *const BorrowedOutput,
    out: *mut *mut DiagnosticsHandle,
    out_error: *mut *mut ErrorHandle,
) -> u32 {
    boundary(out_error, || {
        initialize_handle(out)?;
        check_ptr(program, 1)?;
        let program_range = Range::of(program, 1)?;
        let program = &mut *program;
        if program.poisoned {
            return Err(ErrorHandle {
                status: POISONED,
                mysql_code: 0,
                message: "program is poisoned after an evaluation panic".to_owned(),
            });
        }
        let evaluated = guard(|| {
            #[cfg(test)]
            super::tests::maybe_inject_panic();
            if !supports(program) {
                return Err(ErrorHandle::invalid(
                    "program does not support numeric borrowed evaluation",
                ));
            }
            check_count::<usize>(row_count)?;
            if columns_len != program.schema_types.len() {
                return Err(ErrorHandle::invalid(
                    "column count differs from compiled schema",
                ));
            }
            let mut inputs = vec![
                program_range,
                Range::of(columns, columns_len)?,
                Range::of(output, 1)?,
                Range::of(out, 1)?,
            ];
            if !out_error.is_null() {
                inputs.push(Range::of(out_error, 1)?);
            }
            let selection = if selection.is_null() {
                None
            } else {
                inputs.push(Range::of(selection, 1)?);
                let selection = foreign_ref(selection)?;
                inputs.push(Range::of(selection.indices, selection.len)?);
                let indices = foreign_slice(selection.indices, selection.len)?;
                if indices.iter().any(|&row| row >= row_count) {
                    return Err(ErrorHandle::invalid("selection index out of bounds"));
                }
                Some(indices)
            };
            let rows = selection.map_or(row_count, <[usize]>::len);
            let output = *foreign_ref(output)?;
            let writes = output_ranges(&output, rows, program.output_type)?;
            let columns = foreign_slice(columns, columns_len)?
                .iter()
                .zip(&program.schema_types)
                .map(|(desc, &expected)| column(desc, row_count, expected, &mut inputs))
                .collect::<Fallible<Vec<_>>>()?;
            if writes
                .iter()
                .any(|write| inputs.iter().any(|input| write.overlaps(*input)))
            {
                return Err(ErrorHandle::invalid(
                    "output overlaps borrowed input or descriptor",
                ));
            }
            let mut index = 0;
            // eval_borrowed validates every layout/selected REAL before any sink
            // callback or kernel. Do not move writes above that validation.
            let diagnostics = program
                .prepared
                .eval_borrowed(&columns, row_count, selection, |value| {
                    let failure = || tidb_query_expr::standalone::Error {
                        code: 1105,
                        message: "borrowed output conversion out of range/type".to_owned(),
                    };
                    match (output.type_, value) {
                        (INT64, ScalarRef::Int(v)) => output.ints.add(index).write(v),
                        (FLOAT64, ScalarRef::Real(v)) => output.reals.add(index).write(v),
                        (UINT8, ScalarRef::Int(v @ 0..=1)) => {
                            output.uint8s.add(index).write(v as u8)
                        }
                        (UINT64, ScalarRef::Int(v)) if v >= 0 => {
                            output.uint64s.add(index).write(v as u64)
                        }
                        (INT64, ScalarRef::Null) => output.ints.add(index).write(0),
                        (FLOAT64, ScalarRef::Null) => output.reals.add(index).write(0.0),
                        (UINT8, ScalarRef::Null) => output.uint8s.add(index).write(0),
                        (UINT64, ScalarRef::Null) => output.uint64s.add(index).write(0),
                        _ => return Err(failure()),
                    }
                    output
                        .nulls
                        .add(index)
                        .write(u8::from(matches!(value, ScalarRef::Null)));
                    index += 1;
                    Ok(())
                })
                .map_err(|e| ErrorHandle::engine(EVAL_ERROR, e))?;
            Ok(DiagnosticsHandle::new(diagnostics))
        });
        if evaluated
            .as_ref()
            .err()
            .is_some_and(|error| error.status == PANIC)
        {
            program.poisoned = true;
        }
        out.write(Box::into_raw(Box::new(evaluated?)));
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn tikv_expr_diagnostics_get_view(
    diagnostics: *const DiagnosticsHandle,
    out: *mut DiagnosticsView,
) -> u32 {
    boundary(ptr::null_mut(), || {
        check_ptr(out, 1)?;
        out.write(DiagnosticsView {
            warnings: ptr::null(),
            warnings_len: 0,
            warning_count: 0,
        });
        let diagnostics = foreign_ref(diagnostics)?;
        out.write(DiagnosticsView {
            warnings: ptr_or_null(&diagnostics.warnings),
            warnings_len: diagnostics.warnings.len(),
            warning_count: diagnostics.diagnostics.warning_count,
        });
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn tikv_expr_diagnostics_free(diagnostics: *mut DiagnosticsHandle) {
    free_handle(diagnostics);
}
