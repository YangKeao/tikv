// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use protobuf::Message;
use tidb_query_datatype::FieldTypeTp;
use tipb::{Expr, FieldType, ScalarFuncSig};
use tipb_helper::ExprDefBuilder as E;

use crate::standalone::{
    Column, ColumnRef, Context, Error, PreparedExpression, ScalarRef, SelectedColumnRef,
};

fn prepare(expr: impl Into<Expr>, schema: &[FieldType], context: Context) -> PreparedExpression {
    PreparedExpression::compile(
        &expr.into().write_to_bytes().unwrap(),
        &schema
            .iter()
            .map(|ft| ft.write_to_bytes().unwrap())
            .collect::<Vec<_>>(),
        context,
    )
    .unwrap()
}
fn ints(values: &[i64]) -> Vec<u8> {
    values.iter().flat_map(|x| x.to_ne_bytes()).collect()
}
fn reals(values: &[f64]) -> Vec<u8> {
    values.iter().flat_map(|x| x.to_ne_bytes()).collect()
}
fn collect_int(
    program: &mut PreparedExpression,
    columns: &[ColumnRef<'_>],
    rows: usize,
    selection: Option<&[usize]>,
) -> Vec<Option<i64>> {
    let mut output = Vec::new();
    program
        .eval_borrowed(columns, rows, selection, |value| {
            output.push(match value {
                ScalarRef::Null => None,
                ScalarRef::Int(value) => Some(value),
                _ => panic!("unexpected result"),
            });
            Ok(())
        })
        .unwrap();
    output
}

#[test]
fn borrowed_selected_inputs_use_independent_rows() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let program = prepare(
        E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::column_ref(1, ft.clone())),
        &[ft.clone(), ft],
        Context::default(),
    );
    let left = ints(&[1, 2, 3]);
    let right = ints(&[10, 20, 30]);
    let left_rows = [2, 0];
    let right_rows = [1, 1];
    let inputs = [
        SelectedColumnRef {
            column: ColumnRef::Int {
                values: &left,
                validity: &[0b111],
            },
            selection: Some(&left_rows),
        },
        SelectedColumnRef {
            column: ColumnRef::Int {
                values: &right,
                validity: &[0b111],
            },
            selection: Some(&right_rows),
        },
    ];
    let mut actual = Vec::new();
    program
        .eval_borrowed_selected_shared(&inputs, 3, 2, |value| {
            actual.push(match value {
                ScalarRef::Int(value) => value,
                _ => panic!("unexpected result"),
            });
            Ok(())
        })
        .unwrap();
    assert_eq!(actual, [23, 21]);
}

#[test]
fn borrowed_selected_unequal_lengths_cross_batches() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let program = prepare(
        E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::column_ref(1, ft.clone())),
        &[ft.clone(), ft],
        Context::default(),
    );
    let left = ints(&[7]);
    let right = ints(&[10, 20, 30]);
    let count = crate::BATCH_MAX_SIZE + 3;
    let left_rows = vec![0; count];
    let right_rows: Vec<_> = (0..count).map(|i| 2 - i % 3).collect();
    let inputs = [
        SelectedColumnRef {
            column: ColumnRef::Int {
                values: &left,
                validity: &[1],
            },
            selection: Some(&left_rows),
        },
        SelectedColumnRef {
            column: ColumnRef::Int {
                values: &right,
                validity: &[0b101],
            },
            selection: Some(&right_rows),
        },
    ];
    // No shared physical count can describe these two input chunks.
    assert!(
        program
            .eval_borrowed_selected_shared(&inputs, 3, count, |_| panic!("invalid shared shape"))
            .is_err()
    );
    let mut actual = Vec::new();
    program
        .eval_borrowed_selected_columns_shared(&inputs, &[1, 3], count, |v| {
            actual.push(match v {
                ScalarRef::Int(v) => Some(v),
                ScalarRef::Null => None,
                _ => panic!("unexpected result"),
            });
            Ok(())
        })
        .unwrap();
    assert_eq!(
        actual,
        (0..count)
            .map(|i| match i % 3 {
                0 => Some(37),
                1 => None,
                _ => Some(17),
            })
            .collect::<Vec<_>>()
    );
    for counts in [&[][..], &[1][..], &[2, 3][..], &[1, 2][..]] {
        assert!(
            program
                .eval_borrowed_selected_columns_shared(&inputs, counts, count, |_| panic!(
                    "preflight failed"
                ))
                .is_err()
        );
    }
    let bad_rows = vec![1; count];
    let mut invalid = inputs;
    invalid[0].selection = Some(&bad_rows);
    assert!(
        program
            .eval_borrowed_selected_columns_shared(&invalid, &[1, 3], count, |_| panic!(
                "column-local bounds failed"
            ))
            .is_err()
    );
    invalid[0].selection = None;
    assert!(
        program
            .eval_borrowed_selected_columns_shared(&invalid, &[1, 3], count, |_| panic!(
                "dense bounds failed"
            ))
            .is_err()
    );
    invalid[0].selection = Some(&[]);
    invalid[1].selection = Some(&[]);
    program
        .eval_borrowed_selected_columns_shared(&invalid, &[1, 3], 0, |_| panic!("empty output"))
        .unwrap();
}

#[test]
fn borrowed_selected_mixed_dense_and_repeated_inputs() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let program = prepare(
        E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::column_ref(1, ft.clone())),
        &[ft.clone(), ft],
        Context::default(),
    );
    let left = ints(&[1, 2, 3]);
    let right = ints(&[10]);
    let inputs = [
        SelectedColumnRef {
            column: ColumnRef::Int {
                values: &left,
                validity: &[7],
            },
            selection: None,
        },
        SelectedColumnRef {
            column: ColumnRef::Int {
                values: &right,
                validity: &[1],
            },
            selection: Some(&[0, 0, 0]),
        },
    ];
    let mut actual = Vec::new();
    program
        .eval_borrowed_selected_columns_shared(&inputs, &[3, 1], 3, |v| {
            actual.push(match v {
                ScalarRef::Int(v) => v,
                _ => panic!("unexpected result"),
            });
            Ok(())
        })
        .unwrap();
    assert_eq!(actual, [11, 12, 13]);
}

#[test]
fn borrowed_selected_constants_need_no_physical_columns() {
    let program = prepare(E::constant_int(7), &[], Context::default());
    let mut count = 0;
    program
        .eval_borrowed_selected_columns_shared(&[], &[], 3, |v| {
            assert_eq!(v, ScalarRef::Int(7));
            count += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(count, 3);
    assert!(
        program
            .eval_borrowed_selected_columns_shared(&[], &[0], 0, |_| panic!("invalid shape"))
            .is_err()
    );
}

#[test]
fn borrowed_selected_dense_bounds_are_checked_before_sink() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let program = prepare(E::column_ref(0, ft.clone()), &[ft], Context::default());
    let payload = ints(&[7]);
    let inputs = [SelectedColumnRef {
        column: ColumnRef::Int {
            values: &payload,
            validity: &[1],
        },
        selection: None,
    }];
    let mut calls = 0;
    let result = program.eval_borrowed_selected_shared(&inputs, 1, 2, |_| {
        calls += 1;
        Ok(())
    });
    assert!(
        result.is_err(),
        "dense selection must not read beyond physical rows"
    );
    assert_eq!(calls, 0, "preflight must reject before invoking the sink");
}

#[test]
fn borrowed_selected_cross_batch_and_preflight() {
    let ft: FieldType = FieldTypeTp::Double.into();
    let program = prepare(
        E::scalar_func(ScalarFuncSig::PlusReal, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::column_ref(1, ft.clone())),
        &[ft.clone(), ft],
        Context::default(),
    );
    let left = reals(&[1.0, f64::INFINITY, 3.0]);
    let right = reals(&[10.0, 20.0, f64::NAN]);
    let count = crate::BATCH_MAX_SIZE + 3;
    let left_rows: Vec<_> = (0..count).map(|i| if i % 2 == 0 { 2 } else { 0 }).collect();
    let right_rows: Vec<_> = (0..count).map(|i| i % 2).collect();
    let mut inputs = [
        SelectedColumnRef {
            column: ColumnRef::Real {
                values: &left,
                validity: &[7],
            },
            selection: Some(&left_rows),
        },
        SelectedColumnRef {
            column: ColumnRef::Real {
                values: &right,
                validity: &[7],
            },
            selection: Some(&right_rows),
        },
    ];
    let mut actual = Vec::new();
    program
        .eval_borrowed_selected_shared(&inputs, 3, count, |v| {
            actual.push(match v {
                ScalarRef::Real(v) => v,
                _ => panic!("unexpected output"),
            });
            Ok(())
        })
        .unwrap();
    assert_eq!(
        actual,
        (0..count)
            .map(|i| if i % 2 == 0 { 13.0 } else { 21.0 })
            .collect::<Vec<_>>()
    );

    for bad in [vec![0], vec![3; count], vec![2; count]] {
        // Wrong length, out-of-range index, or selected NaN must all fail
        // before any output callback, including when another column is valid.
        let mut bad_inputs = inputs;
        bad_inputs[1].selection = Some(&bad);
        assert!(
            program
                .eval_borrowed_selected_shared(&bad_inputs, 3, count, |_| panic!(
                    "preflight failed"
                ))
                .is_err()
        );
    }
    inputs[0].selection = Some(&[]);
    inputs[1].selection = Some(&[]);
    program
        .eval_borrowed_selected_shared(&inputs, 3, 0, |_| panic!("empty output"))
        .unwrap();
}

#[test]
fn borrowed_nullable_nested_selection_and_unaligned_numeric() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let tree = E::scalar_func(ScalarFuncSig::AbsInt, ft.clone()).push_child(
        E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_int(-5)),
    );
    let mut program = prepare(tree, &[ft], Context::default());
    assert!(program.supports_borrowed());
    let mut payload = vec![0xEE];
    payload.extend(ints(&[2, 0, 9, -3]));
    let columns = [ColumnRef::Int {
        values: &payload[1..],
        validity: &[0b1101],
    }];
    let selection = [3, 1, 0, 3];
    let actual = collect_int(&mut program, &columns, 4, Some(&selection));
    assert_eq!(actual, vec![Some(8), None, Some(3), Some(8)]);
    assert_eq!(
        program
            .eval(
                &[Column::Int(vec![Some(2), None, Some(9), Some(-3)])],
                4,
                Some(&selection)
            )
            .unwrap()
            .column,
        Column::Int(actual)
    );
}

#[test]
fn borrowed_large_split_empty_and_constant() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let mut program = prepare(
        E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_int(7)),
        &[ft],
        Context::default(),
    );
    for rows in [0_usize, 1, 1024, 1025, 4097] {
        let values: Vec<_> = (0..rows as i64).collect();
        let data = ints(&values);
        let validity = vec![0xff; rows.div_ceil(8)];
        let columns = [ColumnRef::Int {
            values: &data,
            validity: &validity,
        }];
        let selection: Vec<_> = (0..rows).rev().flat_map(|row| [row, row]).collect();
        assert_eq!(
            collect_int(&mut program, &columns, rows, Some(&selection)),
            selection
                .iter()
                .map(|row| Some(*row as i64 + 7))
                .collect::<Vec<_>>()
        );
        assert!(collect_int(&mut program, &columns, rows, Some(&[])).is_empty());
    }
    let mut constant = prepare(E::constant_int(42), &[], Context::default());
    assert_eq!(
        collect_int(&mut constant, &[], 1025, None),
        vec![Some(42); 1025]
    );
}

#[test]
fn borrowed_bytes_payload_pointer_is_preserved() {
    let ft: FieldType = FieldTypeTp::VarString.into();
    let payload = [0xff, 0, b'x', 0xc3, 0xa9];
    let offsets = [0, 3, 5, 5];
    let columns = [ColumnRef::Bytes {
        values: &payload,
        offsets: &offsets,
        validity: &[0b011],
    }];
    let mut program = prepare(
        E::column_ref(0, ft.clone()),
        &[ft.clone()],
        Context::default(),
    );
    let mut seen = 0;
    program
        .eval_borrowed(&columns, 3, Some(&[1, 0, 1, 2]), |value| {
            match (seen, value) {
                (0 | 2, ScalarRef::Bytes(bytes)) => {
                    assert_eq!(bytes, &payload[3..]);
                    assert_eq!(bytes.as_ptr(), payload[3..].as_ptr());
                }
                (1, ScalarRef::Bytes(bytes)) => {
                    assert_eq!(bytes, &payload[..3]);
                    assert_eq!(bytes.as_ptr(), payload.as_ptr());
                }
                (3, ScalarRef::Null) => {}
                _ => panic!("unexpected result {value:?}"),
            }
            seen += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(seen, 4);
    let mut length = prepare(
        E::scalar_func(ScalarFuncSig::Length, FieldTypeTp::LongLong)
            .push_child(E::column_ref(0, ft.clone())),
        &[ft],
        Context::default(),
    );
    assert_eq!(
        collect_int(&mut length, &columns, 3, None),
        vec![Some(3), Some(2), None]
    );
}

#[test]
fn borrowed_real_kernels_and_nullable_comparison_match_copying() {
    let ft: FieldType = FieldTypeTp::Double.into();
    let tree = E::scalar_func(ScalarFuncSig::NullEqReal, FieldTypeTp::LongLong)
        .push_child(
            E::scalar_func(ScalarFuncSig::MultiplyReal, ft.clone())
                .push_child(E::column_ref(0, ft.clone()))
                .push_child(E::constant_real(2.0)),
        )
        .push_child(E::column_ref(1, ft.clone()));
    let mut program = prepare(tree, &[ft.clone(), ft], Context::default());
    let left = reals(&[1.5, 0.0, -2.0]);
    let right = reals(&[3.0, 0.0, 1.0]);
    let columns = [
        ColumnRef::Real {
            values: &left,
            validity: &[0b101],
        },
        ColumnRef::Real {
            values: &right,
            validity: &[0b101],
        },
    ];
    let output = collect_int(&mut program, &columns, 3, None);
    assert_eq!(output, vec![Some(1), Some(1), Some(0)]);
    assert_eq!(
        program
            .eval(
                &[
                    Column::Real(vec![Some(1.5), None, Some(-2.0)]),
                    Column::Real(vec![Some(3.0), None, Some(1.0)])
                ],
                3,
                None
            )
            .unwrap()
            .column,
        Column::Int(output)
    );
}

#[test]
fn borrowed_preflight_checks_whole_batch_before_any_sink() {
    let ft: FieldType = FieldTypeTp::Double.into();
    let mut program = prepare(
        E::scalar_func(ScalarFuncSig::MultiplyReal, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_real(0.0)),
        &[ft],
        Context::default(),
    );
    let rows = 1025;
    let mut values = vec![1.0; rows];
    values[1024] = f64::INFINITY;
    let data = reals(&values);
    let validity = vec![0xff; rows.div_ceil(8)];
    let columns = [ColumnRef::Real {
        values: &data,
        validity: &validity,
    }];
    let mut calls = 0;
    assert!(
        program
            .eval_borrowed(&columns, rows, None, |_| {
                calls += 1;
                Ok(())
            })
            .is_err()
    );
    assert_eq!(calls, 0);
    program
        .eval_borrowed(&columns, rows, Some(&[0, 1, 0]), |value| {
            assert_eq!(value, ScalarRef::Real(0.0));
            calls += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(calls, 3);
    for bad in [
        ColumnRef::Real {
            values: &data[..8],
            validity: &validity,
        },
        ColumnRef::Real {
            values: &data,
            validity: &[],
        },
    ] {
        assert!(
            program
                .eval_borrowed(&[bad], rows, None, |_| panic!("preflight must reject"))
                .is_err()
        );
    }
    assert!(
        program
            .eval_borrowed(&columns, rows, Some(&[rows]), |_| panic!("bad selection"))
            .is_err()
    );
}

#[test]
fn borrowed_bad_offsets_and_unsupported_program_are_rejected() {
    let ft: FieldType = FieldTypeTp::VarString.into();
    let mut program = prepare(E::column_ref(0, ft.clone()), &[ft], Context::default());
    for offsets in [&[0, -1][..], &[1, 0], &[0, 2], &[0]] {
        let col = ColumnRef::Bytes {
            values: b"x",
            validity: &[1],
            offsets,
        };
        assert!(
            program
                .eval_borrowed(&[col], 1, None, |_| panic!("invalid shape"))
                .is_err()
        );
    }
    let mut unsupported = prepare(
        E::scalar_func(ScalarFuncSig::Concat, FieldTypeTp::VarString)
            .push_child(E::constant_bytes(b"a".to_vec())),
        &[],
        Context::default(),
    );
    assert!(!unsupported.supports_borrowed());
    assert!(
        unsupported
            .eval_borrowed(&[], 1, None, |_| panic!("unsupported kernel"))
            .is_err()
    );
}

#[test]
fn borrowed_warning_counts_and_sink_abort() {
    let ft: FieldType = FieldTypeTp::Double.into();
    let mut program = prepare(
        E::scalar_func(ScalarFuncSig::DivideReal, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_real(0.0)),
        &[ft],
        Context {
            flags: 1 << 5,
            max_warning_count: 1,
            ..Context::default()
        },
    );
    let data = reals(&[1.0, 2.0]);
    let columns = [ColumnRef::Real {
        values: &data,
        validity: &[3],
    }];
    assert!(program.supports_borrowed());
    let diagnostics = program
        .eval_borrowed(&columns, 2, None, |value| {
            assert_eq!(value, ScalarRef::Null);
            Ok(())
        })
        .unwrap();
    assert_eq!(diagnostics.warning_count, 2);
    assert_eq!(diagnostics.warnings.len(), 1);
    let error = program
        .eval_borrowed(&columns, 2, None, |_| {
            Err(Error {
                code: 1105,
                message: "sink stopped".into(),
            })
        })
        .unwrap_err();
    assert_eq!(error.message, "sink stopped");
    assert_eq!(
        program
            .eval_borrowed(&columns, 2, Some(&[]), |_| panic!("empty selection"))
            .unwrap()
            .warning_count,
        0
    );
}

#[test]
fn borrowed_preserves_eager_nested_overflow_with_null_parent() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let tree = E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
        .push_child(E::column_ref(0, ft.clone()))
        .push_child(
            E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
                .push_child(E::column_ref(1, ft.clone()))
                .push_child(E::constant_int(1)),
        );
    let mut program = prepare(tree, &[ft.clone(), ft.clone()], Context::default());
    let null = ints(&[0]);
    let max = ints(&[i64::MAX]);
    let columns = [
        ColumnRef::Int {
            values: &null,
            validity: &[0],
        },
        ColumnRef::Int {
            values: &max,
            validity: &[1],
        },
    ];
    let borrowed = program
        .eval_borrowed(&columns, 1, None, |_| {
            panic!("overflow must precede output")
        })
        .unwrap_err();
    let copied = program
        .eval(
            &[Column::Int(vec![None]), Column::Int(vec![Some(i64::MAX)])],
            1,
            None,
        )
        .unwrap_err();
    assert_eq!(borrowed, copied);
    assert_eq!(borrowed.code, 1690);
    let tree = E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
        .push_child(
            E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
                .push_child(E::column_ref(0, ft.clone()))
                .push_child(E::column_ref(1, ft.clone())),
        )
        .push_child(E::constant_int(1));
    let mut program = prepare(tree, &[ft.clone(), ft], Context::default());
    assert_eq!(collect_int(&mut program, &columns, 1, None), vec![None]);
}

#[test]
fn borrowed_runtime_error_after_previous_batch_is_not_replayed() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let mut program = prepare(
        E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_int(1)),
        &[ft],
        Context::default(),
    );
    let mut values = vec![0; 1025];
    values[1024] = i64::MAX;
    let data = ints(&values);
    let validity = vec![0xff; 129];
    let mut calls = 0;
    let error = program
        .eval_borrowed(
            &[ColumnRef::Int {
                values: &data,
                validity: &validity,
            }],
            1025,
            None,
            |_| {
                calls += 1;
                Ok(())
            },
        )
        .unwrap_err();
    assert_eq!(error.code, 1690);
    assert_eq!(calls, 1024);
}
