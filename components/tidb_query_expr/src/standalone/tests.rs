// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use protobuf::Message;
use tidb_query_datatype::{FieldTypeAccessor, FieldTypeFlag, FieldTypeTp};
use tipb_helper::ExprDefBuilder as E;

use super::*;

fn prepare(expr: impl Into<Expr>, schema: &[FieldType], context: Context) -> PreparedExpression {
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
    let mut prepared = prepare(expr, std::slice::from_ref(&ft), Context::default());
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
        std::slice::from_ref(&ft),
        Context::default(),
    );
    assert_eq!(
        expr.eval(&[Column::Real(vec![Some(3.5), None, Some(1.0)])], 3, None)
            .unwrap()
            .column,
        Column::Int(vec![Some(1), None, Some(0)])
    );
    let mut identity = prepare(E::column_ref(0, ft.clone()), &[ft], Context::default());
    identity
        .eval(&[Column::Real(vec![Some(f64::NAN)])], 1, None)
        .unwrap_err();
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
            let failure = format!("{sig:?} accepted {value:?}");
            result.expect_err(&failure);
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
            let failure = format!("accepted nonfinite constant {value:?}");
            PreparedExpression::compile(
                &expression.write_to_bytes().unwrap(),
                &[],
                Context::default(),
            )
            .expect_err(&failure);
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
    let mut prepared = prepare(expr, std::slice::from_ref(&ft), Context::default());
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
        std::slice::from_ref(&ft),
        Context::default(),
    );
    let result = prepared
        .eval(
            &[Column::Decimal(vec![
                Some("1.50".into()),
                None,
                Some("-2.25".into()),
            ])],
            3,
            None,
        )
        .unwrap();
    let Column::Decimal(values) = result.column else {
        panic!("expected decimals")
    };
    assert_eq!(
        values[0].as_ref().unwrap().parse::<Decimal>().unwrap(),
        "1.75".parse::<Decimal>().unwrap()
    );
    assert_eq!(values[1], None);
    assert_eq!(
        values[2].as_ref().unwrap().parse::<Decimal>().unwrap(),
        "-2.00".parse::<Decimal>().unwrap()
    );
    prepared
        .eval(
            &[Column::Decimal(vec![Some("not-a-decimal".into())])],
            1,
            None,
        )
        .unwrap_err();

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
        .eval(&[Column::Decimal(vec![Some("1".into())])], 1, None)
        .unwrap();
    let Column::Decimal(values) = result.column else {
        panic!("expected decimals")
    };
    assert!(values[0].as_ref().unwrap().starts_with("0.333333"));
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
    let mut warned = prepare(divide(), std::slice::from_ref(&ft), context);
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
        std::slice::from_ref(&ft),
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
        std::slice::from_ref(&ft),
        Context::default(),
    );
    expr.eval(&[], 0, None).unwrap_err();
    expr.eval(&[Column::Int(vec![None])], 2, None).unwrap_err();
    expr.eval(&[Column::Real(vec![None])], 1, None).unwrap_err();
    expr.eval(&[Column::Int(vec![None])], 1, Some(&[1]))
        .unwrap_err();
    let schema = vec![ft.write_to_bytes().unwrap()];
    let reject = |expr: Expr| {
        PreparedExpression::compile(&expr.write_to_bytes().unwrap(), &schema, Context::default())
            .unwrap_err();
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
    reject(E::scalar_func(ScalarFuncSig::Rand, FieldTypeTp::Double).build());
    reject(E::constant_null(FieldTypeTp::Json).build());
    let mut malformed = E::constant_int(1).build();
    malformed.set_val(vec![0]);
    reject(malformed);
    let mut malformed = E::constant_int(1).build();
    malformed.mut_children().push(E::constant_int(2).build());
    reject(malformed);
    let mut malformed = E::constant_int(1).build();
    malformed.mut_field_type().set_tp(12345);
    reject(malformed);
    PreparedExpression::compile(&[255], &schema, Context::default()).unwrap_err();
    PreparedExpression::compile(&[], &schema, Context::default()).unwrap_err();
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
    context.config().unwrap();
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
    Context {
        div_precision_increment: 31,
        ..Context::default()
    }
    .config()
    .unwrap_err();
    Context {
        max_warning_count: usize::MAX,
        ..Context::default()
    }
    .config()
    .unwrap_err();
}
