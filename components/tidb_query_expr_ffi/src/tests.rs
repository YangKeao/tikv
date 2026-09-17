// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use std::cell::Cell;

use protobuf::Message;
use tidb_query_datatype::FieldTypeTp;
use tipb::ScalarFuncSig;
use tipb_helper::ExprDefBuilder as E;

use super::*;

thread_local! { static INJECT_PANIC: Cell<bool> = const { Cell::new(false) }; }

pub(super) fn maybe_inject_panic() {
    INJECT_PANIC.with(|flag| {
        if flag.replace(false) {
            panic!("injected ABI evaluation panic");
        }
    });
}

struct ProgramOwner(*mut Program);
impl Drop for ProgramOwner {
    fn drop(&mut self) {
        unsafe { tikv_expr_program_free(self.0) }
    }
}
struct ResultOwner(*mut ResultHandle);
impl Drop for ResultOwner {
    fn drop(&mut self) {
        unsafe { tikv_expr_result_free(self.0) }
    }
}

unsafe fn error_info(error: *mut ErrorHandle) -> (u32, i32, String) {
    let mut view = ErrorView::default();
    assert_eq!(tikv_expr_error_get_view(error, &mut view), OK);
    let message = str::from_utf8(foreign_slice(view.message.data, view.message.len).unwrap())
        .unwrap()
        .to_owned();
    tikv_expr_error_free(error);
    (view.status, view.mysql_code, message)
}

fn compile(expr: impl Into<Expr>, fields: &[FieldType], context: ContextDesc) -> ProgramOwner {
    let expr = expr.into().write_to_bytes().unwrap();
    let fields: Vec<_> = fields
        .iter()
        .map(|field| field.write_to_bytes().unwrap())
        .collect();
    let schema: Vec<_> = fields
        .iter()
        .map(|bytes| Bytes::from_slice(bytes))
        .collect();
    let mut program = ptr::null_mut();
    let mut error = ptr::null_mut();
    let status = unsafe {
        tikv_expr_compile(
            Bytes::from_slice(&expr),
            schema.as_ptr(),
            schema.len(),
            &context,
            &mut program,
            &mut error,
        )
    };
    if status != OK {
        panic!("compile: {:?}", unsafe { error_info(error) });
    }
    assert!(error.is_null());
    ProgramOwner(program)
}

fn eval(
    program: &ProgramOwner,
    columns: &[ColumnDesc],
    rows: usize,
    selection: Option<&Selection>,
) -> ResultOwner {
    let mut result = ptr::null_mut();
    let mut error = ptr::null_mut();
    let status = unsafe {
        tikv_expr_eval(
            program.0,
            columns.as_ptr(),
            columns.len(),
            rows,
            selection.map_or(ptr::null(), |s| s),
            &mut result,
            &mut error,
        )
    };
    if status != OK {
        panic!("eval: {:?}", unsafe { error_info(error) });
    }
    assert!(error.is_null());
    ResultOwner(result)
}

fn int_column(values: &[i64], nulls: Option<&[u8]>) -> ColumnDesc {
    ColumnDesc {
        struct_size: size_of::<ColumnDesc>() as u32,
        type_: INT64,
        len: values.len(),
        ints: values.as_ptr(),
        nulls: nulls.map_or(ptr::null(), |n| n.as_ptr()),
        ..ColumnDesc::default()
    }
}
fn real_column(values: &[f64]) -> ColumnDesc {
    ColumnDesc {
        struct_size: size_of::<ColumnDesc>() as u32,
        type_: FLOAT64,
        len: values.len(),
        reals: values.as_ptr(),
        ..ColumnDesc::default()
    }
}
fn bytes_column(values: &[Bytes], nulls: Option<&[u8]>) -> ColumnDesc {
    ColumnDesc {
        struct_size: size_of::<ColumnDesc>() as u32,
        type_: BYTES,
        len: values.len(),
        strings: values.as_ptr(),
        nulls: nulls.map_or(ptr::null(), |n| n.as_ptr()),
        ..ColumnDesc::default()
    }
}
fn view(result: &ResultOwner) -> ResultView {
    let mut view = ResultView::default();
    assert_eq!(
        unsafe { tikv_expr_result_get_view(result.0, &mut view) },
        OK
    );
    view
}
fn values(result: &ResultOwner) -> Column {
    let desc = view(result).column;
    let indices: Vec<_> = (0..desc.len).collect();
    unsafe {
        let nulls = validate_column(&desc, desc.len, desc.type_).unwrap();
        gather_column(&desc, &indices, nulls).unwrap()
    }
}

fn eval_error(
    program: &ProgramOwner,
    columns: &[ColumnDesc],
    rows: usize,
    selection: Option<&Selection>,
) -> (u32, i32, String) {
    let mut result = ptr::dangling_mut();
    let mut error = ptr::null_mut();
    let status = unsafe {
        tikv_expr_eval(
            program.0,
            columns.as_ptr(),
            columns.len(),
            rows,
            selection.map_or(ptr::null(), |s| s),
            &mut result,
            &mut error,
        )
    };
    assert_ne!(status, OK);
    assert!(result.is_null());
    let info = unsafe { error_info(error) };
    assert_eq!(info.0, status);
    info
}

#[test]
fn version_defaults_and_layout() {
    assert_eq!(tikv_expr_abi_version(), 1);
    let mut context = ContextDesc::default();
    context.flags = 123;
    assert_eq!(unsafe { tikv_expr_context_default(&mut context) }, OK);
    assert_eq!(
        (context.flags, context.sql_mode, context.timezone_offset),
        (0, 0, 0)
    );
    assert_eq!(
        (context.div_precision_increment, context.max_warning_count),
        (4, 64)
    );
    assert_eq!(context.struct_size as usize, size_of::<ContextDesc>());
    // These match the native 64-bit C/C++ header and catch accidental field drift.
    if size_of::<usize>() == 8 {
        assert_eq!(size_of::<Bytes>(), 16);
        assert_eq!(size_of::<ContextDesc>(), 56);
        assert_eq!(size_of::<ColumnDesc>(), 48);
        assert_eq!(size_of::<Selection>(), 16);
        assert_eq!(size_of::<WarningView>(), 24);
        assert_eq!(size_of::<ResultView>(), 72);
        assert_eq!(size_of::<ErrorView>(), 24);
    }
    unsafe {
        assert_eq!(tikv_expr_context_default(ptr::null_mut()), INVALID_ARGUMENT);
        tikv_expr_program_free(ptr::null_mut());
        tikv_expr_result_free(ptr::null_mut());
        tikv_expr_error_free(ptr::null_mut());
    }
}

#[test]
fn context_fields_are_forwarded_without_narrowing() {
    let desc = ContextDesc {
        flags: u64::MAX,
        sql_mode: u64::MAX,
        timezone_name: Bytes::from_slice(b"UTC"),
        timezone_offset: i64::MAX, // A nonempty valid name takes precedence.
        div_precision_increment: 30,
        max_warning_count: 65535,
        ..ContextDesc::default()
    };
    let context = unsafe { context_from_desc(&desc).unwrap() };
    assert_eq!(context.flags, u64::MAX);
    assert_eq!(context.sql_mode, u64::MAX);
    assert_eq!(context.time_zone_name.as_deref(), Some("UTC"));
    assert_eq!(context.time_zone_offset, i64::MAX);
    assert_eq!(context.div_precision_increment, 30);
    assert_eq!(context.max_warning_count, 65535);
    let program = compile(E::constant_int(7), &[], desc);
    assert_eq!(
        values(&eval(&program, &[], 1, None)),
        Column::Int(vec![Some(7)])
    );
}

#[test]
fn copying_nullable_selected_repeated_and_result_independence() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let program = compile(
        E::scalar_func(ScalarFuncSig::AbsInt, ft.clone()).push_child(
            E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
                .push_child(E::column_ref(0, ft.clone()))
                .push_child(E::constant_int(-5)),
        ),
        &[ft],
        ContextDesc::default(),
    );
    let mut input = vec![2, 99, 9, -3];
    let nulls = [0, 1, 0, 0];
    let indices = [3, 1, 0, 3];
    let selection = Selection {
        indices: indices.as_ptr(),
        len: indices.len(),
    };
    let result = eval(
        &program,
        &[int_column(&input, Some(&nulls))],
        input.len(),
        Some(&selection),
    );
    input.fill(999); // No result pointer refers to caller storage.
    drop(input);
    let second = eval(&program, &[int_column(&[6], None)], 1, None);
    drop(program); // Results also outlive the compiled program and later calls.
    assert_eq!(
        values(&result),
        Column::Int(vec![Some(8), None, Some(3), Some(8)])
    );
    assert_eq!(values(&second), Column::Int(vec![Some(1)]));
}

#[test]
fn binary_slices_null_empty_and_copied_ownership() {
    let ft: FieldType = FieldTypeTp::VarString.into();
    let program = compile(
        E::scalar_func(ScalarFuncSig::Concat, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::column_ref(0, ft.clone())),
        &[ft],
        ContextDesc::default(),
    );
    let result = {
        let payload = vec![b'a', 0, 0xff];
        let strings = [
            Bytes::from_slice(&payload),
            Bytes::default(),
            Bytes::default(),
        ];
        let nulls = [0, 0, 1];
        let indices = [2, 0, 1, 0];
        eval(
            &program,
            &[bytes_column(&strings, Some(&nulls))],
            3,
            Some(&Selection {
                indices: indices.as_ptr(),
                len: 4,
            }),
        )
    };
    assert_eq!(
        values(&result),
        Column::Bytes(vec![
            None,
            Some(vec![b'a', 0, 0xff, b'a', 0, 0xff]),
            Some(vec![]),
            Some(vec![b'a', 0, 0xff, b'a', 0, 0xff])
        ])
    );
}

#[test]
fn selected_reals_ignore_unselected_nonfinite() {
    let ft: FieldType = FieldTypeTp::Double.into();
    let program = compile(
        E::scalar_func(ScalarFuncSig::MultiplyReal, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_real(2.0)),
        &[ft],
        ContextDesc::default(),
    );
    let input = [f64::INFINITY, 2.5, f64::NAN];
    let columns = [real_column(&input)];
    let indices = [1, 1];
    assert_eq!(
        values(&eval(
            &program,
            &columns,
            3,
            Some(&Selection {
                indices: indices.as_ptr(),
                len: 2
            })
        )),
        Column::Real(vec![Some(5.0); 2])
    );
    let error = eval_error(&program, &columns, 3, None);
    assert_eq!((error.0, error.1), (EVAL_ERROR, 1105));
    assert!(error.2.contains("nonfinite"));
    assert_eq!(
        values(&eval(&program, &columns, 3, Some(&Selection::default()))),
        Column::Real(vec![])
    );
}

#[test]
fn none_vs_empty_selection_zero_columns_and_split_batches() {
    let program = compile(E::constant_int(42), &[], ContextDesc::default());
    let rows = 2053;
    assert_eq!(
        values(&eval(&program, &[], rows, None)),
        Column::Int(vec![Some(42); rows])
    );
    assert_eq!(
        values(&eval(&program, &[], rows, Some(&Selection::default()))),
        Column::Int(vec![])
    );
    assert_eq!(values(&eval(&program, &[], 0, None)), Column::Int(vec![]));
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let identity = compile(E::column_ref(0, ft.clone()), &[ft], ContextDesc::default());
    let empty = ColumnDesc {
        struct_size: size_of::<ColumnDesc>() as u32,
        type_: INT64,
        ..ColumnDesc::default()
    };
    assert_eq!(
        values(&eval(&identity, &[empty], 0, None)),
        Column::Int(vec![])
    );
    let input: Vec<_> = (0..rows as i64).collect();
    let indices: Vec<_> = (0..rows).rev().collect();
    assert_eq!(
        values(&eval(
            &identity,
            &[int_column(&input, None)],
            rows,
            Some(&Selection {
                indices: indices.as_ptr(),
                len: rows
            })
        )),
        Column::Int(input.into_iter().rev().map(Some).collect())
    );
}

#[test]
fn zero_column_output_capacity_overflow_is_rejected_before_allocation() {
    let program = compile(E::constant_int(42), &[], ContextDesc::default());
    let impossible = isize::MAX as usize / size_of::<Option<Vec<u8>>>() + 1;
    check_count::<usize>(impossible).unwrap();
    assert_eq!(
        eval_error(&program, &[], impossible, None).0,
        INVALID_ARGUMENT
    );
    // A large physical count with no logical rows needs no output allocation.
    assert_eq!(
        values(&eval(
            &program,
            &[],
            impossible,
            Some(&Selection::default())
        )),
        Column::Int(vec![])
    );
    assert_eq!(
        values(&eval(&program, &[], 1, None)),
        Column::Int(vec![Some(42)])
    );
}

#[test]
fn diagnostics_match_original_facade_across_batches() {
    let ft: FieldType = FieldTypeTp::Double.into();
    let expr = E::scalar_func(ScalarFuncSig::DivideReal, ft.clone())
        .push_child(E::column_ref(0, ft.clone()))
        .push_child(E::constant_real(0.0))
        .build();
    for cap in [0, 2] {
        let context = ContextDesc {
            flags: 1 << 5,
            max_warning_count: cap,
            ..ContextDesc::default()
        };
        let program = compile(expr.clone(), slice::from_ref(&ft), context);
        let input = vec![1.0; 2053];
        let result = eval(&program, &[real_column(&input)], input.len(), None);
        let mut native = PreparedExpression::compile(
            &expr.write_to_bytes().unwrap(),
            &[ft.write_to_bytes().unwrap()],
            Context {
                flags: 1 << 5,
                max_warning_count: cap as usize,
                ..Context::default()
            },
        )
        .unwrap();
        let expected = native
            .eval(
                &[Column::Real(vec![Some(1.0); input.len()])],
                input.len(),
                None,
            )
            .unwrap();
        assert_eq!(values(&result), expected.column);
        let actual_view = view(&result);
        assert_eq!(actual_view.warning_count, expected.warning_count);
        assert_eq!(actual_view.warnings_len, expected.warnings.len());
        assert_eq!(actual_view.warning_count, input.len());
        let warnings =
            unsafe { foreign_slice(actual_view.warnings, actual_view.warnings_len).unwrap() };
        for (actual, expected) in warnings.iter().zip(&expected.warnings) {
            assert_eq!(actual.mysql_code, expected.code);
            assert_eq!(
                unsafe { foreign_slice(actual.message.data, actual.message.len).unwrap() },
                expected.message.as_bytes()
            );
        }
        assert_eq!(
            view(&eval(&program, &[real_column(&[])], 0, None)).warning_count,
            0
        );
    }
}

#[test]
fn native_error_retained_and_not_poisoned() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let program = compile(
        E::scalar_func(ScalarFuncSig::AbsInt, ft.clone()).push_child(E::column_ref(0, ft.clone())),
        &[ft],
        ContextDesc::default(),
    );
    let error = eval_error(&program, &[int_column(&[i64::MIN], None)], 1, None);
    assert_eq!((error.0, error.1), (EVAL_ERROR, 1690));
    assert!(!error.2.is_empty());
    assert_eq!(
        values(&eval(&program, &[int_column(&[-2], None)], 1, None)),
        Column::Int(vec![Some(2)])
    );
}

#[test]
fn structural_validation_without_dereferencing_invalid_memory() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let program = compile(E::column_ref(0, ft.clone()), &[ft], ContextDesc::default());
    let values = [1, 2];
    let good = int_column(&values, None);
    let mut bad = good;
    bad.ints = ptr::null();
    assert_eq!(eval_error(&program, &[bad], 2, None).0, INVALID_ARGUMENT);
    let misaligned = values.as_ptr().wrapping_byte_add(1);
    bad.ints = misaligned; // Rejected by alignment before any read.
    assert_eq!(eval_error(&program, &[bad], 2, None).0, INVALID_ARGUMENT);
    bad = good;
    bad.struct_size -= 1;
    assert_eq!(eval_error(&program, &[bad], 2, None).0, INVALID_ARGUMENT);
    assert_eq!(eval_error(&program, &[good], 1, None).0, INVALID_ARGUMENT);
    assert_eq!(eval_error(&program, &[], 2, None).0, INVALID_ARGUMENT);
    bad = good;
    bad.nulls = [0, 2].as_ptr();
    assert_eq!(eval_error(&program, &[bad], 2, None).0, INVALID_ARGUMENT);
    bad = good;
    let real = [1.0, 2.0];
    bad.reals = real.as_ptr();
    assert_eq!(eval_error(&program, &[bad], 2, None).0, INVALID_ARGUMENT);
    let index = [2];
    assert_eq!(
        eval_error(
            &program,
            &[good],
            2,
            Some(&Selection {
                indices: index.as_ptr(),
                len: 1
            })
        )
        .0,
        INVALID_ARGUMENT
    );
    assert_eq!(
        eval_error(
            &program,
            &[good],
            2,
            Some(&Selection {
                indices: ptr::null(),
                len: 1
            })
        )
        .0,
        INVALID_ARGUMENT
    );
    assert_eq!(
        eval_error(&program, &[good], usize::MAX, None).0,
        INVALID_ARGUMENT
    );
    check_ptr::<u64>(ptr::dangling(), usize::MAX).unwrap_err();
    check_ptr::<u8>(usize::MAX as *const u8, 2).unwrap_err();
    check_ptr::<i64>(ptr::null(), 0).unwrap();
    check_ptr::<i64>(misaligned, 0).unwrap();
}

#[test]
fn bytes_shape_checked_even_for_empty_selection() {
    let ft: FieldType = FieldTypeTp::VarString.into();
    let program = compile(E::column_ref(0, ft.clone()), &[ft], ContextDesc::default());
    let strings = [Bytes {
        data: ptr::null(),
        len: 3,
    }];
    assert_eq!(
        eval_error(
            &program,
            &[bytes_column(&strings, None)],
            1,
            Some(&Selection::default())
        )
        .0,
        INVALID_ARGUMENT
    );
    // NULL string payload is ignored; its descriptor is still a live array slot.
    assert_eq!(
        values(&eval(
            &program,
            &[bytes_column(&strings, Some(&[1]))],
            1,
            None
        )),
        Column::Bytes(vec![None])
    );
}

#[test]
fn malformed_compile_context_and_unsupported_metadata() {
    unsafe fn rejected(expr: &[u8], context: ContextDesc) -> (u32, i32, String) {
        let mut program = ptr::dangling_mut();
        let mut error = ptr::null_mut();
        assert_ne!(
            tikv_expr_compile(
                Bytes::from_slice(expr),
                ptr::null(),
                0,
                &context,
                &mut program,
                &mut error
            ),
            OK
        );
        assert!(program.is_null());
        error_info(error)
    }
    let expr = E::constant_int(1).build().write_to_bytes().unwrap();
    unsafe {
        assert_eq!(rejected(&[0xff], ContextDesc::default()).0, COMPILE_ERROR);
        assert_eq!(
            rejected(
                &expr,
                ContextDesc {
                    abi_version: 99,
                    ..ContextDesc::default()
                }
            )
            .0,
            INVALID_ARGUMENT
        );
        assert_eq!(
            rejected(
                &expr,
                ContextDesc {
                    struct_size: 1,
                    ..ContextDesc::default()
                }
            )
            .0,
            INVALID_ARGUMENT
        );
        assert_eq!(
            rejected(
                &expr,
                ContextDesc {
                    div_precision_increment: 31,
                    ..ContextDesc::default()
                }
            )
            .0,
            INVALID_ARGUMENT
        );
        assert_eq!(
            rejected(
                &expr,
                ContextDesc {
                    max_warning_count: 65536,
                    ..ContextDesc::default()
                }
            )
            .0,
            INVALID_ARGUMENT
        );
        assert_eq!(
            rejected(
                &expr,
                ContextDesc {
                    timezone_name: Bytes::from_slice(&[0xff]),
                    ..ContextDesc::default()
                }
            )
            .0,
            INVALID_ARGUMENT
        );
        assert_eq!(
            rejected(
                &expr,
                ContextDesc {
                    timezone_offset: i64::MAX,
                    ..ContextDesc::default()
                }
            )
            .0,
            COMPILE_ERROR
        );
        let mut decimal = E::constant_int(1).build();
        decimal.mut_field_type().set_tp(246);
        assert_eq!(
            rejected(&decimal.write_to_bytes().unwrap(), ContextDesc::default()).0,
            COMPILE_ERROR
        );
        let mut out = ptr::dangling_mut();
        assert_eq!(
            tikv_expr_compile(
                Bytes {
                    data: ptr::null(),
                    len: 1
                },
                ptr::null(),
                0,
                &ContextDesc::default(),
                &mut out,
                ptr::null_mut()
            ),
            INVALID_ARGUMENT
        );
        assert!(out.is_null()); // Optional error slot does not change status.
    }
}

#[test]
fn panic_is_contained_and_program_poisoned() {
    let program = compile(E::constant_int(1), &[], ContextDesc::default());
    INJECT_PANIC.with(|flag| flag.set(true));
    assert_eq!(eval_error(&program, &[], 1, None).0, PANIC);
    assert_eq!(eval_error(&program, &[], 1, None).0, POISONED);
    let error = guard::<()>(|| panic!("synthetic compile boundary panic")).unwrap_err();
    assert_eq!(error.status, PANIC);
    unsafe {
        let mut result_view = ResultView::default();
        result_view.warning_count = 1;
        assert_eq!(
            tikv_expr_result_get_view(ptr::null(), &mut result_view),
            INVALID_ARGUMENT
        );
        assert_eq!(result_view.warning_count, 0);
        let mut error_view = ErrorView::default();
        assert_eq!(
            tikv_expr_error_get_view(ptr::null(), &mut error_view),
            INVALID_ARGUMENT
        );
    }
}
