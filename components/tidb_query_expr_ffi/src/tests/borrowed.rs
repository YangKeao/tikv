// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use super::*;

struct DiagnosticsOwner(*mut DiagnosticsHandle);
impl Drop for DiagnosticsOwner {
    fn drop(&mut self) {
        unsafe { tikv_expr_diagnostics_free(self.0) }
    }
}

fn native_column(type_: u32, len: usize, nulls: Option<&[u8]>) -> BorrowedColumn {
    BorrowedColumn {
        struct_size: size_of::<BorrowedColumn>() as u32,
        type_,
        flags: 0,
        reserved: 0,
        len,
        nulls: nulls.map_or(ptr::null(), |n| n.as_ptr()),
        nulls_len: nulls.map_or(0, <[u8]>::len),
        ints: ptr::null(),
        reals: ptr::null(),
        chars: ptr::null(),
        chars_len: 0,
        offsets: ptr::null(),
        offsets_len: 0,
    }
}

fn native_int(values: &[i64], nulls: Option<&[u8]>) -> BorrowedColumn {
    BorrowedColumn {
        ints: values.as_ptr(),
        ..native_column(INT64, values.len(), nulls)
    }
}

fn native_real(values: &[f64], nulls: Option<&[u8]>) -> BorrowedColumn {
    BorrowedColumn {
        reals: values.as_ptr(),
        ..native_column(FLOAT64, values.len(), nulls)
    }
}

fn native_bytes(chars: &[u8], offsets: &[u64], nulls: Option<&[u8]>) -> BorrowedColumn {
    BorrowedColumn {
        chars: chars.as_ptr(),
        chars_len: chars.len(),
        offsets: offsets.as_ptr(),
        offsets_len: offsets.len(),
        ..native_column(BYTES, offsets.len(), nulls)
    }
}

fn native_output(type_: u32, capacity: usize, nulls: &mut [u8]) -> BorrowedOutput {
    BorrowedOutput {
        struct_size: size_of::<BorrowedOutput>() as u32,
        type_,
        capacity,
        nulls: nulls.as_mut_ptr(),
        nulls_capacity: nulls.len(),
        ints: ptr::null_mut(),
        reals: ptr::null_mut(),
        uint8s: ptr::null_mut(),
        uint64s: ptr::null_mut(),
    }
}

fn int_output(values: &mut [i64], nulls: &mut [u8]) -> BorrowedOutput {
    BorrowedOutput {
        ints: values.as_mut_ptr(),
        ..native_output(INT64, values.len(), nulls)
    }
}

fn real_output(values: &mut [f64], nulls: &mut [u8]) -> BorrowedOutput {
    BorrowedOutput {
        reals: values.as_mut_ptr(),
        ..native_output(FLOAT64, values.len(), nulls)
    }
}

fn uint8_output(values: &mut [u8], nulls: &mut [u8]) -> BorrowedOutput {
    BorrowedOutput {
        uint8s: values.as_mut_ptr(),
        ..native_output(UINT8, values.len(), nulls)
    }
}

fn uint64_output(values: &mut [u64], nulls: &mut [u8]) -> BorrowedOutput {
    BorrowedOutput {
        uint64s: values.as_mut_ptr(),
        ..native_output(UINT64, values.len(), nulls)
    }
}

fn borrowed_eval(
    program: &ProgramOwner,
    columns: &[BorrowedColumn],
    rows: usize,
    selection: Option<&Selection>,
    output: &BorrowedOutput,
) -> Result<DiagnosticsOwner, (u32, i32, String)> {
    let mut diagnostics = ptr::null_mut();
    let mut error = ptr::null_mut();
    let status = unsafe {
        tikv_expr_eval_borrowed(
            program.0,
            columns.as_ptr(),
            columns.len(),
            rows,
            selection.map_or(ptr::null(), |s| s),
            output,
            &mut diagnostics,
            &mut error,
        )
    };
    if status == OK {
        assert!(error.is_null());
        assert!(!diagnostics.is_null());
        Ok(DiagnosticsOwner(diagnostics))
    } else {
        assert!(diagnostics.is_null());
        let info = unsafe { error_info(error) };
        assert_eq!(info.0, status);
        Err(info)
    }
}

fn borrowed_error(
    program: &ProgramOwner,
    columns: &[BorrowedColumn],
    rows: usize,
    selection: Option<&Selection>,
    output: &BorrowedOutput,
) -> (u32, i32, String) {
    match borrowed_eval(program, columns, rows, selection, output) {
        Err(error) => error,
        Ok(_) => panic!("expected borrowed evaluation failure"),
    }
}

fn diagnostics_view(diagnostics: &DiagnosticsOwner) -> DiagnosticsView {
    let mut view = DiagnosticsView {
        warnings: ptr::null(),
        warnings_len: 0,
        warning_count: 0,
    };
    assert_eq!(
        unsafe { tikv_expr_diagnostics_get_view(diagnostics.0, &mut view) },
        OK
    );
    view
}

#[test]
fn native_version_layout_and_numeric_null255_repeated_selection() {
    assert_eq!(tikv_expr_borrowed_abi_version(), 1);
    if size_of::<usize>() == 8 {
        assert_eq!(size_of::<BorrowedColumn>(), 88);
        assert_eq!(size_of::<BorrowedOutput>(), 64);
        assert_eq!(size_of::<DiagnosticsView>(), 24);
    }
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
    assert_eq!(unsafe { tikv_expr_program_supports_borrowed(program.0) }, 1);
    let input = [2, i64::MIN, 9, -3];
    let nulls = [0, 255, 0, 0];
    let indices = [3, 1, 0, 3];
    let selection = Selection {
        indices: indices.as_ptr(),
        len: indices.len(),
    };
    let mut output = [-99; 5];
    let mut output_nulls = [99; 5];
    let diagnostics = borrowed_eval(
        &program,
        &[native_int(&input, Some(&nulls))],
        input.len(),
        Some(&selection),
        &int_output(&mut output, &mut output_nulls),
    )
    .unwrap();
    assert_eq!(output, [8, 0, 3, 8, -99]);
    assert_eq!(output_nulls, [0, 1, 0, 0, 99]);
    let view = diagnostics_view(&diagnostics);
    assert_eq!((view.warnings_len, view.warning_count), (0, 0));
}

#[test]
fn native_broadcast_mixed_columns_and_empty_selection() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let program = compile(
        E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::column_ref(1, ft.clone())),
        &[ft.clone(), ft],
        ContextDesc::default(),
    );
    let broadcast_values = [7];
    let physical_values = [10, 20, 30];
    let physical_nulls = [0, 2, 0];
    let mut broadcast = native_int(&broadcast_values, None);
    broadcast.flags = BROADCAST;
    let mut columns = [
        broadcast,
        native_int(&physical_values, Some(&physical_nulls)),
    ];
    let indices = [2, 0, 1, 2];
    let selection = Selection {
        indices: indices.as_ptr(),
        len: indices.len(),
    };
    let mut output = [-99; 4];
    let mut nulls = [99; 4];
    borrowed_eval(
        &program,
        &columns,
        3,
        Some(&selection),
        &int_output(&mut output, &mut nulls),
    )
    .unwrap();
    assert_eq!(output, [37, 17, 0, 37]);
    assert_eq!(nulls, [0, 0, 1, 0]);

    let broadcast_null = [255];
    columns[0].nulls = broadcast_null.as_ptr();
    columns[0].nulls_len = 1;
    borrowed_eval(
        &program,
        &columns,
        3,
        Some(&selection),
        &int_output(&mut output, &mut nulls),
    )
    .unwrap();
    assert_eq!(output, [0; 4]);
    assert_eq!(nulls, [1; 4]);

    let empty_output = BorrowedOutput {
        nulls: ptr::null_mut(),
        ..native_output(INT64, 0, &mut [])
    };
    borrowed_eval(
        &program,
        &columns,
        3,
        Some(&Selection::default()),
        &empty_output,
    )
    .unwrap();
    // A broadcast retains one stored row even when the physical batch is empty.
    columns[1] = native_int(&[], None);
    borrowed_eval(&program, &columns, 0, None, &empty_output).unwrap();
    columns[0].len = 0;
    assert_eq!(
        borrowed_error(&program, &columns, 0, None, &empty_output).0,
        INVALID_ARGUMENT
    );
}

#[test]
fn native_column_string_end_offsets_run_length_kernel() {
    let bytes_ft: FieldType = FieldTypeTp::VarString.into();
    let int_ft: FieldType = FieldTypeTp::LongLong.into();
    let program = compile(
        E::scalar_func(ScalarFuncSig::Length, int_ft)
            .push_child(E::column_ref(0, bytes_ft.clone())),
        &[bytes_ft],
        ContextDesc::default(),
    );
    // Three-byte binary value, empty string, one-byte value, then a NULL row.
    let chars = [b'a', 0, 0xff, 0, 0, b'x', 0, 0];
    let offsets = [4, 5, 7, 8];
    let input_nulls = [0, 0, 0, 255];
    let indices = [1, 0, 3, 2, 0];
    let selection = Selection {
        indices: indices.as_ptr(),
        len: indices.len(),
    };
    let mut output = [-99; 5];
    let mut nulls = [99; 5];
    borrowed_eval(
        &program,
        &[native_bytes(&chars, &offsets, Some(&input_nulls))],
        4,
        Some(&selection),
        &int_output(&mut output, &mut nulls),
    )
    .unwrap();
    assert_eq!(output, [0, 3, 0, 1, 3]);
    assert_eq!(nulls, [0, 0, 1, 0, 0]);

    let one_offset = [4];
    let mut broadcast = native_bytes(&chars[..4], &one_offset, None);
    broadcast.flags = BROADCAST;
    borrowed_eval(
        &program,
        &[broadcast],
        4,
        Some(&selection),
        &int_output(&mut output, &mut nulls),
    )
    .unwrap();
    assert_eq!(output, [3; 5]);
    assert_eq!(nulls, [0; 5]);
}

#[test]
fn native_float64_output_selected_finite_validation_precedes_all_writes() {
    let ft: FieldType = FieldTypeTp::Double.into();
    let program = compile(
        E::scalar_func(ScalarFuncSig::MultiplyReal, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_real(2.0)),
        &[ft],
        ContextDesc::default(),
    );
    let input = [2.5, f64::INFINITY, f64::NAN, -3.0];
    let input_nulls = [0, 0, 255, 0];
    let columns = [native_real(&input, Some(&input_nulls))];
    let indices = [3, 0, 2, 0];
    let selection = Selection {
        indices: indices.as_ptr(),
        len: indices.len(),
    };
    let mut output = [-99.0; 4];
    let mut nulls = [99; 4];
    borrowed_eval(
        &program,
        &columns,
        4,
        Some(&selection),
        &real_output(&mut output, &mut nulls),
    )
    .unwrap();
    assert_eq!(output, [-6.0, 5.0, 0.0, 5.0]);
    assert_eq!(nulls, [0, 0, 1, 0]);

    // A later invalid selected value must not permit the first row to be written.
    for invalid_row in [1, 2] {
        let no_nulls = [native_real(&input, None)];
        let selected = [0, invalid_row];
        output.fill(-99.0);
        nulls.fill(99);
        let error = borrowed_error(
            &program,
            &no_nulls,
            4,
            Some(&Selection {
                indices: selected.as_ptr(),
                len: 2,
            }),
            &real_output(&mut output, &mut nulls),
        );
        assert_eq!((error.0, error.1), (EVAL_ERROR, 1105));
        assert!(error.2.contains("nonfinite"));
        assert_eq!(output, [-99.0; 4]);
        assert_eq!(nulls, [99; 4]);
    }
    borrowed_eval(
        &program,
        &[native_real(&input, None)],
        4,
        Some(&Selection::default()),
        &real_output(&mut output, &mut nulls),
    )
    .unwrap();
    assert_eq!(output, [-99.0; 4]);
    assert_eq!(nulls, [99; 4]);
}

#[test]
fn malformed_native_descriptors_and_output_capacities_never_write_payloads() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let program = compile(E::column_ref(0, ft.clone()), &[ft], ContextDesc::default());
    let input = [10, 20];
    let input_nulls = [0, 255];
    let reals = [1.0, 2.0];
    let good = native_int(&input, Some(&input_nulls));
    let mut bad_columns = Vec::new();
    let mut bad = good;
    bad.struct_size -= 1;
    bad_columns.push(bad);
    bad = good;
    bad.len = 1;
    bad_columns.push(bad);
    bad = good;
    bad.flags = BROADCAST << 1;
    bad_columns.push(bad);
    bad = good;
    bad.flags = BROADCAST; // Broadcast requires exactly one stored row.
    bad_columns.push(bad);
    bad = good;
    bad.reserved = 1;
    bad_columns.push(bad);
    bad = good;
    bad.nulls_len = 1;
    bad_columns.push(bad);
    bad = good;
    bad.nulls = ptr::null();
    bad_columns.push(bad);
    bad = good;
    bad.ints = ptr::null();
    bad_columns.push(bad);
    bad = good;
    bad.reals = reals.as_ptr();
    bad_columns.push(bad);
    bad = good;
    bad.chars_len = 1;
    bad_columns.push(bad);
    bad = good;
    bad.type_ = UINT64;
    bad_columns.push(bad);
    for column in bad_columns {
        let mut output = [-99; 2];
        let mut nulls = [99; 2];
        let error = borrowed_error(
            &program,
            &[column],
            2,
            None,
            &int_output(&mut output, &mut nulls),
        );
        assert_eq!(error.0, INVALID_ARGUMENT);
        assert_eq!(output, [-99; 2]);
        assert_eq!(nulls, [99; 2]);
    }

    let mut output = [-99; 2];
    let mut nulls = [99; 2];
    let mut unused = [0.0; 2];
    let valid_output = int_output(&mut output, &mut nulls);
    for case in 0..7 {
        let mut bad = valid_output;
        match case {
            0 => bad.capacity = 1,
            1 => bad.nulls_capacity = 1,
            2 => bad.struct_size -= 1,
            3 => bad.nulls = ptr::null_mut(),
            4 => bad.ints = ptr::null_mut(),
            5 => bad.reals = unused.as_mut_ptr(),
            6 => bad.type_ = BYTES,
            _ => unreachable!(),
        }
        assert_eq!(
            borrowed_error(&program, &[good], 2, None, &bad).0,
            INVALID_ARGUMENT
        );
        assert_eq!(output, [-99; 2]);
        assert_eq!(nulls, [99; 2]);
    }
    let indices = [0, 2];
    assert_eq!(
        borrowed_error(
            &program,
            &[good],
            2,
            Some(&Selection {
                indices: indices.as_ptr(),
                len: 2
            }),
            &valid_output,
        )
        .0,
        INVALID_ARGUMENT
    );
    assert_eq!(
        borrowed_error(&program, &[], 2, None, &valid_output).0,
        INVALID_ARGUMENT
    );
    assert_eq!(output, [-99; 2]);
    assert_eq!(nulls, [99; 2]);
}

#[test]
fn malformed_native_end_offsets_validate_null_and_unselected_rows_without_writes() {
    let bytes_ft: FieldType = FieldTypeTp::VarString.into();
    let int_ft: FieldType = FieldTypeTp::LongLong.into();
    let program = compile(
        E::scalar_func(ScalarFuncSig::Length, int_ft)
            .push_child(E::column_ref(0, bytes_ft.clone())),
        &[bytes_ft],
        ContextDesc::default(),
    );
    let nulls = [0, 255];
    let indices = [0];
    let selected = Selection {
        indices: indices.as_ptr(),
        len: 1,
    };
    let empty = Selection::default();
    // All arrays remain live and allocated; no dangling or out-of-bounds memory
    // is used to manufacture a supposedly catchable pointer failure.
    let chars = [b'a', 0, b'b', 0];
    let missing_terminal = [b'a', 0, b'b', b'x'];
    let invalid_offsets = [[0, 4], [2, 2], [3, 2], [2, 5], [2, 3]];
    for selection in [None, Some(&selected), Some(&empty)] {
        for offsets in &invalid_offsets {
            let mut output = [-99; 2];
            let mut output_nulls = [99; 2];
            let error = borrowed_error(
                &program,
                &[native_bytes(&chars, offsets, Some(&nulls))],
                2,
                selection,
                &int_output(&mut output, &mut output_nulls),
            );
            assert_eq!((error.0, error.1), (EVAL_ERROR, 1105));
            assert_eq!(output, [-99; 2]);
            assert_eq!(output_nulls, [99; 2]);
        }
        let offsets = [2, 4];
        let mut output = [-99; 2];
        let mut output_nulls = [99; 2];
        let error = borrowed_error(
            &program,
            &[native_bytes(&missing_terminal, &offsets, Some(&nulls))],
            2,
            selection,
            &int_output(&mut output, &mut output_nulls),
        );
        assert_eq!((error.0, error.1), (EVAL_ERROR, 1105));
        assert_eq!(output, [-99; 2]);
        assert_eq!(output_nulls, [99; 2]);

        let mut bad = native_bytes(&chars, &offsets, Some(&nulls));
        bad.offsets_len = 1;
        assert_eq!(
            borrowed_error(
                &program,
                &[bad],
                2,
                selection,
                &int_output(&mut output, &mut output_nulls),
            )
            .0,
            INVALID_ARGUMENT
        );
        assert_eq!(output, [-99; 2]);
        assert_eq!(output_nulls, [99; 2]);
    }
}

#[test]
fn native_payload_aliases_are_rejected_without_writes() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let program = compile(E::column_ref(0, ft.clone()), &[ft], ContextDesc::default());
    let mut input = [10, 20, 30];
    let mut nulls = [99; 2];
    // The advertised overlapping range has real writable backing storage.
    let alias = input.as_mut_ptr().wrapping_add(1);
    let columns = [native_int(&input, None)];
    let output = BorrowedOutput {
        ints: alias,
        ..native_output(INT64, 2, &mut nulls)
    };
    let indices = [0, 1];
    let selection = Selection {
        indices: indices.as_ptr(),
        len: 2,
    };
    assert_eq!(
        borrowed_error(&program, &columns, 3, Some(&selection), &output).0,
        INVALID_ARGUMENT
    );
    assert_eq!(input, [10, 20, 30]);
    assert_eq!(nulls, [99; 2]);

    let mut output_values = [-99; 3];
    let mut input_nulls = [0, 255, 0];
    let alias_nulls = input_nulls.as_mut_ptr();
    let columns = [native_int(&input, Some(&input_nulls))];
    let output = BorrowedOutput {
        nulls: alias_nulls,
        nulls_capacity: 3,
        ..int_output(&mut output_values, &mut [])
    };
    assert_eq!(
        borrowed_error(&program, &columns, 3, None, &output).0,
        INVALID_ARGUMENT
    );
    assert_eq!(input_nulls, [0, 255, 0]);
    assert_eq!(output_values, [-99; 3]);

    // The two writable output buffers must also be disjoint.
    let mut shared = [99u8; 6];
    let output = BorrowedOutput {
        uint8s: shared.as_mut_ptr(),
        nulls: shared.as_mut_ptr().wrapping_add(1),
        nulls_capacity: 3,
        ..native_output(UINT8, 3, &mut [])
    };
    assert_eq!(
        borrowed_error(&program, &[native_int(&input, None)], 3, None, &output).0,
        INVALID_ARGUMENT
    );
    assert_eq!(shared, [99; 6]);
}

#[test]
fn native_checked_uint8_and_uint64_sinks_and_partial_later_failure() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let program = compile(E::column_ref(0, ft.clone()), &[ft], ContextDesc::default());
    let input = [0, 1, -999];
    let input_nulls = [0, 0, 255];
    let mut output8 = [99; 3];
    let mut nulls = [99; 3];
    borrowed_eval(
        &program,
        &[native_int(&input, Some(&input_nulls))],
        3,
        None,
        &uint8_output(&mut output8, &mut nulls),
    )
    .unwrap();
    assert_eq!(output8, [0, 1, 0]);
    assert_eq!(nulls, [0, 0, 1]);

    for invalid in [-1, 2, 255, i64::MAX] {
        output8.fill(99);
        nulls.fill(99);
        let input = [1, invalid, 0];
        let error = borrowed_error(
            &program,
            &[native_int(&input, None)],
            3,
            None,
            &uint8_output(&mut output8, &mut nulls),
        );
        assert_eq!((error.0, error.1), (EVAL_ERROR, 1105));
        assert!(error.2.contains("conversion"));
        // Sink failures are not atomic: callers must discard the whole output.
        assert_eq!(output8, [1, 99, 99]);
        assert_eq!(nulls, [0, 99, 99]);
    }

    let input = [0, i64::MAX, -999];
    let mut output64 = [99; 3];
    borrowed_eval(
        &program,
        &[native_int(&input, Some(&input_nulls))],
        3,
        None,
        &uint64_output(&mut output64, &mut nulls),
    )
    .unwrap();
    assert_eq!(output64, [0, i64::MAX as u64, 0]);
    assert_eq!(nulls, [0, 0, 1]);
    output64.fill(99);
    nulls.fill(99);
    let negative = [7, -1, 8];
    let error = borrowed_error(
        &program,
        &[native_int(&negative, None)],
        3,
        None,
        &uint64_output(&mut output64, &mut nulls),
    );
    assert_eq!((error.0, error.1), (EVAL_ERROR, 1105));
    assert_eq!(output64, [7, 99, 99]);
    assert_eq!(nulls, [0, 99, 99]);
    // Ordinary sink errors do not poison or change the original copying ABI.
    assert_eq!(unsafe { tikv_expr_program_supports_borrowed(program.0) }, 1);
    assert_eq!(
        values(&eval(&program, &[int_column(&negative, None)], 3, None)),
        Column::Int(vec![Some(7), Some(-1), Some(8)])
    );
}

#[test]
fn native_diagnostics_own_warnings_beyond_buffers_program_and_later_calls() {
    let ft: FieldType = FieldTypeTp::Double.into();
    let expr = E::scalar_func(ScalarFuncSig::DivideReal, ft.clone())
        .push_child(E::column_ref(0, ft.clone()))
        .push_child(E::constant_real(0.0))
        .build();
    for cap in [0, 2] {
        let program = compile(
            expr.clone(),
            slice::from_ref(&ft),
            ContextDesc {
                flags: 1 << 5,
                max_warning_count: cap,
                ..ContextDesc::default()
            },
        );
        let (diagnostics, expected) = {
            let rows = 2053;
            let input = vec![1.0; rows];
            let mut output = vec![-99.0; rows];
            let mut nulls = vec![99; rows];
            let diagnostics = borrowed_eval(
                &program,
                &[native_real(&input, None)],
                rows,
                None,
                &real_output(&mut output, &mut nulls),
            )
            .unwrap();
            assert_eq!(output, vec![0.0; rows]);
            assert_eq!(nulls, vec![1; rows]);
            let copied = eval(&program, &[real_column(&input)], rows, None);
            let copied_view = view(&copied);
            let expected: Vec<_> =
                unsafe { foreign_slice(copied_view.warnings, copied_view.warnings_len).unwrap() }
                    .iter()
                    .map(|warning| {
                        let message = unsafe {
                            foreign_slice(warning.message.data, warning.message.len).unwrap()
                        }
                        .to_vec();
                        (warning.mysql_code, message)
                    })
                    .collect();
            assert_eq!(copied_view.warning_count, rows);
            (diagnostics, expected)
        };
        // A fresh call does not reset an older diagnostics handle's owned data.
        let empty = borrowed_eval(
            &program,
            &[native_real(&[], None)],
            0,
            None,
            &real_output(&mut [], &mut []),
        )
        .unwrap();
        assert_eq!(diagnostics_view(&empty).warning_count, 0);
        drop(program);
        let view = diagnostics_view(&diagnostics);
        assert_eq!(view.warning_count, 2053);
        assert_eq!(view.warnings_len, cap as usize);
        let actual: Vec<_> = unsafe { foreign_slice(view.warnings, view.warnings_len).unwrap() }
            .iter()
            .map(|warning| {
                let message =
                    unsafe { foreign_slice(warning.message.data, warning.message.len).unwrap() }
                        .to_vec();
                (warning.mysql_code, message)
            })
            .collect();
        assert_eq!(actual, expected);
        for (code, message) in actual {
            assert_eq!(code, 1365);
            assert!(!message.is_empty());
        }
    }
    unsafe {
        let mut view = DiagnosticsView {
            warnings: ptr::null(),
            warnings_len: 7,
            warning_count: 9,
        };
        assert_eq!(
            tikv_expr_diagnostics_get_view(ptr::null(), &mut view),
            INVALID_ARGUMENT
        );
        assert!(view.warnings.is_null());
        assert_eq!((view.warnings_len, view.warning_count), (0, 0));
        tikv_expr_diagnostics_free(ptr::null_mut());
    }
}

#[test]
fn native_and_copying_panics_poison_the_same_program_in_both_directions() {
    for borrowed_panics in [true, false] {
        let program = compile(E::constant_int(1), &[], ContextDesc::default());
        let mut output = [-99];
        let mut nulls = [99];
        let sink = int_output(&mut output, &mut nulls);
        INJECT_PANIC.with(|flag| flag.set(true));
        if borrowed_panics {
            assert_eq!(borrowed_error(&program, &[], 1, None, &sink).0, PANIC);
        } else {
            assert_eq!(eval_error(&program, &[], 1, None).0, PANIC);
        }
        assert_eq!(unsafe { tikv_expr_program_supports_borrowed(program.0) }, 0);
        assert_eq!(borrowed_error(&program, &[], 1, None, &sink).0, POISONED);
        assert_eq!(eval_error(&program, &[], 1, None).0, POISONED);
        assert_eq!(output, [-99]);
        assert_eq!(nulls, [99]);
    }
}

#[test]
fn native_supports_rejects_bytes_output_without_affecting_copying() {
    unsafe {
        assert_eq!(tikv_expr_program_supports_borrowed(ptr::null()), 0);
    }
    let ft: FieldType = FieldTypeTp::VarString.into();
    let program = compile(
        E::scalar_func(ScalarFuncSig::Concat, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::column_ref(0, ft.clone())),
        &[ft],
        ContextDesc::default(),
    );
    assert_eq!(unsafe { tikv_expr_program_supports_borrowed(program.0) }, 0);
    let chars = [b'x', 0];
    let offsets = [2];
    let mut output = [-99];
    let mut nulls = [99];
    assert_eq!(
        borrowed_error(
            &program,
            &[native_bytes(&chars, &offsets, None)],
            1,
            None,
            &int_output(&mut output, &mut nulls),
        )
        .0,
        INVALID_ARGUMENT
    );
    assert_eq!(output, [-99]);
    assert_eq!(nulls, [99]);
    let slices = [Bytes::from_slice(&chars[..1])];
    assert_eq!(
        values(&eval(&program, &[bytes_column(&slices, None)], 1, None)),
        Column::Bytes(vec![Some(b"xx".to_vec())])
    );
}
