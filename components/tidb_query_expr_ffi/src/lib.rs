// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Linux C ABI for the original **copying** standalone expression facade.
//!
//! The complete unsafe caller contract is in `include/tikv_expr.h`. Structural
//! checks do not prove arbitrary foreign pointers are accessible. No SQL
//! kernels live here: compile/eval delegate to the unchanged
//! `PreparedExpression`.
#![allow(clippy::missing_safety_doc)] // The shared C header specifies every entry point.

use std::{
    mem::{align_of, size_of},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr, slice, str,
};

use tidb_query_expr::standalone::{Column, Context, EvalOutput, PreparedExpression};
use tipb::{Expr, FieldType};

pub const ABI_VERSION: u32 = 1;
pub const OK: u32 = 0;
pub const INVALID_ARGUMENT: u32 = 1;
pub const COMPILE_ERROR: u32 = 2;
pub const EVAL_ERROR: u32 = 3;
pub const PANIC: u32 = 4;
pub const POISONED: u32 = 5;
pub const INT64: u32 = 1;
pub const FLOAT64: u32 = 2;
pub const BYTES: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Bytes {
    pub data: *const u8,
    pub len: usize,
}

impl Default for Bytes {
    fn default() -> Self {
        Self {
            data: ptr::null(),
            len: 0,
        }
    }
}

impl Bytes {
    fn from_slice(bytes: &[u8]) -> Self {
        Self {
            data: if bytes.is_empty() {
                ptr::null()
            } else {
                bytes.as_ptr()
            },
            len: bytes.len(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ContextDesc {
    pub struct_size: u32,
    pub abi_version: u32,
    pub flags: u64,
    pub sql_mode: u64,
    pub timezone_name: Bytes,
    pub timezone_offset: i64,
    pub div_precision_increment: u32,
    pub max_warning_count: u32,
}

impl Default for ContextDesc {
    fn default() -> Self {
        Self {
            struct_size: size_of::<Self>() as u32,
            abi_version: ABI_VERSION,
            flags: 0,
            sql_mode: 0,
            timezone_name: Bytes::default(),
            timezone_offset: 0,
            div_precision_increment: 4,
            max_warning_count: 64,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ColumnDesc {
    pub struct_size: u32,
    pub type_: u32,
    pub len: usize,
    pub nulls: *const u8,
    pub ints: *const i64,
    pub reals: *const f64,
    pub strings: *const Bytes,
}

impl Default for ColumnDesc {
    fn default() -> Self {
        Self {
            struct_size: 0,
            type_: 0,
            len: 0,
            nulls: ptr::null(),
            ints: ptr::null(),
            reals: ptr::null(),
            strings: ptr::null(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Selection {
    pub indices: *const usize,
    pub len: usize,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            indices: ptr::null(),
            len: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct WarningView {
    pub mysql_code: i32,
    pub message: Bytes,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ResultView {
    pub column: ColumnDesc,
    pub warnings: *const WarningView,
    pub warnings_len: usize,
    pub warning_count: usize,
}

impl Default for ResultView {
    fn default() -> Self {
        Self {
            column: ColumnDesc::default(),
            warnings: ptr::null(),
            warnings_len: 0,
            warning_count: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ErrorView {
    pub status: u32,
    pub mysql_code: i32,
    pub message: Bytes,
}

pub struct Program {
    prepared: PreparedExpression,
    schema_types: Vec<u32>,
    poisoned: bool,
}

#[derive(Debug)]
pub struct ErrorHandle {
    status: u32,
    mysql_code: i32,
    message: String,
}

type Fallible<T> = Result<T, ErrorHandle>;

impl ErrorHandle {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: INVALID_ARGUMENT,
            mysql_code: 0,
            message: message.into(),
        }
    }

    fn engine(status: u32, error: tidb_query_expr::standalone::Error) -> Self {
        Self {
            status,
            mysql_code: error.code,
            message: error.message,
        }
    }

    fn compile(message: impl Into<String>) -> Self {
        Self {
            status: COMPILE_ERROR,
            mysql_code: 1105,
            message: message.into(),
        }
    }
}

pub struct ResultHandle {
    // Own the original facade output. Byte/warning descriptors refer only to its
    // heap allocations, which stay stable when this owner moves into a Box.
    output: EvalOutput,
    nulls: Vec<u8>,
    ints: Vec<i64>,
    reals: Vec<f64>,
    strings: Vec<Bytes>,
    warnings: Vec<WarningView>,
}

impl ResultHandle {
    fn new(output: EvalOutput) -> Fallible<Self> {
        let mut result = Self {
            output,
            nulls: Vec::new(),
            ints: Vec::new(),
            reals: Vec::new(),
            strings: Vec::new(),
            warnings: Vec::new(),
        };
        match &result.output.column {
            Column::Int(values) => {
                for value in values {
                    result.nulls.push(u8::from(value.is_none()));
                    result.ints.push(value.unwrap_or_default());
                }
            }
            Column::Real(values) => {
                for value in values {
                    result.nulls.push(u8::from(value.is_none()));
                    result.reals.push(value.unwrap_or_default());
                }
            }
            Column::Bytes(values) => {
                for value in values {
                    result.nulls.push(u8::from(value.is_none()));
                    result.strings.push(
                        value
                            .as_ref()
                            .map_or_else(Bytes::default, |v| Bytes::from_slice(v)),
                    );
                }
            }
            Column::Decimal(_) => return Err(ErrorHandle::invalid("unexpected decimal output")),
        }
        result.warnings = result
            .output
            .warnings
            .iter()
            .map(|warning| WarningView {
                mysql_code: warning.code,
                message: Bytes::from_slice(warning.message.as_bytes()),
            })
            .collect();
        Ok(result)
    }

    fn view(&self) -> ResultView {
        let type_ = match &self.output.column {
            Column::Int(_) => INT64,
            Column::Real(_) => FLOAT64,
            Column::Bytes(_) => BYTES,
            Column::Decimal(_) => unreachable!("ABI admission excludes decimal"),
        };
        ResultView {
            column: ColumnDesc {
                struct_size: size_of::<ColumnDesc>() as u32,
                type_,
                len: self.nulls.len(),
                nulls: ptr_or_null(&self.nulls),
                ints: ptr_or_null(&self.ints),
                reals: ptr_or_null(&self.reals),
                strings: ptr_or_null(&self.strings),
            },
            warnings: ptr_or_null(&self.warnings),
            warnings_len: self.warnings.len(),
            warning_count: self.output.warning_count,
        }
    }
}

fn ptr_or_null<T>(values: &[T]) -> *const T {
    if values.is_empty() {
        ptr::null()
    } else {
        values.as_ptr()
    }
}

fn check_count<T>(len: usize) -> Fallible<usize> {
    len.checked_mul(size_of::<T>())
        .filter(|&bytes| bytes <= isize::MAX as usize)
        .ok_or_else(|| ErrorHandle::invalid("buffer size exceeds addressable range"))
}

fn check_ptr<T>(data: *const T, len: usize) -> Fallible<()> {
    let bytes = check_count::<T>(len)?;
    if len == 0 {
        return Ok(());
    }
    if data.is_null() || !(data as usize).is_multiple_of(align_of::<T>()) {
        return Err(ErrorHandle::invalid(
            "required pointer is null or misaligned",
        ));
    }
    if (data as usize).checked_add(bytes).is_none() {
        return Err(ErrorHandle::invalid("buffer address range wraps"));
    }
    Ok(())
}

// SAFETY: caller ensures allocation validity, initialization and call lifetime;
// check_ptr only validates shape, not the existence/provenance of an
// allocation.
unsafe fn foreign_slice<'a, T>(data: *const T, len: usize) -> Fallible<&'a [T]> {
    check_ptr(data, len)?;
    if len == 0 {
        Ok(&[])
    } else {
        Ok(slice::from_raw_parts(data, len))
    }
}

unsafe fn foreign_ref<'a, T>(data: *const T) -> Fallible<&'a T> {
    check_ptr(data, 1)?;
    Ok(&*data)
}

unsafe fn initialize_handle<T>(out: *mut *mut T) -> Fallible<()> {
    check_ptr(out, 1)?;
    out.write(ptr::null_mut());
    Ok(())
}

fn guard<T>(action: impl FnOnce() -> Fallible<T>) -> Fallible<T> {
    match catch_unwind(AssertUnwindSafe(action)) {
        Ok(result) => result,
        Err(payload) => {
            // A foreign panic payload may itself have a panicking destructor.
            // Leaking this exceptional payload is preferable to unwinding over C.
            std::mem::forget(payload);
            Err(ErrorHandle {
                status: PANIC,
                mysql_code: 0,
                message: "Rust panic contained at expression ABI".to_owned(),
            })
        }
    }
}

unsafe fn boundary(out_error: *mut *mut ErrorHandle, action: impl FnOnce() -> Fallible<()>) -> u32 {
    if !out_error.is_null() {
        if check_ptr(out_error, 1).is_err() {
            return INVALID_ARGUMENT;
        }
        out_error.write(ptr::null_mut());
    }
    match guard(action) {
        Ok(()) => OK,
        Err(error) => {
            let status = error.status;
            if !out_error.is_null() {
                out_error.write(Box::into_raw(Box::new(error)));
            }
            status
        }
    }
}

unsafe fn context_from_desc(desc: *const ContextDesc) -> Fallible<Context> {
    let desc = foreign_ref(desc)?;
    if desc.struct_size != size_of::<ContextDesc>() as u32 || desc.abi_version != ABI_VERSION {
        return Err(ErrorHandle::invalid("context size or ABI version mismatch"));
    }
    if desc.div_precision_increment > 30 || desc.max_warning_count > 65535 {
        return Err(ErrorHandle::invalid(
            "division precision or warning cap out of range",
        ));
    }
    let name = foreign_slice(desc.timezone_name.data, desc.timezone_name.len)?;
    let name =
        str::from_utf8(name).map_err(|_| ErrorHandle::invalid("timezone name must be UTF-8"))?;
    Ok(Context {
        flags: desc.flags,
        sql_mode: desc.sql_mode,
        time_zone_name: if name.is_empty() {
            None
        } else {
            Some(name.to_owned())
        },
        time_zone_offset: desc.timezone_offset,
        div_precision_increment: desc.div_precision_increment as u8,
        max_warning_count: desc.max_warning_count as usize,
    })
}

// ABI admission only, not a second expression validator/evaluator. The original
// facade remains authoritative for presence bits, signatures, arity and
// metadata.
fn abi_type(field: &FieldType) -> Fallible<u32> {
    match field.get_tp() {
        1 | 2 | 3 | 8 | 9 | 13 => Ok(INT64),
        4 | 5 => Ok(FLOAT64),
        15 | 249..=254 => Ok(BYTES),
        _ => Err(ErrorHandle::compile(
            "C ABI supports only Int64, Float64 and Bytes metadata",
        )),
    }
}

fn admit_tree(expr: &Expr, depth: usize) -> Fallible<()> {
    if depth > 64 {
        return Err(ErrorHandle::compile("expression is too deep"));
    }
    abi_type(expr.get_field_type())?;
    for child in expr.get_children() {
        admit_tree(child, depth + 1)?;
    }
    Ok(())
}

unsafe fn validate_column(desc: &ColumnDesc, rows: usize, type_: u32) -> Fallible<&[u8]> {
    if desc.struct_size != size_of::<ColumnDesc>() as u32 || desc.len != rows || desc.type_ != type_
    {
        return Err(ErrorHandle::invalid(
            "column size, type or row count mismatch",
        ));
    }
    let nulls = if desc.nulls.is_null() {
        &[]
    } else {
        foreign_slice(desc.nulls, rows)?
    };
    if nulls.iter().any(|&null| null > 1) {
        return Err(ErrorHandle::invalid("NULL map must contain only 0 or 1"));
    }
    match type_ {
        INT64 if desc.reals.is_null() && desc.strings.is_null() => check_ptr(desc.ints, rows)?,
        FLOAT64 if desc.ints.is_null() && desc.strings.is_null() => check_ptr(desc.reals, rows)?,
        BYTES if desc.ints.is_null() && desc.reals.is_null() => {
            let strings = foreign_slice(desc.strings, rows)?;
            for (row, value) in strings.iter().enumerate() {
                if nulls.is_empty() || nulls[row] == 0 {
                    // Check all non-NULL row shapes without reading their payload.
                    check_ptr(value.data, value.len)?;
                }
            }
        }
        _ => {
            return Err(ErrorHandle::invalid(
                "unknown column type or non-NULL unused pointer",
            ));
        }
    }
    Ok(nulls)
}

unsafe fn gather_column(desc: &ColumnDesc, rows: &[usize], nulls: &[u8]) -> Fallible<Column> {
    let valid = |row: usize| nulls.is_empty() || nulls[row] == 0;
    // Pointer ranges were checked for every physical row. Only selected payload
    // values are read. This is an owned gather, not a borrowed engine input.
    match desc.type_ {
        INT64 => {
            check_count::<Option<i64>>(rows.len())?;
            Ok(Column::Int(
                rows.iter()
                    .map(|&row| {
                        if valid(row) {
                            Some(*desc.ints.add(row))
                        } else {
                            None
                        }
                    })
                    .collect(),
            ))
        }
        FLOAT64 => {
            check_count::<Option<f64>>(rows.len())?;
            Ok(Column::Real(
                rows.iter()
                    .map(|&row| {
                        if valid(row) {
                            Some(*desc.reals.add(row))
                        } else {
                            None
                        }
                    })
                    .collect(),
            ))
        }
        BYTES => {
            check_count::<Option<Vec<u8>>>(rows.len())?;
            let mut values = Vec::with_capacity(rows.len());
            for &row in rows {
                values.push(if valid(row) {
                    let bytes = &*desc.strings.add(row);
                    Some(foreign_slice(bytes.data, bytes.len)?.to_vec())
                } else {
                    None
                });
            }
            Ok(Column::Bytes(values))
        }
        _ => Err(ErrorHandle::invalid("unknown column type")),
    }
}

#[no_mangle]
pub extern "C" fn tikv_expr_abi_version() -> u32 {
    ABI_VERSION
}

#[no_mangle]
pub unsafe extern "C" fn tikv_expr_context_default(out: *mut ContextDesc) -> u32 {
    boundary(ptr::null_mut(), || {
        check_ptr(out, 1)?;
        out.write(ContextDesc::default());
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn tikv_expr_compile(
    expr: Bytes,
    schema: *const Bytes,
    schema_len: usize,
    context: *const ContextDesc,
    out: *mut *mut Program,
    out_error: *mut *mut ErrorHandle,
) -> u32 {
    boundary(out_error, || {
        initialize_handle(out)?;
        let context = context_from_desc(context)?;
        let expr = foreign_slice(expr.data, expr.len)?;
        let schema = foreign_slice(schema, schema_len)?;
        check_count::<Vec<u8>>(schema_len)?;
        let schema: Vec<Vec<u8>> = schema
            .iter()
            .map(|bytes| Ok(foreign_slice(bytes.data, bytes.len)?.to_vec()))
            .collect::<Fallible<_>>()?;
        let tree = protobuf::parse_from_bytes::<Expr>(expr)
            .map_err(|e| ErrorHandle::compile(e.to_string()))?;
        admit_tree(&tree, 0)?;
        let schema_types = schema
            .iter()
            .map(|bytes| {
                let field = protobuf::parse_from_bytes::<FieldType>(bytes)
                    .map_err(|e| ErrorHandle::compile(e.to_string()))?;
                abi_type(&field)
            })
            .collect::<Fallible<Vec<_>>>()?;
        let prepared = PreparedExpression::compile(expr, &schema, context)
            .map_err(|e| ErrorHandle::engine(COMPILE_ERROR, e))?;
        out.write(Box::into_raw(Box::new(Program {
            prepared,
            schema_types,
            poisoned: false,
        })));
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn tikv_expr_eval(
    program: *mut Program,
    columns: *const ColumnDesc,
    columns_len: usize,
    row_count: usize,
    selection: *const Selection,
    out: *mut *mut ResultHandle,
    out_error: *mut *mut ErrorHandle,
) -> u32 {
    boundary(out_error, || {
        initialize_handle(out)?;
        check_ptr(program, 1)?;
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
            tests::maybe_inject_panic();
            check_count::<usize>(row_count)?;
            if columns_len != program.schema_types.len() {
                return Err(ErrorHandle::invalid(
                    "column count differs from compiled schema",
                ));
            }
            let columns = foreign_slice(columns, columns_len)?;
            let null_maps = columns
                .iter()
                .zip(&program.schema_types)
                .map(|(column, &type_)| validate_column(column, row_count, type_))
                .collect::<Fallible<Vec<_>>>()?;
            let rows = if selection.is_null() {
                // Include the largest owned row representation even for a
                // zero-column constant: there is no input gather to check it.
                check_count::<Option<Vec<u8>>>(row_count)?;
                (0..row_count).collect::<Vec<_>>()
            } else {
                let selection = foreign_ref(selection)?;
                check_count::<Option<Vec<u8>>>(selection.len)?;
                let rows = foreign_slice(selection.indices, selection.len)?;
                if rows.iter().any(|&row| row >= row_count) {
                    return Err(ErrorHandle::invalid("selection index out of bounds"));
                }
                rows.to_vec()
            };
            let owned = columns
                .iter()
                .zip(null_maps)
                .map(|(column, nulls)| gather_column(column, &rows, nulls))
                .collect::<Fallible<Vec<_>>>()?;
            let output = program
                .prepared
                .eval(&owned, rows.len(), None)
                .map_err(|e| ErrorHandle::engine(EVAL_ERROR, e))?;
            ResultHandle::new(output)
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
pub unsafe extern "C" fn tikv_expr_result_get_view(
    result: *const ResultHandle,
    out: *mut ResultView,
) -> u32 {
    boundary(ptr::null_mut(), || {
        check_ptr(out, 1)?;
        out.write(ResultView::default());
        out.write(foreign_ref(result)?.view());
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn tikv_expr_error_get_view(
    error: *const ErrorHandle,
    out: *mut ErrorView,
) -> u32 {
    boundary(ptr::null_mut(), || {
        check_ptr(out, 1)?;
        out.write(ErrorView::default());
        let error = foreign_ref(error)?;
        out.write(ErrorView {
            status: error.status,
            mysql_code: error.mysql_code,
            message: Bytes::from_slice(error.message.as_bytes()),
        });
        Ok(())
    })
}

unsafe fn free_handle<T>(handle: *mut T) {
    if !handle.is_null() {
        let _ = guard(|| {
            drop(Box::from_raw(handle));
            Ok(())
        });
    }
}

#[no_mangle]
pub unsafe extern "C" fn tikv_expr_program_free(program: *mut Program) {
    free_handle(program);
}

#[no_mangle]
pub unsafe extern "C" fn tikv_expr_result_free(result: *mut ResultHandle) {
    free_handle(result);
}

#[no_mangle]
pub unsafe extern "C" fn tikv_expr_error_free(error: *mut ErrorHandle) {
    free_handle(error);
}

#[cfg(test)]
mod tests;
