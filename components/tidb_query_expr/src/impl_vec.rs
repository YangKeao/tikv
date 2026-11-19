// Copyright 2024 TiKV Project Authors. Licensed under Apache-2.0.

use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::codec::data_type::*;

#[rpn_fn(writer)]
#[inline]
fn vec_as_text(a: VectorFloat32Ref, writer: BytesWriter) -> Result<BytesGuard> {
    Ok(writer.write(Some(Bytes::from(a.to_string()))))
}

#[rpn_fn]
#[inline]
fn vec_dims(arg: VectorFloat32Ref) -> Result<Option<Int>> {
    Ok(Some(arg.len() as Int))
}

#[rpn_fn]
#[inline]
fn vec_l1_distance(a: VectorFloat32Ref, b: VectorFloat32Ref) -> Result<Option<Real>> {
    // TiKV does not support NaN. This turns NaN into null
    Ok(Real::new(a.l1_distance(b)?).ok())
}

#[rpn_fn]
#[inline]
fn vec_l2_distance(a: VectorFloat32Ref, b: VectorFloat32Ref) -> Result<Option<Real>> {
    // TiKV does not support NaN. This turns NaN into null
    Ok(Real::new(a.l2_distance(b)?).ok())
}

#[rpn_fn]
#[inline]
fn vec_negative_inner_product(a: VectorFloat32Ref, b: VectorFloat32Ref) -> Result<Option<Real>> {
    // TiKV does not support NaN. This turns NaN into null
    Ok(Real::new(-a.inner_product(b)?).ok())
}

#[rpn_fn]
#[inline]
fn vec_cosine_distance(a: VectorFloat32Ref, b: VectorFloat32Ref) -> Result<Option<Real>> {
    // TiKV does not support NaN. This turns NaN into null
    Ok(Real::new(a.cosine_distance(b)?).ok())
}

#[rpn_fn]
#[inline]
fn vec_l2_norm(a: VectorFloat32Ref) -> Result<Option<Real>> {
    // TiKV does not support NaN. This turns NaN into null
    Ok(Real::new(a.l2_norm()).ok())
}

/// Uses the native SQL vector parser and its error contract.
#[rpn_fn]
pub fn native_vec_from_text(input: BytesRef) -> Result<Option<VectorFloat32>> {
    use tidb_query_common::error::EvaluateError;

    let invalid = |msg: String| EvaluateError::Custom { code: 1105, msg };
    let text = std::str::from_utf8(input).map_err(|error| invalid(error.to_string()))?;
    let vector =
        tidb_datatype::VectorFloat32::parse(text).map_err(|error| invalid(error.to_string()))?;
    Ok(Some(VectorFloat32::from_f32(vector.elements().to_vec())?))
}

/// CAST AS VECTOR uses the native SQL conversion and declared dimension.
#[rpn_fn(capture = [extra])]
pub fn native_cast_string_as_vector(
    extra: &mut crate::RpnFnCallExtra<'_>,
    input: BytesRef,
) -> Result<Option<VectorFloat32>> {
    use tidb_datatype::{ConversionFlags, Datum, FieldType, FieldTypeCode};
    use tidb_query_common::error::EvaluateError;

    let target = FieldType::new(FieldTypeCode::VectorFloat32)
        .with_flen(i64::from(extra.ret_field_type.get_flen()));
    let value = Datum::new_bytes(input)
        .convert_to(&target, ConversionFlags::default())
        .map_err(|error| EvaluateError::Custom {
            code: 1105,
            msg: error.to_string(),
        })?
        .value;
    let Datum::VectorFloat32(vector) = value else {
        unreachable!("byte input converted to a vector");
    };
    Ok(Some(VectorFloat32::from_f32(vector.elements().to_vec())?))
}

/// Native vector functions use the same algorithms and retain SQL errors.
#[derive(Clone, Copy)]
pub enum NativeVectorDistance {
    L1,
    L2,
    NegativeInnerProduct,
    Cosine,
}

pub fn native_vector_distance_fn_meta() -> crate::RpnFnMeta {
    use tidb_query_common::error::{ErrorInner, EvaluateError};

    crate::RpnFnMeta {
        fn_ptr: |ctx, rows, args, extra, metadata| {
            let Some(distance) = metadata.downcast_ref::<NativeVectorDistance>() else {
                return Err(other_err!("missing vector distance metadata"));
            };
            let kernel = match distance {
                NativeVectorDistance::L1 => vec_l1_distance_fn_meta(),
                NativeVectorDistance::L2 => vec_l2_distance_fn_meta(),
                NativeVectorDistance::NegativeInnerProduct => vec_negative_inner_product_fn_meta(),
                NativeVectorDistance::Cosine => vec_cosine_distance_fn_meta(),
            };
            (kernel.fn_ptr)(ctx, rows, args, extra, &()).map_err(|error| {
                let ErrorInner::Evaluate(EvaluateError::Other(message)) = &*error.0 else {
                    return error;
                };
                // The codec adds its source location to dimension errors.
                let message = message
                    .strip_prefix('[')
                    .and_then(|message| message.split_once("]: "))
                    .map_or(message.as_str(), |(_, text)| text);
                if !message.starts_with("vectors have different dimensions: ") {
                    return error;
                }
                EvaluateError::Custom {
                    code: 1105,
                    msg: message.to_owned(),
                }
                .into()
            })
        },
        ..vec_l1_distance_fn_meta()
    }
}

#[cfg(test)]
mod tests {
    use tipb::ScalarFuncSig;

    use super::*;
    use crate::types::test_util::RpnFnScalarEvaluator;

    // Test cases are ported from pgvector: https://github.com/pgvector/pgvector/blob/master/test/expected/functions.out
    // Copyright (c) 1996-2023, PostgreSQL Global Development Group

    #[test]
    fn test_dims() {
        let cases = vec![
            (vec![], Some(0)),
            (vec![1.0, 2.0], Some(2)),
            (vec![1.0, 2.0, 3.0], Some(3)),
        ];
        for (arg, expected_output) in cases {
            let arg = VectorFloat32::from_f32(arg).unwrap();
            let output: Option<Int> = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::VecDimsSig)
                .unwrap();
            assert_eq!(output, expected_output);
        }
    }

    #[test]
    fn test_l2_norm() {
        let cases = vec![
            (vec![], Some(0.0)),
            (vec![3.0, 4.0], Some(5.0)),
            (vec![0.0, 1.0], Some(1.0)),
        ];

        for (arg, expected_output) in cases {
            let arg = VectorFloat32::from_f32(arg).unwrap();
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::VecL2NormSig)
                .unwrap();
            assert_eq!(output, expected_output.map(|x| Real::new(x).unwrap()));
        }
    }

    #[test]
    fn test_l2_distance() {
        let ok_cases = vec![
            (Some(vec![0.0, 0.0]), Some(vec![3.0, 4.0]), Some(5.0)),
            (Some(vec![0.0, 0.0]), Some(vec![0.0, 1.0]), Some(1.0)),
            (Some(vec![3e38]), Some(vec![-3e38]), Some(f64::INFINITY)),
            (Some(vec![1.0, 2.0]), None, None),
        ];
        for (arg1, arg2, expected_output) in ok_cases {
            let arg1 = arg1.map(|v| VectorFloat32::from_f32(v).unwrap());
            let arg2 = arg2.map(|v| VectorFloat32::from_f32(v).unwrap());
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(arg1)
                .push_param(arg2)
                .evaluate(ScalarFuncSig::VecL2DistanceSig)
                .unwrap();
            assert_eq!(output, expected_output.map(|x| Real::new(x).unwrap()));
        }

        let err_cases = vec![(vec![1.0, 2.0], vec![3.0])];
        for (arg1, arg2) in err_cases {
            let arg1 = VectorFloat32::from_f32(arg1).unwrap();
            let arg2 = VectorFloat32::from_f32(arg2).unwrap();
            let output: Result<Option<Real>> = RpnFnScalarEvaluator::new()
                .push_param(arg1)
                .push_param(arg2)
                .evaluate(ScalarFuncSig::VecL2DistanceSig);
            assert!(output.is_err(), "expected error, got {:?}", output);
        }
    }

    #[test]
    fn test_negative_inner_product() {
        let ok_cases = vec![
            (Some(vec![1.0, 2.0]), Some(vec![3.0, 4.0]), Some(-11.0)),
            (Some(vec![3e38]), Some(vec![3e38]), Some(f64::NEG_INFINITY)),
            (Some(vec![1.0, 2.0]), None, None),
        ];
        for (arg1, arg2, expected_output) in ok_cases {
            let arg1 = arg1.map(|v| VectorFloat32::from_f32(v).unwrap());
            let arg2 = arg2.map(|v| VectorFloat32::from_f32(v).unwrap());
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(arg1)
                .push_param(arg2)
                .evaluate(ScalarFuncSig::VecNegativeInnerProductSig)
                .unwrap();
            assert_eq!(output, expected_output.map(|x| Real::new(x).unwrap()));
        }

        let err_cases = vec![(vec![1.0, 2.0], vec![3.0])];
        for (arg1, arg2) in err_cases {
            let arg1 = VectorFloat32::from_f32(arg1).unwrap();
            let arg2 = VectorFloat32::from_f32(arg2).unwrap();
            let output: Result<Option<Real>> = RpnFnScalarEvaluator::new()
                .push_param(arg1)
                .push_param(arg2)
                .evaluate(ScalarFuncSig::VecNegativeInnerProductSig);
            assert!(output.is_err(), "expected error, got {:?}", output);
        }
    }

    #[test]
    fn test_cosine_distance() {
        let ok_cases = vec![
            (Some(vec![1.0, 2.0]), Some(vec![2.0, 4.0]), Some(0.0)),
            (Some(vec![1.0, 2.0]), Some(vec![0.0, 0.0]), None), // NaN turns to NULL
            (Some(vec![1.0, 1.0]), Some(vec![1.0, 1.0]), Some(0.0)),
            (Some(vec![1.0, 0.0]), Some(vec![0.0, 2.0]), Some(1.0)),
            (Some(vec![1.0, 1.0]), Some(vec![-1.0, -1.0]), Some(2.0)),
            (Some(vec![1.0, 1.0]), Some(vec![1.1, 1.1]), Some(0.0)),
            (Some(vec![1.0, 1.0]), Some(vec![-1.1, -1.1]), Some(2.0)),
            (Some(vec![3e38]), Some(vec![3e38]), None), // NaN turns to NULL
            (Some(vec![1.0, 2.0]), None, None),
        ];
        for (arg1, arg2, expected_output) in ok_cases {
            let arg1 = arg1.map(|v| VectorFloat32::from_f32(v).unwrap());
            let arg2 = arg2.map(|v| VectorFloat32::from_f32(v).unwrap());
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(arg1)
                .push_param(arg2)
                .evaluate(ScalarFuncSig::VecCosineDistanceSig)
                .unwrap();
            assert_eq!(output, expected_output.map(|x| Real::new(x).unwrap()));
        }

        let err_cases = vec![(vec![1.0, 2.0], vec![3.0])];
        for (arg1, arg2) in err_cases {
            let arg1 = VectorFloat32::from_f32(arg1).unwrap();
            let arg2 = VectorFloat32::from_f32(arg2).unwrap();
            let output: Result<Option<Real>> = RpnFnScalarEvaluator::new()
                .push_param(arg1)
                .push_param(arg2)
                .evaluate(ScalarFuncSig::VecCosineDistanceSig);
            assert!(output.is_err(), "expected error, got {:?}", output);
        }
    }

    #[test]
    fn test_l1_distance() {
        let ok_cases = vec![
            (Some(vec![0.0, 0.0]), Some(vec![3.0, 4.0]), Some(7.0)),
            (Some(vec![0.0, 0.0]), Some(vec![0.0, 1.0]), Some(1.0)),
            (Some(vec![3e38]), Some(vec![-3e38]), Some(f64::INFINITY)),
            (Some(vec![1.0, 2.0]), None, None),
        ];
        for (arg1, arg2, expected_output) in ok_cases {
            let arg1 = arg1.map(|v| VectorFloat32::from_f32(v).unwrap());
            let arg2 = arg2.map(|v| VectorFloat32::from_f32(v).unwrap());
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(arg1)
                .push_param(arg2)
                .evaluate(ScalarFuncSig::VecL1DistanceSig)
                .unwrap();
            assert_eq!(output, expected_output.map(|x| Real::new(x).unwrap()));
        }

        let err_cases = vec![(vec![1.0, 2.0], vec![3.0])];
        for (arg1, arg2) in err_cases {
            let arg1 = VectorFloat32::from_f32(arg1).unwrap();
            let arg2 = VectorFloat32::from_f32(arg2).unwrap();
            let output: Result<Option<Real>> = RpnFnScalarEvaluator::new()
                .push_param(arg1)
                .push_param(arg2)
                .evaluate(ScalarFuncSig::VecL1DistanceSig);
            assert!(output.is_err(), "expected error, got {:?}", output);
        }
    }
}
