// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::codec::data_type::*;

#[rpn_fn(nullable)]
#[inline]
fn if_null<T: Evaluable + EvaluableRet>(lhs: Option<&T>, rhs: Option<&T>) -> Result<Option<T>> {
    match crate::native_if_null_choose_first(lhs) {
        crate::NativeIfNullChoice::Done(value) => Ok(Some(value).cloned()),
        crate::NativeIfNullChoice::NeedSecond => Ok(rhs.cloned()),
    }
}

#[rpn_fn(nullable)]
#[inline]
fn if_null_json(lhs: Option<JsonRef>, rhs: Option<JsonRef>) -> Result<Option<Json>> {
    match crate::native_if_null_choose_first(lhs) {
        crate::NativeIfNullChoice::Done(value) => Ok(Some(value.to_owned())),
        crate::NativeIfNullChoice::NeedSecond => Ok(rhs.map(|x| x.to_owned())),
    }
}

#[rpn_fn(nullable)]
#[inline]
fn if_null_bytes(lhs: Option<BytesRef>, rhs: Option<BytesRef>) -> Result<Option<Bytes>> {
    match crate::native_if_null_choose_first(lhs) {
        crate::NativeIfNullChoice::Done(value) => Ok(Some(value.to_vec())),
        crate::NativeIfNullChoice::NeedSecond => Ok(rhs.map(|x| x.to_vec())),
    }
}

/// The SDK decides whether the actual first operand needs the lazy right side.
/// Even SQL NULL produces a present demand report, never a NULL head result.
#[rpn_fn(nullable)]
fn if_null_head_native(value: Option<BytesRef>) -> Result<Option<Bytes>> {
    crate::native_if_null::evaluate_if_null_head_native(value)
        .map(Some)
        .map_err(|error| other_err!("Invalid native IFNULL head transport: {:?}", error))
}

#[rpn_fn(nullable)]
fn if_null_finish_native(
    report: Option<BytesRef>,
    value: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    let value = crate::native_if_null::evaluate_if_null_finish_native(report, value)
        .map_err(|error| other_err!("Invalid native IFNULL finish transport: {:?}", error))?;
    // The validated original report proves the first operand was SQL NULL.
    // Reuse the wire worker's nullable byte ownership and the shared choice,
    // without interpreting the native identity representation.
    if_null_bytes(None, value)
}

#[rpn_fn(nullable, raw_varg, extra_validator = case_when_validator::<T>)]
#[inline]
pub fn case_when<T: Evaluable + EvaluableRet>(args: &[ScalarValueRef<'_>]) -> Result<Option<T>> {
    for chunk in args.chunks(2) {
        if chunk.len() == 1 {
            // Else statement
            let ret: Option<&T> = Evaluable::borrow_scalar_value_ref(chunk[0]);
            return Ok(ret.cloned());
        }
        let cond: Option<&Int> = Evaluable::borrow_scalar_value_ref(chunk[0]);
        match crate::native_if_choose_branch(cond.map(|value| *value != 0)) {
            crate::NativeIfBranch::Then => {
                let ret: Option<&T> = Evaluable::borrow_scalar_value_ref(chunk[1]);
                return Ok(ret.cloned());
            }
            crate::NativeIfBranch::Else => continue,
        }
    }
    Ok(None)
}

#[rpn_fn(nullable, raw_varg, extra_validator = case_when_validator::<Bytes>)]
#[inline]
pub fn case_when_bytes(args: &[ScalarValueRef<'_>]) -> Result<Option<Bytes>> {
    for chunk in args.chunks(2) {
        if chunk.len() == 1 {
            // Else statement
            let ret: Option<BytesRef> = EvaluableRef::borrow_scalar_value_ref(chunk[0]);
            return Ok(ret.map(|x| x.to_vec()));
        }
        let cond: Option<&Int> = Evaluable::borrow_scalar_value_ref(chunk[0]);
        match crate::native_if_choose_branch(cond.map(|value| *value != 0)) {
            crate::NativeIfBranch::Then => {
                let ret: Option<BytesRef> = EvaluableRef::borrow_scalar_value_ref(chunk[1]);
                return Ok(ret.map(|x| x.to_vec()));
            }
            crate::NativeIfBranch::Else => continue,
        }
    }
    Ok(None)
}

#[rpn_fn(nullable, raw_varg, extra_validator = case_when_validator::<Json>)]
#[inline]
pub fn case_when_json(args: &[ScalarValueRef<'_>]) -> Result<Option<Json>> {
    for chunk in args.chunks(2) {
        if chunk.len() == 1 {
            // Else statement
            let ret: Option<JsonRef> = EvaluableRef::borrow_scalar_value_ref(chunk[0]);
            return Ok(ret.map(|x| x.to_owned()));
        }
        let cond: Option<&Int> = Evaluable::borrow_scalar_value_ref(chunk[0]);
        match crate::native_if_choose_branch(cond.map(|value| *value != 0)) {
            crate::NativeIfBranch::Then => {
                let ret: Option<JsonRef> = EvaluableRef::borrow_scalar_value_ref(chunk[1]);
                return Ok(ret.map(|x| x.to_owned()));
            }
            crate::NativeIfBranch::Else => continue,
        }
    }
    Ok(None)
}

#[rpn_fn(nullable)]
#[inline]
fn if_condition<T: Evaluable + EvaluableRet>(
    condition: Option<&Int>,
    value_if_true: Option<&T>,
    value_if_false: Option<&T>,
) -> Result<Option<T>> {
    Ok(
        match crate::native_if_choose_branch(condition.map(|value| *value != 0)) {
            crate::NativeIfBranch::Then => value_if_true.cloned(),
            crate::NativeIfBranch::Else => value_if_false.cloned(),
        },
    )
}

#[rpn_fn(nullable)]
#[inline]
fn if_condition_json(
    condition: Option<&Int>,
    value_if_true: Option<JsonRef>,
    value_if_false: Option<JsonRef>,
) -> Result<Option<Json>> {
    Ok(
        match crate::native_if_choose_branch(condition.map(|value| *value != 0)) {
            crate::NativeIfBranch::Then => value_if_true.map(|x| x.to_owned()),
            crate::NativeIfBranch::Else => value_if_false.map(|x| x.to_owned()),
        },
    )
}

#[rpn_fn(nullable)]
#[inline]
fn if_condition_bytes(
    condition: Option<&Int>,
    value_if_true: Option<BytesRef>,
    value_if_false: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    Ok(
        match crate::native_if_choose_branch(condition.map(|value| *value != 0)) {
            crate::NativeIfBranch::Then => value_if_true.map(|x| x.to_vec()),
            crate::NativeIfBranch::Else => value_if_false.map(|x| x.to_vec()),
        },
    )
}

/// The actual normalized condition always produces a present branch report.
#[rpn_fn(nullable)]
fn if_head_native(condition: Option<&Int>) -> Result<Option<Bytes>> {
    crate::native_if::evaluate_if_head_native(condition.copied())
        .map(Some)
        .map_err(|error| other_err!("Invalid native IF head transport: {:?}", error))
}

#[rpn_fn(nullable)]
fn if_finish_native(report: Option<BytesRef>, value: Option<BytesRef>) -> Result<Option<Bytes>> {
    let value = crate::native_if::evaluate_if_finish_native(report, value)
        .map_err(|error| other_err!("Invalid native IF finish transport: {:?}", error))?;
    Ok(value.map(|value| value.to_vec()))
}

#[rpn_fn(nullable)]
fn null_if_native(lhs: Option<BytesRef>, comparison: Option<&Int>) -> Result<Option<Bytes>> {
    let selected = crate::native_if::evaluate_null_if_native(lhs, comparison.copied())
        .map_err(|error| other_err!("Invalid native NULLIF transport: {:?}", error))?;
    Ok(selected.map(|value| value.to_vec()))
}

fn case_when_validator<T: EvaluableRet>(expr: &crate::types::function::CallShape) -> Result<()> {
    for chunk in expr.args().chunks(2) {
        if chunk.len() == 1 {
            super::function::validate_field_type(chunk[0].field_type(), T::EVAL_TYPE)?;
        } else {
            super::function::validate_field_type(
                chunk[0].field_type(),
                <Int as Evaluable>::EVAL_TYPE,
            )?;
            super::function::validate_field_type(chunk[1].field_type(), T::EVAL_TYPE)?;
        }
    }
    Ok(())
}

/// The local signed-integer seed retains its first operand instead of lowering
/// to an IF tree which evaluates that operand a second time.
#[rpn_fn(nullable)]
pub fn local_nullif_int_signed_signed(lhs: Option<&Int>, rhs: Option<&Int>) -> Result<Option<Int>> {
    use crate::impl_compare::{BasicComparer, CmpOpEq, compare};
    let equal = compare::<BasicComparer<Int, CmpOpEq>>(lhs, rhs)?;
    Ok(
        match crate::native_if_choose_branch(equal.map(|value| value == 1)) {
            crate::NativeIfBranch::Then => None,
            crate::NativeIfBranch::Else => lhs.copied(),
        },
    )
}

#[cfg(test)]
mod tests {
    use tipb::ScalarFuncSig;

    use super::*;
    use crate::test_util::RpnFnScalarEvaluator;

    #[test]
    fn test_if_null() {
        let cases = vec![
            (None, None, None),
            (None, Some(1), Some(1)),
            (Some(2), None, Some(2)),
            (Some(2), Some(1), Some(2)),
        ];
        for (lhs, rhs, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::IfNullInt)
                .unwrap();
            assert_eq!(output, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
        }
    }

    #[test]
    fn case_when_shared_branch_stops_on_selected_null_and_keeps_owned_else() {
        use ScalarValueRef as Ref;
        let zero = 0_i64;
        let later = 9_i64;
        for condition in [i64::MIN, -1, 2, i64::MAX] {
            for selected in [Some(&zero), None] {
                assert_eq!(
                    case_when::<Int>(&[
                        Ref::Int(Some(&condition)),
                        Ref::Int(selected),
                        Ref::Int(Some(&1)),
                        Ref::Int(Some(&later)),
                        Ref::Int(Some(&later)),
                    ])
                    .unwrap(),
                    selected.copied()
                );
            }
        }
        assert_eq!(case_when::<Int>(&[]).unwrap(), None);
        assert_eq!(case_when_bytes(&[]).unwrap(), None);
        assert_eq!(case_when_json(&[]).unwrap(), None);
        assert_eq!(
            case_when::<Int>(&[
                Ref::Int(None),
                Ref::Int(Some(&zero)),
                Ref::Int(Some(&later))
            ])
            .unwrap(),
            Some(later)
        );
        assert_eq!(
            case_when_bytes(&[
                Ref::Int(Some(&-1)),
                Ref::Bytes(None),
                Ref::Bytes(Some(b"dead else")),
            ])
            .unwrap(),
            None
        );

        let mut source = vec![255, 0, 7];
        let owned = case_when_bytes(&[
            Ref::Int(Some(&zero)),
            Ref::Bytes(Some(b"skipped")),
            Ref::Bytes(Some(&source)),
        ])
        .unwrap()
        .unwrap();
        assert_ne!(owned.as_ptr(), source.as_ptr());
        assert_eq!(
            case_when_bytes(&[Ref::Bytes(Some(&source))]).unwrap(),
            Some(source.clone())
        );
        source[0] = 1;
        drop(source);
        assert_eq!(owned, [255, 0, 7]);

        let json: Json = "null".parse().unwrap();
        assert_eq!(
            case_when_json(&[
                Ref::Int(Some(&-1)),
                Ref::Json(None),
                Ref::Json(Some(json.as_ref())),
            ])
            .unwrap(),
            None
        );
        assert_eq!(
            case_when_json(&[
                Ref::Int(None),
                Ref::Json(None),
                Ref::Json(Some(json.as_ref())),
            ])
            .unwrap(),
            Some(json.clone())
        );
        assert_eq!(
            case_when_json(&[Ref::Json(Some(json.as_ref()))]).unwrap(),
            Some(json)
        );
    }

    #[test]
    fn test_case_when() {
        let cases: Vec<(Vec<ScalarValue>, Option<Real>)> = vec![
            (
                vec![1.into(), (3.0).into(), 1.into(), (5.0).into()],
                Real::new(3.0).ok(),
            ),
            (
                vec![0.into(), (3.0).into(), 1.into(), (5.0).into()],
                Real::new(5.0).ok(),
            ),
            (
                vec![ScalarValue::Int(None), (2.0).into(), 1.into(), (6.0).into()],
                Real::new(6.0).ok(),
            ),
            (vec![(7.0).into()], Real::new(7.0).ok()),
            (vec![0.into(), ScalarValue::Real(None)], None),
            (vec![1.into(), ScalarValue::Real(None)], None),
            (vec![1.into(), (3.5).into()], Real::new(3.5).ok()),
            (vec![2.into(), (3.5).into()], Real::new(3.5).ok()),
            (
                vec![
                    0.into(),
                    ScalarValue::Real(None),
                    ScalarValue::Int(None),
                    ScalarValue::Real(None),
                    (5.5).into(),
                ],
                Real::new(5.5).ok(),
            ),
        ];

        for (args, expected) in cases {
            let mut evaluator = RpnFnScalarEvaluator::new();
            for arg in args {
                evaluator = evaluator.push_param(arg);
            }
            let output = evaluator.evaluate(ScalarFuncSig::CaseWhenReal).unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_case_when_bytes() {
        let cases: Vec<(Vec<ScalarValue>, Option<Bytes>)> = vec![
            (
                vec![
                    1.into(),
                    vec![1, 2, 3].into(),
                    1.into(),
                    vec![4, 5, 6].into(),
                ],
                Some(vec![1, 2, 3]),
            ),
            (
                vec![
                    0.into(),
                    vec![1, 2, 3].into(),
                    1.into(),
                    vec![4, 5, 6].into(),
                ],
                Some(vec![4, 5, 6]),
            ),
        ];

        for (args, expected) in cases {
            let mut evaluator = RpnFnScalarEvaluator::new();
            for arg in args {
                evaluator = evaluator.push_param(arg);
            }
            let output = evaluator.evaluate(ScalarFuncSig::CaseWhenString).unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_if() {
        use std::f64::consts::{E, PI};

        let cases = vec![
            ((Some(0), E, PI), Real::new(PI).ok()),
            ((Some(1), E, PI), Real::new(E).ok()),
            ((None, E, PI), Real::new(PI).ok()),
        ];

        for ((condition, value1, value2), expected) in cases {
            assert_eq!(
                expected,
                RpnFnScalarEvaluator::new()
                    .push_param(condition)
                    .push_param(value1)
                    .push_param(value2)
                    .evaluate(ScalarFuncSig::IfReal)
                    .unwrap()
            );
        }
    }
}
