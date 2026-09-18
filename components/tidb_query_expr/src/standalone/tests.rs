// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use protobuf::Message;
use tidb_query_datatype::{FieldTypeAccessor, FieldTypeFlag, FieldTypeTp};
use tipb_helper::ExprDefBuilder as E;

use super::*;

pub(super) fn prepare(
    expr: impl Into<Expr>,
    schema: &[FieldType],
    context: Context,
) -> PreparedExpression {
    let schema = schema
        .iter()
        .map(|ft| ft.write_to_bytes().unwrap())
        .collect::<Vec<_>>();
    PreparedExpression::compile(&expr.into().write_to_bytes().unwrap(), &schema, context).unwrap()
}

#[test]
fn nullable_nested_numeric_selection_and_reuse() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let expr = E::scalar_func(ScalarFuncSig::AbsInt, ft.clone()).push_child(
        E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_int(-5)),
    );
    let mut prepared = prepare(expr, &[ft.clone()], Context::default());
    let input = [Column::Int(vec![Some(2), None, Some(9), Some(-3)])];
    let result = prepared.eval(&input, 4, Some(&[3, 1, 0, 3])).unwrap();
    assert_eq!(
        result.column,
        Column::Int(vec![Some(8), None, Some(3), Some(8)])
    );
    assert_eq!(result.warning_count, 0);
    assert_eq!(
        prepared
            .eval(&[Column::Int(vec![Some(5)])], 1, None)
            .unwrap()
            .column,
        Column::Int(vec![Some(0)])
    );

    // Bare column references must also normalize logical rows, not leak their
    // underlying physical order (including duplicates and NULLs).
    let mut column = prepare(E::column_ref(0, ft.clone()), &[ft], Context::default());
    assert_eq!(
        column.eval(&input, 4, Some(&[2, 1, 2])).unwrap().column,
        Column::Int(vec![Some(9), None, Some(9)])
    );
}

#[test]
fn split_large_batches_and_broadcast_constants() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let mut expr = prepare(
        E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_int(7)),
        &[ft],
        Context::default(),
    );
    let rows = BATCH_MAX_SIZE * 2 + 13;
    let input = Column::Int((0..rows).map(|v| Some(v as i64)).collect());
    let selection = (0..rows).rev().collect::<Vec<_>>();
    assert_eq!(
        expr.eval(&[input], rows, Some(&selection)).unwrap().column,
        Column::Int(selection.iter().map(|v| Some(*v as i64 + 7)).collect())
    );
    let mut scalar = prepare(E::constant_int(42), &[], Context::default());
    assert_eq!(
        scalar.eval(&[], rows, None).unwrap().column,
        Column::Int(vec![Some(42); rows])
    );
    let mut null = prepare(
        E::constant_null(FieldTypeTp::Double),
        &[],
        Context::default(),
    );
    assert_eq!(
        null.eval(&[], 3, None).unwrap().column,
        Column::Real(vec![None; 3])
    );
}

#[test]
fn empty_batches_and_empty_selection() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let mut expr = prepare(
        E::scalar_func(ScalarFuncSig::AbsInt, ft.clone()).push_child(E::column_ref(0, ft.clone())),
        &[ft],
        Context::default(),
    );
    assert_eq!(
        expr.eval(&[Column::Int(vec![])], 0, None).unwrap().column,
        Column::Int(vec![])
    );
    // No selected row is evaluated, so even an overflowing value is harmless.
    assert_eq!(
        expr.eval(&[Column::Int(vec![Some(i64::MIN)])], 1, Some(&[]))
            .unwrap()
            .column,
        Column::Int(vec![])
    );
    let mut scalar = prepare(E::constant_int(1), &[], Context::default());
    assert_eq!(
        scalar.eval(&[], 0, None).unwrap().column,
        Column::Int(vec![])
    );
}

#[test]
fn real_nullable_arithmetic_and_comparison() {
    let ft: FieldType = FieldTypeTp::Double.into();
    let mut expr = prepare(
        E::scalar_func(ScalarFuncSig::GtReal, FieldTypeTp::LongLong)
            .push_child(
                E::scalar_func(ScalarFuncSig::MultiplyReal, ft.clone())
                    .push_child(E::column_ref(0, ft.clone()))
                    .push_child(E::constant_real(2.0)),
            )
            .push_child(E::constant_real(4.0)),
        &[ft.clone()],
        Context::default(),
    );
    assert_eq!(
        expr.eval(&[Column::Real(vec![Some(3.5), None, Some(1.0)])], 3, None)
            .unwrap()
            .column,
        Column::Int(vec![Some(1), None, Some(0)])
    );
    let mut identity = prepare(E::column_ref(0, ft.clone()), &[ft], Context::default());
    assert!(
        identity
            .eval(&[Column::Real(vec![Some(f64::NAN)])], 1, None)
            .is_err()
    );
}

#[test]
fn nonfinite_columns_return_errors_before_arithmetic() {
    let ft: FieldType = FieldTypeTp::Double.into();
    for sig in [ScalarFuncSig::MultiplyReal, ScalarFuncSig::MinusReal] {
        for value in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            let mut expr = prepare(
                E::scalar_func(sig, ft.clone())
                    .push_child(E::column_ref(0, ft.clone()))
                    .push_child(E::column_ref(1, ft.clone())),
                &[ft.clone(), ft.clone()],
                Context::default(),
            );
            let rhs = if sig == ScalarFuncSig::MultiplyReal {
                0.0
            } else {
                value
            };
            // In particular, Inf * 0 and Inf - Inf must not reach NotNan's
            // arithmetic operators, which panic when their result is NaN.
            let result = expr.eval(
                &[
                    Column::Real(vec![Some(value)]),
                    Column::Real(vec![Some(rhs)]),
                ],
                1,
                None,
            );
            assert!(result.is_err(), "{sig:?} accepted {value:?}");
        }
    }
}

#[test]
fn nonfinite_constants_are_rejected_at_compile_time() {
    let ft: FieldType = FieldTypeTp::Double.into();
    for value in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
        let constant = E::constant_real(value).build();
        let expressions = [
            constant.clone(),
            E::scalar_func(ScalarFuncSig::MultiplyReal, ft.clone())
                .push_child(constant.clone())
                .push_child(E::constant_real(0.0))
                .build(),
            E::scalar_func(ScalarFuncSig::MinusReal, ft.clone())
                .push_child(constant.clone())
                .push_child(constant)
                .build(),
        ];
        for expression in expressions {
            assert!(
                PreparedExpression::compile(
                    &expression.write_to_bytes().unwrap(),
                    &[],
                    Context::default()
                )
                .is_err(),
                "accepted nonfinite constant {value:?}"
            );
        }
    }
}

#[test]
fn nonfinite_unselected_rows_are_not_converted() {
    let ft: FieldType = FieldTypeTp::Double.into();
    let mut expr = prepare(
        E::scalar_func(ScalarFuncSig::MultiplyReal, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_real(0.0)),
        &[ft],
        Context::default(),
    );
    let columns = [Column::Real(vec![
        Some(f64::INFINITY),
        Some(2.0),
        Some(f64::NEG_INFINITY),
        Some(f64::NAN),
        None,
    ])];
    assert_eq!(
        expr.eval(&columns, 5, Some(&[1, 4, 1])).unwrap().column,
        Column::Real(vec![Some(0.0), None, Some(0.0)])
    );
    assert_eq!(
        expr.eval(&columns, 5, Some(&[])).unwrap().column,
        Column::Real(vec![])
    );
}

#[test]
fn bytes_are_copied_without_utf8_conversion() {
    let ft: FieldType = FieldTypeTp::VarChar.into();
    let expr = E::scalar_func(ScalarFuncSig::Concat, ft.clone())
        .push_child(E::column_ref(0, ft.clone()))
        .push_child(E::constant_bytes(vec![0, 255]));
    let mut prepared = prepare(expr, &[ft.clone()], Context::default());
    let input = [Column::Bytes(vec![
        Some(b"abc".to_vec()),
        None,
        Some(vec![]),
    ])];
    assert_eq!(
        prepared.eval(&input, 3, Some(&[2, 0, 1])).unwrap().column,
        Column::Bytes(vec![
            Some(vec![0, 255]),
            Some(vec![97, 98, 99, 0, 255]),
            None
        ])
    );
    let mut length = prepare(
        E::scalar_func(ScalarFuncSig::Length, FieldTypeTp::LongLong)
            .push_child(E::column_ref(0, ft.clone())),
        &[ft],
        Context::default(),
    );
    assert_eq!(
        length.eval(&input, 3, None).unwrap().column,
        Column::Int(vec![Some(3), None, Some(0)])
    );
}

#[test]
fn decimal_uses_existing_arithmetic_and_codec() {
    let mut ft: FieldType = FieldTypeTp::NewDecimal.into();
    ft.set_flen(20);
    ft.set_decimal(4);
    let mut prepared = prepare(
        E::scalar_func(ScalarFuncSig::PlusDecimal, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_decimal("0.25".parse().unwrap())),
        &[ft.clone()],
        Context::default(),
    );
    let result = prepared
        .eval(
            &[Column::Decimal(vec![
                Some("1.50".parse().unwrap()),
                None,
                Some("-2.25".parse().unwrap()),
            ])],
            3,
            None,
        )
        .unwrap();
    let Column::Decimal(values) = result.column else {
        panic!("expected decimals")
    };
    assert_eq!(values[0].unwrap(), "1.75".parse::<Decimal>().unwrap());
    assert_eq!(values[1], None);
    assert_eq!(values[2].unwrap(), "-2.00".parse::<Decimal>().unwrap());
    assert!("not-a-decimal".parse::<Decimal>().is_err());

    let mut context = Context::default();
    context.div_precision_increment = 6;
    let mut divide = prepare(
        E::scalar_func(ScalarFuncSig::DivideDecimal, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_decimal("3".parse().unwrap())),
        &[ft],
        context,
    );
    let result = divide
        .eval(
            &[Column::Decimal(vec![Some("1".parse().unwrap())])],
            1,
            None,
        )
        .unwrap();
    let Column::Decimal(values) = result.column else {
        panic!("expected decimals")
    };
    assert!(
        values[0]
            .as_ref()
            .unwrap()
            .to_string()
            .starts_with("0.333333")
    );
}

#[test]
fn mysql_errors_and_warning_limit_across_batches() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let mut overflow = prepare(
        E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_int(1)),
        &[ft],
        Context::default(),
    );
    let error = overflow
        .eval(&[Column::Int(vec![Some(i64::MAX)])], 1, None)
        .unwrap_err();
    assert_eq!(error.code, 1690);
    assert!(error.message.contains("value is out of range"));
    assert!(!error.message.contains("Evaluate error:"));

    let ft: FieldType = FieldTypeTp::Double.into();
    let divide = || {
        E::scalar_func(ScalarFuncSig::DivideReal, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_real(0.0))
    };
    let mut context = Context::default();
    context.flags = Flag::IN_SELECT_STMT.bits();
    context.max_warning_count = 2;
    let mut warned = prepare(divide(), &[ft.clone()], context);
    let rows = BATCH_MAX_SIZE + 1;
    let result = warned
        .eval(&[Column::Real(vec![Some(1.0); rows])], rows, None)
        .unwrap();
    assert_eq!(result.column, Column::Real(vec![None; rows]));
    assert_eq!(result.warning_count, rows);
    assert_eq!(result.warnings.len(), 2);
    assert!(
        result
            .warnings
            .iter()
            .all(|w| w.code == 1365 && w.message == "evaluation failed: Division by 0")
    );
    assert_eq!(
        warned
            .eval(&[Column::Real(vec![None])], 1, None)
            .unwrap()
            .warning_count,
        0
    );

    let mut count_only = prepare(
        divide(),
        &[ft.clone()],
        Context {
            max_warning_count: 0,
            ..Context::default()
        },
    );
    let output = count_only
        .eval(&[Column::Real(vec![Some(1.0); 3])], 3, None)
        .unwrap();
    assert_eq!(output.warning_count, 3);
    assert!(output.warnings.is_empty());

    let context = Context {
        flags: Flag::IN_INSERT_STMT.bits(),
        sql_mode: (SqlMode::STRICT_ALL_TABLES | SqlMode::ERROR_FOR_DIVISION_BY_ZERO).bits(),
        ..Context::default()
    };
    let mut strict = prepare(divide(), &[ft], context);
    let error = strict
        .eval(&[Column::Real(vec![Some(1.0)])], 1, None)
        .unwrap_err();
    assert_eq!(
        error,
        Error {
            code: 1365,
            message: "Division by 0".into()
        }
    );
}

#[test]
fn reject_schema_shape_and_malformed_expression_before_engine() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let mut expr = prepare(
        E::column_ref(0, ft.clone()),
        &[ft.clone()],
        Context::default(),
    );
    assert!(expr.eval(&[], 0, None).is_err());
    assert!(expr.eval(&[Column::Int(vec![None])], 2, None).is_err());
    assert!(expr.eval(&[Column::Real(vec![None])], 1, None).is_err());
    assert!(
        expr.eval(&[Column::Int(vec![None])], 1, Some(&[1]))
            .is_err()
    );
    let schema = vec![ft.write_to_bytes().unwrap()];
    let reject = |expr: Expr| {
        assert!(
            PreparedExpression::compile(
                &expr.write_to_bytes().unwrap(),
                &schema,
                Context::default()
            )
            .is_err()
        );
    };
    reject(E::column_ref(1, ft.clone()).build());
    reject(E::column_ref(usize::MAX, ft.clone()).build());
    reject(E::constant_real(f64::NAN).build());
    let mut deep = E::constant_int(1).build();
    for _ in 0..66 {
        deep = E::scalar_func(ScalarFuncSig::AbsInt, ft.clone())
            .push_child(deep)
            .build();
    }
    reject(deep);
    reject(E::column_ref(0, FieldTypeTp::Double).build());
    reject(E::scalar_func(ScalarFuncSig::PlusInt, ft.clone()).build());
    reject(
        E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
            .push_child(E::constant_real(1.0))
            .push_child(E::constant_int(2))
            .build(),
    );
    reject(
        E::scalar_func(ScalarFuncSig::PlusInt, FieldTypeTp::Double)
            .push_child(E::constant_int(1))
            .push_child(E::constant_int(2))
            .build(),
    );
    // `Set` used to be refused at this boundary. It is now a carried input
    // type, so a NULL Set constant compiles and evaluates to a Set column.
    let mut null_set = prepare(E::constant_null(FieldTypeTp::Set), &[], Context::default());
    assert_eq!(
        null_set.eval(&[], 1, None).unwrap().column,
        Column::Set(vec![None])
    );
    let mut malformed = E::constant_int(1).build();
    malformed.set_val(vec![0]);
    reject(malformed);
    let mut malformed = E::constant_int(1).build();
    malformed.mut_children().push(E::constant_int(2).build());
    reject(malformed);
    let mut malformed = E::constant_int(1).build();
    malformed.mut_field_type().set_tp(12345);
    reject(malformed);
    assert!(PreparedExpression::compile(&[255], &schema, Context::default()).is_err());
    assert!(PreparedExpression::compile(&[], &schema, Context::default()).is_err());
}

#[test]
fn unsigned_bits_and_context_validation() {
    let mut ft: FieldType = FieldTypeTp::LongLong.into();
    ft.as_mut_accessor().set_flag(FieldTypeFlag::UNSIGNED);
    let mut expr = prepare(
        E::scalar_func(ScalarFuncSig::GtInt, FieldTypeTp::LongLong)
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_uint(1)),
        &[ft],
        Context::default(),
    );
    assert_eq!(
        expr.eval(&[Column::Int(vec![Some(-1)])], 1, None)
            .unwrap()
            .column,
        Column::Int(vec![Some(1)])
    );
    let context = Context {
        time_zone_name: Some("invalid/timezone".into()),
        ..Context::default()
    };
    assert_eq!(context.config().unwrap_err().code, 1298);
    let context = Context {
        time_zone_name: Some("UTC".into()),
        time_zone_offset: i64::MAX,
        ..Context::default()
    };
    assert!(context.config().is_ok());
    assert_eq!(
        Context {
            time_zone_offset: i64::MAX,
            ..Context::default()
        }
        .config()
        .unwrap_err()
        .code,
        1298
    );
    assert!(
        Context {
            div_precision_increment: 31,
            ..Context::default()
        }
        .config()
        .is_err()
    );
    assert!(
        Context {
            max_warning_count: usize::MAX,
            ..Context::default()
        }
        .config()
        .is_err()
    );
}

/// Reusing one [`ExecutionState`] across calls must reset warnings and rebind
/// to whichever program is being evaluated.
#[test]
fn execution_state_reuse_resets_warnings_and_rebinds_context() {
    let ft: FieldType = FieldTypeTp::Double.into();
    let divide = prepare(
        E::scalar_func(ScalarFuncSig::DivideReal, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_real(0.0)),
        &[ft],
        Context {
            flags: Flag::IN_SELECT_STMT.bits(),
            max_warning_count: 2,
            ..Context::default()
        },
    );
    let rows = BATCH_MAX_SIZE + 1;
    let mut state = divide.execution_state();
    let first = divide
        .eval_with_state(
            &mut state,
            &[Column::Real(vec![Some(1.0); rows])],
            rows,
            None,
        )
        .unwrap();
    assert_eq!(first.warning_count, rows);
    assert_eq!(first.warnings.len(), 2);

    // The same state starts the next call warning-fresh.
    let second = divide
        .eval_with_state(&mut state, &[Column::Real(vec![None])], 1, None)
        .unwrap();
    assert_eq!(second.column, Column::Real(vec![None]));
    assert_eq!(second.warning_count, 0);
    assert!(second.warnings.is_empty());

    // A state created by another program is rebound to this program's config.
    let other = prepare(E::constant_int(9), &[], Context::default());
    assert_eq!(
        other
            .eval_with_state(&mut state, &[], 1, None)
            .unwrap()
            .column,
        Column::Int(vec![Some(9)])
    );
    let third = divide
        .eval_with_state(&mut state, &[Column::Real(vec![Some(1.0)])], 1, None)
        .unwrap();
    assert_eq!(third.warning_count, 1);
    assert_eq!(third.warnings.len(), 1);
}

/// Milestone A acceptance: one compiled program is evaluated concurrently from
/// two threads and every result equals the single-threaded run. Sharing the
/// `Arc<PreparedExpression>` across `thread::spawn` also proves `Send + Sync`.
#[test]
fn compiled_program_evaluates_concurrently_from_two_threads() {
    use std::sync::{Arc, Barrier};

    let ft: FieldType = FieldTypeTp::LongLong.into();
    let expr = E::scalar_func(ScalarFuncSig::AbsInt, ft.clone()).push_child(
        E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_int(9)),
    );
    let program = Arc::new(prepare(expr, &[ft], Context::default()));
    let rows = BATCH_MAX_SIZE + 37;
    let input = Column::Int(
        (0..rows)
            .map(|row| {
                if row % 7 == 0 {
                    None
                } else {
                    Some(row as i64 - 500)
                }
            })
            .collect(),
    );
    let selection = (0..rows).rev().collect::<Vec<_>>();
    let expected = program
        .eval_shared(&[input.clone()], rows, Some(&selection))
        .unwrap();

    let barrier = Arc::new(Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let program = Arc::clone(&program);
            let input = input.clone();
            let selection = selection.clone();
            let expected = expected.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..8 {
                    let got = program
                        .eval_shared(&[input.clone()], rows, Some(&selection))
                        .unwrap();
                    assert_eq!(got, expected);
                }
            })
        })
        .collect();
    barrier.wait();
    for worker in workers {
        worker.join().unwrap();
    }
}

/// A `SET` column is a first-class carried input: `Column::Set` is copied into
/// `VectorValue::Set`, evaluated, and copied back into `Column::Set`. The
/// element name bytes are preserved verbatim, including non-UTF8 names, because
/// `Set`'s equality only compares the bit mask.
#[test]
fn set_column_roundtrip_null_empty_selection_multielement_and_non_utf8_names() {
    let mut set_ft: FieldType = FieldTypeTp::Set.into();
    set_ft.set_elems(protobuf::RepeatedField::from_vec(vec![
        "a".to_owned(),
        "b".to_owned(),
        "c".to_owned(),
    ]));
    let mut identity = prepare(
        E::column_ref(0, set_ft.clone()),
        std::slice::from_ref(&set_ft),
        Context::default(),
    );
    let input = Column::Set(vec![
        None,
        // Empty element selection: bit mask zero, name forced empty.
        Some(Set::new(b"ignored".to_vec(), 0)),
        // Multi-element, declaration order.
        Some(Set::new(b"a,c".to_vec(), 0b101)),
        // Non-UTF8 element names must survive unchanged.
        Some(Set::new(vec![0xff, 0xfe], 0b011)),
    ]);
    let output = identity
        .eval(std::slice::from_ref(&input), 4, None)
        .unwrap();
    assert_eq!(output.warning_count, 0);
    assert_eq!(output.column, input);
    let Column::Set(values) = &output.column else {
        panic!("expected a Set column")
    };
    assert_eq!(values[0], None);
    assert_eq!(values[1].as_ref().unwrap().name(), b"");
    assert_eq!(values[1].as_ref().unwrap().value(), 0);
    assert_eq!(values[2].as_ref().unwrap().name(), b"a,c");
    assert_eq!(values[3].as_ref().unwrap().name(), &[0xff, 0xfe]);

    // An empty selection produces an empty typed column without entering the
    // engine; a reordered/repeated selection follows selection order.
    assert_eq!(
        identity
            .eval(std::slice::from_ref(&input), 4, Some(&[]))
            .unwrap()
            .column,
        Column::Set(vec![])
    );
    assert_eq!(
        identity
            .eval(std::slice::from_ref(&input), 4, Some(&[3, 0, 3]))
            .unwrap()
            .column,
        Column::Set(vec![input_set(&input, 3), None, input_set(&input, 3)])
    );

    // The `(Set, Int)` cast consumes a Set column: the numeric value is the
    // selection bit mask. The engine keys casts off the field types, so the
    // wire signature only needs to be one the dispatcher admits.
    let mut cast = prepare(
        E::scalar_func(ScalarFuncSig::CastStringAsInt, FieldTypeTp::LongLong)
            .push_child(E::column_ref(0, set_ft.clone())),
        std::slice::from_ref(&set_ft),
        Context::default(),
    );
    assert_eq!(
        cast.eval(std::slice::from_ref(&input), 4, None)
            .unwrap()
            .column,
        Column::Int(vec![None, Some(0), Some(0b101), Some(0b011)])
    );
}

/// Helper for the repeated-selection assertion: clone one `Set` out of a
/// `Column::Set` input.
fn input_set(column: &Column, row: usize) -> Option<Set> {
    let Column::Set(values) = column else {
        panic!("expected a Set column")
    };
    values[row].clone()
}

/// `has_lazy_nodes` is a presence check; `eager_lazy_risk` is the sound
/// admission signal. A program can mix a lazily dispatched control node with an
/// eager node of a lazy-sensitive family that is not registered yet.
#[test]
fn eager_lazy_risk_reports_only_unregistered_lazy_sensitive_kernels() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let eager = prepare(
        E::scalar_func(ScalarFuncSig::PlusInt, ft.clone())
            .push_child(E::column_ref(0, ft.clone()))
            .push_child(E::constant_int(1)),
        std::slice::from_ref(&ft),
        Context::default(),
    );
    assert!(!eager.has_lazy_nodes());
    assert!(eager.eager_lazy_risk().is_empty());

    let lazy = prepare(
        E::scalar_func(ScalarFuncSig::IfInt, ft.clone())
            .push_child(E::constant_int(1))
            .push_child(E::constant_int(7))
            .push_child(E::constant_int(9)),
        &[],
        Context::default(),
    );
    assert!(lazy.has_lazy_nodes());
    assert!(lazy.eager_lazy_risk().is_empty());

    // The Tier 2 families are lazy now, so their nodes are not a risk even
    // though their names were once in `LAZY_SENSITIVE_KERNELS`.
    let elt = prepare(
        E::scalar_func(ScalarFuncSig::Elt, FieldTypeTp::VarChar)
            .push_child(E::constant_int(1))
            .push_child(E::constant_bytes(b"a".to_vec())),
        &[],
        Context::default(),
    );
    assert!(elt.has_lazy_nodes());
    assert!(elt.eager_lazy_risk().is_empty());

    for sig in [
        ScalarFuncSig::FieldInt,
        ScalarFuncSig::GreatestInt,
        ScalarFuncSig::LeastInt,
        ScalarFuncSig::IntervalInt,
    ] {
        let program = prepare(
            E::scalar_func(sig, FieldTypeTp::LongLong)
                .push_child(E::constant_int(1))
                .push_child(E::constant_int(1)),
            &[],
            Context::default(),
        );
        assert!(program.has_lazy_nodes(), "{sig:?} must be lazy");
        assert!(
            program.eager_lazy_risk().is_empty(),
            "{sig:?} must not be reported as an eager risk"
        );
    }

    // Every lazy-sensitive family is now lazy, so no program has eager risk:
    // the same `AddTime*Null` node that used to be reported is a lazy kernel
    // now, and a program mixing it with another lazy node reports nothing.
    let all_lazy = prepare(
        E::scalar_func(ScalarFuncSig::AddTimeDateTimeNull, FieldTypeTp::DateTime)
            .push_child(
                E::scalar_func(ScalarFuncSig::IfTime, FieldTypeTp::DateTime)
                    .push_child(E::constant_int(1))
                    .push_child(E::constant_null(FieldTypeTp::DateTime))
                    .push_child(E::constant_null(FieldTypeTp::DateTime)),
            )
            .push_child(E::constant_null(FieldTypeTp::DateTime)),
        &[],
        Context::default(),
    );
    assert!(all_lazy.has_lazy_nodes());
    assert!(all_lazy.eager_lazy_risk().is_empty());
}
