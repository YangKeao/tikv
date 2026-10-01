// Copyright 2024 TiKV Project Authors. Licensed under Apache-2.0.

use tidb_query_codegen::rpn_fn;
use tidb_query_common::{Result, error::EvaluateError};
use tidb_query_datatype::codec::{
    data_type::*,
    mysql::{NativeVectorError, NativeVectorFloat32},
};

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

// Native-only recipes retain actual typed vector inputs. Ordinary wire dispatch
// continues to use the seven original kernels above.
pub(crate) fn get_native_vec_dims_fn_meta() -> crate::RpnFnMeta {
    vec_dims_fn_meta()
}

fn native_vector_error(error: NativeVectorError) -> tidb_query_common::Error {
    EvaluateError::Caused(Box::new(error)).into()
}

fn native_vector_real(value: f64) -> Option<Bytes> {
    if value.is_nan() {
        None
    } else {
        Some(value.to_bits().to_le_bytes().to_vec())
    }
}

#[rpn_fn]
#[inline]
fn get_native_vec_as_text(arg: VectorFloat32Ref) -> Result<Option<Bytes>> {
    Ok(Some(arg.to_native_string().into_bytes()))
}

#[rpn_fn]
#[inline]
fn get_native_vec_l1_distance(a: VectorFloat32Ref, b: VectorFloat32Ref) -> Result<Option<Bytes>> {
    Ok(native_vector_real(
        a.native_l1_distance(b).map_err(native_vector_error)?,
    ))
}

#[rpn_fn]
#[inline]
fn get_native_vec_l2_distance(a: VectorFloat32Ref, b: VectorFloat32Ref) -> Result<Option<Bytes>> {
    Ok(native_vector_real(
        a.native_l2_distance(b).map_err(native_vector_error)?,
    ))
}

#[rpn_fn]
#[inline]
fn get_native_vec_negative_inner_product(
    a: VectorFloat32Ref,
    b: VectorFloat32Ref,
) -> Result<Option<Bytes>> {
    Ok(native_vector_real(
        -a.native_inner_product(b).map_err(native_vector_error)?,
    ))
}

#[rpn_fn]
#[inline]
fn get_native_vec_cosine_distance(
    a: VectorFloat32Ref,
    b: VectorFloat32Ref,
) -> Result<Option<Bytes>> {
    Ok(native_vector_real(
        a.native_cosine_distance(b).map_err(native_vector_error)?,
    ))
}

#[rpn_fn]
#[inline]
fn get_native_vec_l2_norm(arg: VectorFloat32Ref) -> Result<Option<Bytes>> {
    Ok(native_vector_real(arg.native_l2_norm()))
}

#[rpn_fn]
#[inline]
fn get_native_vec_from_text(arg: BytesRef) -> Result<Option<Bytes>> {
    // The dedicated result role consumes an actual standard-LE vector value,
    // not a vector chunk or a control envelope.
    Ok(Some(
        NativeVectorFloat32::parse_bytes(arg)
            .map_err(native_vector_error)?
            .serialize(),
    ))
}

fn native_vector_from_packed(arg: VectorFloat32Ref) -> NativeVectorFloat32 {
    // The existing typed input has no dimension header: it stores native-endian
    // f32 cells. Copy layout only, preserving mutated NaN/Inf bits and empty or
    // uncapped dimensions. create()/text parsing would add a different policy.
    let packed = arg.to_owned();
    let mut value = NativeVectorFloat32::init(arg.len());
    for (element, bytes) in value
        .elements_mut()
        .iter_mut()
        .zip(packed.value.chunks_exact(4))
    {
        *element = f32::from_ne_bytes(bytes.try_into().expect("one actual packed f32 cell"));
    }
    value
}

macro_rules! native_vector_binary_recipe {
    ($name:ident, $method:ident) => {
        #[rpn_fn]
        fn $name(lhs: VectorFloat32Ref, rhs: VectorFloat32Ref) -> Result<Option<Bytes>> {
            let lhs = native_vector_from_packed(lhs);
            let rhs = native_vector_from_packed(rhs);
            // Reuse the full public arithmetic policy and its actual typed
            // cause, then the same real LE output protocol as VecFromText.
            Ok(Some(
                lhs.$method(&rhs).map_err(native_vector_error)?.serialize(),
            ))
        }
    };
}

native_vector_binary_recipe!(add_vector_native, add);
native_vector_binary_recipe!(sub_vector_native, sub);
native_vector_binary_recipe!(mul_vector_native, mul);

#[rpn_fn(nullable)]
#[inline]
fn get_native_vec_real_null(arg: Option<&Int>) -> Result<Option<Bytes>> {
    match arg {
        None => Ok(None),
        Some(_) => Err(other_err!(
            "Native vector NULL witness must be an actual NULL"
        )),
    }
}

#[cfg(test)]
mod binary_native_tests {
    use tidb_query_common::error::ErrorInner;

    use super::*;

    #[test]
    fn native_binary_vectors_return_actual_le_values_and_causes() {
        type Kernel =
            for<'a, 'b> fn(VectorFloat32Ref<'a>, VectorFloat32Ref<'b>) -> Result<Option<Bytes>>;
        let left = NativeVectorFloat32::must_create(vec![1.0, -2.0]).into_wire_raw();
        let right = NativeVectorFloat32::must_create(vec![2.0, 3.0]).into_wire_raw();
        for (kernel, expected) in [
            (add_vector_native as Kernel, [3.0_f32, 1.0]),
            (sub_vector_native as Kernel, [-1.0, -5.0]),
            (mul_vector_native as Kernel, [2.0, -6.0]),
        ] {
            let mut expected_bytes = 2_u32.to_le_bytes().to_vec();
            for element in expected {
                expected_bytes.extend_from_slice(&element.to_bits().to_le_bytes());
            }
            assert_eq!(
                kernel(left.as_ref(), right.as_ref()).unwrap(),
                Some(expected_bytes)
            );
        }
        let empty = NativeVectorFloat32::init(0).into_wire_raw();
        assert_eq!(
            add_vector_native(empty.as_ref(), empty.as_ref()).unwrap(),
            Some(vec![0; 4])
        );
        let mismatch = sub_vector_native(left.as_ref(), empty.as_ref()).unwrap_err();
        let ErrorInner::Evaluate(EvaluateError::Caused(cause)) = mismatch.0.as_ref() else {
            panic!("lost vector cause");
        };
        assert_eq!(
            cause
                .downcast_ref::<NativeVectorError>()
                .unwrap()
                .to_string(),
            "vectors have different dimensions: 2 and 0"
        );
        // The layout copy must not substitute create()'s earlier finite check.
        let mut raw = NativeVectorFloat32::init(1);
        raw.elements_mut()[0] = f32::NAN;
        let raw = raw.into_wire_raw();
        let zero = NativeVectorFloat32::init(1).into_wire_raw();
        let invalid = mul_vector_native(raw.as_ref(), zero.as_ref()).unwrap_err();
        let ErrorInner::Evaluate(EvaluateError::Caused(cause)) = invalid.0.as_ref() else {
            panic!("lost raw vector cause");
        };
        assert_eq!(
            cause
                .downcast_ref::<NativeVectorError>()
                .unwrap()
                .to_string(),
            "value out of range: NaN"
        );
    }
}

#[cfg(test)]
mod native_tests {
    use tidb_query_common::error::ErrorInner;

    use super::*;

    fn assert_real_bits(result: Result<Option<Bytes>>, expected: Option<u64>) {
        let actual = result
            .unwrap()
            .map(|bytes| u64::from_le_bytes(bytes.try_into().expect("exact native IEEE754 width")));
        assert_eq!(actual, expected);
    }

    fn native_cause(error: &tidb_query_common::Error) -> &NativeVectorError {
        match error.0.as_ref() {
            ErrorInner::Evaluate(EvaluateError::Caused(cause)) => cause
                .downcast_ref::<NativeVectorError>()
                .expect("the actual shared native vector cause"),
            _ => panic!("unexpected vector error: {error:?}"),
        }
    }

    #[test]
    fn test_native_vector_source_literals_and_serialized_shape() {
        // Literal scalar answers from the original native vec.rs and the wire
        // pgvector cases below; expected values are never recorded from a leaf.
        let pair = VectorFloat32::from_f32(vec![1.0, 2.0]).unwrap();
        let right = VectorFloat32::from_f32(vec![3.0, 5.0]).unwrap();
        let zero = VectorFloat32::from_f32(vec![0.0, 0.0]).unwrap();
        let norm = VectorFloat32::from_f32(vec![3.0, 4.0]).unwrap();
        assert_eq!(vec_dims(pair.as_ref()).unwrap(), Some(2));
        assert_eq!(
            get_native_vec_as_text(pair.as_ref()).unwrap(),
            Some(b"[1,2]".to_vec())
        );
        assert_real_bits(
            get_native_vec_l1_distance(pair.as_ref(), right.as_ref()),
            Some(5.0_f64.to_bits()),
        );
        assert_real_bits(
            get_native_vec_l2_distance(zero.as_ref(), norm.as_ref()),
            Some(5.0_f64.to_bits()),
        );
        assert_real_bits(
            get_native_vec_negative_inner_product(pair.as_ref(), norm.as_ref()),
            Some((-11.0_f64).to_bits()),
        );
        assert_real_bits(
            get_native_vec_l2_norm(norm.as_ref()),
            Some(5.0_f64.to_bits()),
        );
        let x = VectorFloat32::from_f32(vec![1.0, 0.0]).unwrap();
        let y = VectorFloat32::from_f32(vec![0.0, 2.0]).unwrap();
        assert_real_bits(
            get_native_vec_cosine_distance(x.as_ref(), y.as_ref()),
            Some(1.0_f64.to_bits()),
        );
        // Hand-derived standard LE count + f32 bits, not a control envelope.
        assert_eq!(
            get_native_vec_from_text(b"[1,2]").unwrap(),
            Some(vec![2, 0, 0, 0, 0, 0, 128, 63, 0, 0, 0, 64])
        );
        assert_eq!(
            get_native_vec_from_text(b"[-0]").unwrap(),
            Some(vec![1, 0, 0, 0, 0, 0, 0, 128])
        );
    }

    #[test]
    fn test_native_vector_null_nonfinite_and_signed_zero() {
        // These overflow/zero-norm answers already occur in the wire fixtures.
        let large = VectorFloat32::from_f32(vec![3e38]).unwrap();
        let negative = VectorFloat32::from_f32(vec![-3e38]).unwrap();
        for result in [
            get_native_vec_l1_distance(large.as_ref(), negative.as_ref()),
            get_native_vec_l2_distance(large.as_ref(), negative.as_ref()),
        ] {
            assert_real_bits(result, Some(f64::INFINITY.to_bits()));
        }
        assert_real_bits(
            get_native_vec_negative_inner_product(large.as_ref(), large.as_ref()),
            Some(f64::NEG_INFINITY.to_bits()),
        );
        assert_real_bits(
            get_native_vec_cosine_distance(large.as_ref(), large.as_ref()),
            None,
        );
        let zero = VectorFloat32::from_f32(vec![0.0]).unwrap();
        assert_real_bits(
            get_native_vec_cosine_distance(zero.as_ref(), large.as_ref()),
            None,
        );
        // Hand-derived raw-value policies: mutation is not construction, and
        // negating the empty +0 inner sum must preserve the resulting -0 bits.
        let empty = NativeVectorFloat32::default().into_wire_raw();
        assert_real_bits(
            get_native_vec_negative_inner_product(empty.as_ref(), empty.as_ref()),
            Some(0x8000_0000_0000_0000),
        );
        let mut raw = NativeVectorFloat32::init(1);
        raw.elements_mut()[0] = f32::INFINITY;
        let infinity = raw.clone().into_wire_raw();
        assert_real_bits(
            get_native_vec_l2_norm(infinity.as_ref()),
            Some(f64::INFINITY.to_bits()),
        );
        raw.elements_mut()[0] = f32::from_bits(0x7fc0_0042);
        let nan = raw.into_wire_raw();
        assert_real_bits(get_native_vec_l2_norm(nan.as_ref()), None);
        assert_eq!(get_native_vec_real_null(None).unwrap(), None);
        assert!(get_native_vec_real_null(Some(&0)).is_err());
    }

    #[test]
    fn test_native_vector_preserves_actual_typed_errors() {
        let left = VectorFloat32::from_f32(vec![1.0]).unwrap();
        let right = VectorFloat32::from_f32(vec![1.0, 2.0]).unwrap();
        for result in [
            get_native_vec_l1_distance(left.as_ref(), right.as_ref()),
            get_native_vec_l2_distance(left.as_ref(), right.as_ref()),
            get_native_vec_negative_inner_product(left.as_ref(), right.as_ref()),
            get_native_vec_cosine_distance(left.as_ref(), right.as_ref()),
        ] {
            // Original native vec.rs diagnostic, checked after the worker errs.
            assert_eq!(
                native_cause(&result.unwrap_err()).to_string(),
                "vectors have different dimensions: 1 and 2"
            );
        }
        // Strict UTF-8 and invalid shapes are shared parser causes, not NULL or
        // an independently rebuilt frontend error. No new message oracle.
        for input in [&b"[1,]"[..], &b"null"[..], &[0xff][..]] {
            let error = get_native_vec_from_text(input).unwrap_err();
            let _ = native_cause(&error);
        }
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
