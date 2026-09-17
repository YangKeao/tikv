// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use protobuf::Message;
use tidb_query_datatype::FieldTypeTp;
use tipb::{Expr, FieldType, ScalarFuncSig};
use tipb_helper::ExprDefBuilder as E;

use crate::standalone::{Column, ColumnRef, Context, Error, PreparedExpression, ScalarRef};

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
        std::slice::from_ref(&ft),
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
    program
        .eval_borrowed(&columns, rows, None, |_| {
            calls += 1;
            Ok(())
        })
        .unwrap_err();
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
        program
            .eval_borrowed(&[bad], rows, None, |_| panic!("preflight must reject"))
            .unwrap_err();
    }
    program
        .eval_borrowed(&columns, rows, Some(&[rows]), |_| panic!("bad selection"))
        .unwrap_err();
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
        program
            .eval_borrowed(&[col], 1, None, |_| panic!("invalid shape"))
            .unwrap_err();
    }
    let mut unsupported = prepare(
        E::scalar_func(ScalarFuncSig::Concat, FieldTypeTp::VarString)
            .push_child(E::constant_bytes(b"a".to_vec())),
        &[],
        Context::default(),
    );
    assert!(!unsupported.supports_borrowed());
    unsupported
        .eval_borrowed(&[], 1, None, |_| panic!("unsupported kernel"))
        .unwrap_err();
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

#[test]
fn native_mixed_bytes_numeric_broadcast_and_fresh_buffers() {
    let int: FieldType = FieldTypeTp::LongLong.into();
    let bytes: FieldType = FieldTypeTp::VarString.into();
    let tree = E::scalar_func(ScalarFuncSig::PlusInt, int.clone())
        .push_child(
            E::scalar_func(ScalarFuncSig::PlusInt, int.clone())
                .push_child(E::column_ref(0, int.clone()))
                .push_child(
                    E::scalar_func(ScalarFuncSig::Length, int.clone())
                        .push_child(E::column_ref(1, bytes.clone())),
                ),
        )
        .push_child(E::column_ref(2, int.clone()));
    let mut program = prepare(tree, &[int.clone(), bytes, int], Context::default());
    for base in [10, 100] {
        // All inputs are dropped after this iteration; the program must retain none.
        let values = vec![base, base + 1, base + 2];
        let payload = b"a\0b\0\0ignored\0".to_vec();
        let offsets = vec![4, 5, 13];
        let nulls = vec![0, 0, 255];
        let increment = vec![7];
        let columns = [
            ColumnRef::NativeInt {
                values: &values,
                nulls: None,
                broadcast: false,
            },
            ColumnRef::NativeBytes {
                values: &payload,
                offsets: &offsets,
                nulls: Some(&nulls),
                broadcast: false,
            },
            ColumnRef::NativeInt {
                values: &increment,
                nulls: Some(&[0]),
                broadcast: true,
            },
        ];
        let selection: Vec<_> = (0..1025).flat_map(|_| [1, 2, 0, 1]).collect();
        let actual = collect_int(&mut program, &columns, 3, Some(&selection));
        let expected: Vec<_> = (0..1025)
            .flat_map(|_| [Some(base + 8), None, Some(base + 10), Some(base + 8)])
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(
            program
                .eval(
                    &[
                        Column::Int(values.iter().copied().map(Some).collect()),
                        Column::Bytes(vec![Some(b"a\0b".to_vec()), Some(vec![]), None]),
                        Column::Int(vec![Some(7); 3]),
                    ],
                    3,
                    Some(&selection),
                )
                .unwrap()
                .column,
            Column::Int(actual)
        );
        assert!(collect_int(&mut program, &columns, 3, Some(&[])).is_empty());
    }
}

#[test]
fn native_bytes_borrow_payload_and_only_strip_final_nul() {
    let ft: FieldType = FieldTypeTp::VarString.into();
    let mut program = prepare(E::column_ref(0, ft.clone()), &[ft], Context::default());
    let payload = [0xff, 0, b'x', 0, 0, b'n', 0];
    let offsets = [4, 5, 7];
    let column = ColumnRef::NativeBytes {
        values: &payload,
        offsets: &offsets,
        nulls: Some(&[0, 0, 255]),
        broadcast: false,
    };
    let selection = [1, 0, 2, 0, 1];
    let mut seen = 0;
    program
        .eval_borrowed(&[column], 3, Some(&selection), |value| {
            match (selection[seen], value) {
                (0, ScalarRef::Bytes(bytes)) => {
                    assert_eq!(bytes, &payload[..3]);
                    assert_eq!(bytes.as_ptr(), payload.as_ptr());
                }
                (1, ScalarRef::Bytes(bytes)) => {
                    assert!(bytes.is_empty());
                    assert_eq!(bytes.as_ptr(), payload[4..].as_ptr());
                }
                (2, ScalarRef::Null) => {}
                _ => panic!("unexpected native byte result {value:?}"),
            }
            seen += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(seen, selection.len());
    for (values, offsets) in [(&b"a\0b\0"[..], &[4_u64][..]), (&b"\0"[..], &[1][..])] {
        let mut calls = 0;
        program
            .eval_borrowed(
                &[ColumnRef::NativeBytes {
                    values,
                    offsets,
                    nulls: None,
                    broadcast: true,
                }],
                1025,
                Some(&[1024, 0, 1024]),
                |value| {
                    let ScalarRef::Bytes(bytes) = value else {
                        panic!("expected bytes")
                    };
                    assert_eq!(bytes, &values[..values.len() - 1]);
                    assert_eq!(bytes.as_ptr(), values.as_ptr());
                    calls += 1;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(calls, 3);
    }
}

#[test]
fn native_real_original_kernel_parity_with_broadcast() {
    let ft: FieldType = FieldTypeTp::Double.into();
    let tree = E::scalar_func(ScalarFuncSig::NullEqReal, FieldTypeTp::LongLong)
        .push_child(
            E::scalar_func(ScalarFuncSig::MultiplyReal, ft.clone())
                .push_child(E::column_ref(0, ft.clone()))
                .push_child(E::column_ref(1, ft.clone())),
        )
        .push_child(E::column_ref(2, ft.clone()));
    let mut program = prepare(tree, &[ft.clone(), ft.clone(), ft], Context::default());
    let values = [1.5, f64::NAN, -2.0];
    let packed = reals(&[3.0, 0.0, 1.0]);
    let columns = [
        ColumnRef::NativeReal {
            values: &values,
            nulls: Some(&[0, 255, 0]),
            broadcast: false,
        },
        ColumnRef::NativeReal {
            values: &[2.0],
            nulls: None,
            broadcast: true,
        },
        ColumnRef::Real {
            values: &packed,
            validity: &[0b101],
        },
    ];
    let selection: Vec<_> = (0..1025).flat_map(|_| [2, 1, 0, 2]).collect();
    let actual = collect_int(&mut program, &columns, 3, Some(&selection));
    assert_eq!(
        actual,
        (0..1025)
            .flat_map(|_| [Some(0), Some(1), Some(1), Some(0)])
            .collect::<Vec<_>>()
    );
    assert_eq!(
        program
            .eval(
                &[
                    Column::Real(vec![Some(1.5), None, Some(-2.0)]),
                    Column::Real(vec![Some(2.0); 3]),
                    Column::Real(vec![Some(3.0), None, Some(1.0)]),
                ],
                3,
                Some(&selection),
            )
            .unwrap()
            .column,
        Column::Int(actual)
    );
}

#[test]
fn native_real_finite_preflight_respects_selection_nulls_and_broadcast() {
    let ft: FieldType = FieldTypeTp::Double.into();
    let mut program = prepare(
        E::scalar_func(ScalarFuncSig::MultiplyReal, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_real(0.0)),
        &[ft],
        Context::default(),
    );
    for nonfinite in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut values = vec![1.0; 1025];
        values[1024] = nonfinite;
        let column = ColumnRef::NativeReal {
            values: &values,
            nulls: None,
            broadcast: false,
        };
        program
            .eval_borrowed(&[column], 1025, None, |_| panic!("preflight"))
            .unwrap_err();
        let mut calls = 0;
        program
            .eval_borrowed(&[column], 1025, Some(&[0, 1, 0]), |value| {
                assert_eq!(value, ScalarRef::Real(0.0));
                calls += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(calls, 3);
        for nulls in [None, Some(&[0][..]), Some(&[255][..])] {
            let column = ColumnRef::NativeReal {
                values: &[nonfinite],
                nulls,
                broadcast: true,
            };
            let mut calls = 0;
            let result = program.eval_borrowed(&[column], 1025, Some(&[1024, 0, 1024]), |value| {
                assert_eq!(value, ScalarRef::Null);
                calls += 1;
                Ok(())
            });
            if nulls == Some(&[255][..]) {
                result.unwrap();
                assert_eq!(calls, 3);
            } else {
                result.unwrap_err();
                assert_eq!(calls, 0);
            }
            program
                .eval_borrowed(&[column], 1025, Some(&[]), |_| panic!("empty selection"))
                .unwrap();
            program
                .eval_borrowed(&[column], 0, None, |_| panic!("empty batch"))
                .unwrap();
        }
    }
}

#[test]
fn native_numeric_shapes_and_nonzero_null_maps() {
    fn make(real: bool, len: usize, nulls: Option<&[u8]>, broadcast: bool) -> ColumnRef<'_> {
        if real {
            ColumnRef::NativeReal {
                values: &[10.0, 20.0, 30.0][..len],
                nulls,
                broadcast,
            }
        } else {
            ColumnRef::NativeInt {
                values: &[10, 20, 30][..len],
                nulls,
                broadcast,
            }
        }
    }
    for real in [false, true] {
        let ft: FieldType = if real {
            FieldTypeTp::Double
        } else {
            FieldTypeTp::LongLong
        }
        .into();
        let mut program = prepare(E::column_ref(0, ft.clone()), &[ft], Context::default());
        for (rows, broadcast, stored) in [(0, false, 0), (0, true, 1), (3, false, 3), (3, true, 1)]
        {
            for nulls in [None, Some(&[0, 0, 0][..stored])] {
                program
                    .eval_borrowed(&[make(real, stored, nulls, broadcast)], rows, None, |_| {
                        Ok(())
                    })
                    .unwrap();
            }
            for len in 0..=3 {
                if len != stored {
                    program
                        .eval_borrowed(&[make(real, len, None, broadcast)], rows, Some(&[]), |_| {
                            panic!("bad length")
                        })
                        .unwrap_err();
                    program
                        .eval_borrowed(
                            &[make(real, stored, Some(&[0, 0, 0][..len]), broadcast)],
                            rows,
                            Some(&[]),
                            |_| panic!("bad null map"),
                        )
                        .unwrap_err();
                }
            }
        }
        for null in [1, 2, 128, 255] {
            let nulls = [0, null, 0];
            let mut seen = 0;
            program
                .eval_borrowed(
                    &[make(real, 3, Some(&nulls), false)],
                    3,
                    Some(&[2, 1, 0, 1]),
                    |value| {
                        if seen == 1 || seen == 3 {
                            assert_eq!(value, ScalarRef::Null);
                        } else if real {
                            assert_eq!(value, ScalarRef::Real(if seen == 0 { 30.0 } else { 10.0 }));
                        } else {
                            assert_eq!(value, ScalarRef::Int(if seen == 0 { 30 } else { 10 }));
                        }
                        seen += 1;
                        Ok(())
                    },
                )
                .unwrap();
            assert_eq!(seen, 4);
            program
                .eval_borrowed(&[make(real, 1, Some(&[null]), true)], 1025, None, |value| {
                    assert_eq!(value, ScalarRef::Null);
                    Ok(())
                })
                .unwrap();
        }
        program
            .eval_borrowed(&[make(real, 1, None, true)], 3, Some(&[3]), |_| {
                panic!("bad logical selection")
            })
            .unwrap_err();
    }
}

#[test]
fn native_bytes_reject_malformed_storage_even_null_or_unselected() {
    let ft: FieldType = FieldTypeTp::VarString.into();
    let mut program = prepare(E::column_ref(0, ft.clone()), &[ft], Context::default());
    let malformed: &[(&[u8], &[u64])] = &[
        (b"a\0b\0", &[2]),           // too few offsets
        (b"a\0b\0", &[2, 4, 4]),     // too many offsets
        (b"a\0b\0", &[0, 4]),        // zero first end
        (b"a\0b\0", &[2, 2]),        // equal ends
        (b"a\0b\0", &[4, 2]),        // decreasing ends
        (b"a\0b\0", &[2, 5]),        // out of bounds
        (b"a\0b\0", &[2, u64::MAX]), // impossible end
        (b"a\0b\0unused", &[2, 4]),  // unused trailing chars
        (b"ax\0", &[2, 3]),          // first terminator absent
        (b"a\0bx", &[2, 4]),         // last terminator absent
    ];
    for &(values, offsets) in malformed {
        for nulls in [None, Some(&[0, 255][..]), Some(&[255, 255][..])] {
            let column = ColumnRef::NativeBytes {
                values,
                offsets,
                nulls,
                broadcast: false,
            };
            for selection in [None, Some(&[0][..]), Some(&[][..])] {
                program
                    .eval_borrowed(&[column], 2, selection, |_| {
                        panic!("invalid storage reached sink")
                    })
                    .unwrap_err();
            }
        }
    }
    for (rows, broadcast, values, offsets) in [
        (0, false, &b""[..], &[][..]),
        (0, true, &b"\0"[..], &[1_u64][..]),
        (2, false, &b"\0\0"[..], &[1, 2][..]),
        (2, true, &b"\0"[..], &[1][..]),
    ] {
        let stored = offsets.len();
        let good = ColumnRef::NativeBytes {
            values,
            offsets,
            nulls: None,
            broadcast,
        };
        program
            .eval_borrowed(&[good], rows, None, |value| {
                assert_eq!(value, ScalarRef::Bytes(b""));
                Ok(())
            })
            .unwrap();
        for len in 0..=3 {
            if len == stored {
                continue;
            }
            let bad = ColumnRef::NativeBytes {
                values,
                offsets,
                nulls: Some(&[0, 0, 0][..len]),
                broadcast,
            };
            program
                .eval_borrowed(&[bad], rows, Some(&[]), |_| panic!("bad null map"))
                .unwrap_err();
        }
    }
    for (values, offsets, broadcast) in [
        (&b"\0"[..], &[][..], false), // no rows may not leave stray chars
        (&b""[..], &[][..], true),    // zero logical rows still need one stored row
        (&b"x"[..], &[1_u64][..], true),
        (&b"\0\0"[..], &[1, 2][..], true),
    ] {
        let bad = ColumnRef::NativeBytes {
            values,
            offsets,
            nulls: None,
            broadcast,
        };
        program
            .eval_borrowed(&[bad], 0, None, |_| panic!("bad empty-batch shape"))
            .unwrap_err();
    }
}

#[test]
fn native_broadcast_bytes_with_packed_int_and_null255() {
    let int: FieldType = FieldTypeTp::LongLong.into();
    let bytes: FieldType = FieldTypeTp::VarString.into();
    let mut program = prepare(
        E::scalar_func(ScalarFuncSig::PlusInt, int.clone())
            .push_child(E::column_ref(0, int.clone()))
            .push_child(
                E::scalar_func(ScalarFuncSig::Length, int.clone())
                    .push_child(E::column_ref(1, bytes.clone())),
            ),
        &[int, bytes],
        Context::default(),
    );
    let packed = ints(&[5, 0, 7]);
    for nulls in [None, Some(&[0][..]), Some(&[255][..])] {
        let columns = [
            ColumnRef::Int {
                values: &packed,
                validity: &[0b101],
            },
            ColumnRef::NativeBytes {
                values: b"a\0b\0",
                offsets: &[4],
                nulls,
                broadcast: true,
            },
        ];
        let expected = if nulls == Some(&[255][..]) {
            vec![None; 4]
        } else {
            vec![Some(10), None, Some(8), Some(10)]
        };
        assert_eq!(
            collect_int(&mut program, &columns, 3, Some(&[2, 1, 0, 2])),
            expected
        );
    }
}

#[test]
fn native_broadcast_original_error_and_warning_parity() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let mut overflow = prepare(
        E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_int(1)),
        &[ft],
        Context::default(),
    );
    let actual = overflow
        .eval_borrowed(
            &[ColumnRef::NativeInt {
                values: &[i64::MAX],
                nulls: None,
                broadcast: true,
            }],
            3,
            None,
            |_| panic!("overflow before sink"),
        )
        .unwrap_err();
    let expected = overflow
        .eval(&[Column::Int(vec![Some(i64::MAX); 3])], 3, None)
        .unwrap_err();
    assert_eq!(actual, expected);
    assert_eq!(actual.code, 1690);

    let ft: FieldType = FieldTypeTp::Double.into();
    let mut divide = prepare(
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
    let actual = divide
        .eval_borrowed(
            &[ColumnRef::NativeReal {
                values: &[1.0],
                nulls: None,
                broadcast: true,
            }],
            1025,
            None,
            |value| {
                assert_eq!(value, ScalarRef::Null);
                Ok(())
            },
        )
        .unwrap();
    let expected = divide
        .eval(&[Column::Real(vec![Some(1.0); 1025])], 1025, None)
        .unwrap();
    assert_eq!(actual.warning_count, 1025);
    assert_eq!(actual.warning_count, expected.warning_count);
    assert_eq!(actual.warnings, expected.warnings);
}
