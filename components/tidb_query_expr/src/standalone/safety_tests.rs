// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use protobuf::Message;
use tidb_query_datatype::{FieldTypeTp, codec::batch::LazyBatchColumnVec};
use tipb_helper::ExprDefBuilder as E;

use super::{tests::prepare, *};

fn scalar(sig: ScalarFuncSig, tp: FieldTypeTp, children: Vec<Expr>) -> Expr {
    let mut expr = E::scalar_func(sig, tp).build();
    expr.set_children(children.into());
    expr
}
fn int(v: i64) -> Expr {
    E::constant_int(v).build()
}
fn real(v: f64) -> Expr {
    E::constant_real(v).build()
}
fn vector(v: f32) -> Expr {
    let mut expr = E::constant_null(FieldTypeTp::TiDbVectorFloat32).build();
    expr.set_tp(ExprType::TiDbVectorFloat32);
    let mut bytes = 1u32.to_le_bytes().to_vec();
    bytes.extend(v.to_le_bytes());
    expr.set_val(bytes);
    expr
}
fn native_eval(expr: Expr) -> Result<(), tidb_query_common::Error> {
    let mut ctx = EvalContext::default();
    let expression = RpnExpressionBuilder::build_from_expr_tree(expr, &mut ctx, 0)?;
    expression.eval_decoded(&mut ctx, &[], &LazyBatchColumnVec::empty(), &[0], 1)?;
    Ok(())
}
fn compile(expr: Expr) -> Result<PreparedExpression, Error> {
    PreparedExpression::compile(&expr.write_to_bytes().unwrap(), &[], Context::default())
}

#[test]
fn malformed_real_metadata_panics_natively_but_is_rejected_before_execution() {
    for (flen, decimal) in [(1, 2), (255, 0)] {
        let mut expr = scalar(
            ScalarFuncSig::CastStringAsReal,
            FieldTypeTp::Double,
            vec![E::constant_bytes(b"1".to_vec()).build()],
        );
        expr.mut_field_type().set_flen(flen);
        expr.mut_field_type().set_decimal(decimal);
        assert!(std::panic::catch_unwind(|| native_eval(expr.clone())).is_err());
        assert!(
            compile(expr)
                .unwrap_err()
                .message
                .contains("REAL precision")
        );
    }
    for (flen, decimal) in [(-2, 0), (0, -2), (0, 255)] {
        let mut expr = real(1.0);
        expr.mut_field_type().set_flen(flen);
        expr.mut_field_type().set_decimal(decimal);
        assert!(compile(expr).is_err());
    }
}

#[test]
fn fractional_digit_panic_witnesses_and_compile_preflight() {
    use ScalarFuncSig::*;
    for (sig, number, digits) in [
        (TruncateReal, 0.0, 309),
        (RoundWithFracReal, 0.0, 309),
        (RoundWithFracReal, 1.0, -400),
    ] {
        let expr = scalar(sig, FieldTypeTp::Double, vec![real(number), int(digits)]);
        assert!(
            std::panic::catch_unwind(|| native_eval(expr.clone())).is_err(),
            "{sig:?}"
        );
        assert!(
            compile(expr)
                .unwrap_err()
                .message
                .contains("fractional digits")
        );
    }
    let expr = scalar(
        RoundWithFracInt,
        FieldTypeTp::LongLong,
        vec![int(1), int(i64::MIN)],
    );
    #[cfg(debug_assertions)]
    assert!(std::panic::catch_unwind(|| native_eval(expr.clone())).is_err());
    assert!(compile(expr).is_err());

    for sig in [
        RoundWithFracInt,
        RoundWithFracDec,
        RoundWithFracReal,
        TruncateInt,
        TruncateUint,
        TruncateReal,
        TruncateDecimal,
    ] {
        let (tp, number) = match sig {
            RoundWithFracDec | TruncateDecimal => (
                FieldTypeTp::NewDecimal,
                E::constant_decimal("1".parse().unwrap()).build(),
            ),
            RoundWithFracReal | TruncateReal => (FieldTypeTp::Double, real(1.0)),
            _ => (FieldTypeTp::LongLong, int(1)),
        };
        for digit in [int(-309), int(309), E::constant_uint(u64::MAX).build()] {
            assert!(compile(scalar(sig, tp, vec![number.clone(), digit])).is_err());
        }
        let derived = scalar(PlusInt, FieldTypeTp::LongLong, vec![int(1), int(1)]);
        assert!(compile(scalar(sig, tp, vec![number, derived])).is_err());
    }
}

#[test]
fn selected_digit_columns_are_preflighted_across_entire_batch() {
    let ft: FieldType = FieldTypeTp::LongLong.into();
    let expr = scalar(
        ScalarFuncSig::RoundWithFracReal,
        FieldTypeTp::Double,
        vec![real(1.25), E::column_ref(0, ft.clone()).build()],
    );
    let mut prepared = prepare(expr, &[ft], Context::default());
    assert!(!prepared.supports_borrowed());
    let mut digits = vec![Some(1); BATCH_MAX_SIZE + 1];
    digits[BATCH_MAX_SIZE] = Some(309);
    let input = [Column::Int(digits)];
    assert!(
        prepared
            .eval(&input, BATCH_MAX_SIZE + 1, None)
            .unwrap_err()
            .message
            .contains("fractional digits")
    );
    assert_eq!(
        prepared
            .eval(&input, BATCH_MAX_SIZE + 1, Some(&[0, 0]))
            .unwrap()
            .column,
        Column::Real(vec![Some(1.2), Some(1.2)])
    );
    assert!(prepared.eval(&input, BATCH_MAX_SIZE + 1, Some(&[])).is_ok());
    assert_eq!(
        prepared
            .eval(&[Column::Int(vec![None])], 1, None)
            .unwrap()
            .column,
        Column::Real(vec![None])
    );
}

#[test]
fn finite_inputs_producing_nonfinite_intermediates_are_checked_before_consumption() {
    let vector_distance = scalar(
        ScalarFuncSig::VecL2DistanceSig,
        FieldTypeTp::Double,
        vec![vector(3e38), vector(-3e38)],
    );
    let round_overflow = scalar(
        ScalarFuncSig::RoundWithFracReal,
        FieldTypeTp::Double,
        vec![real(f64::MAX), int(-308)],
    );
    for producer in [vector_distance, round_overflow] {
        // Normal TiKV behavior remains unchanged: standalone checks are opt-in.
        assert!(native_eval(producer.clone()).is_ok());
        let chain = scalar(
            ScalarFuncSig::MultiplyReal,
            FieldTypeTp::Double,
            vec![producer.clone(), real(0.0)],
        );
        assert!(std::panic::catch_unwind(|| native_eval(chain.clone())).is_err());
        for expr in [producer, chain] {
            let mut prepared = compile(expr).unwrap();
            assert!(
                prepared
                    .eval(&[], 1, None)
                    .unwrap_err()
                    .message
                    .contains("nonfinite REAL")
            );
            assert!(prepared.eval(&[], 0, None).is_ok());
        }
    }
}
