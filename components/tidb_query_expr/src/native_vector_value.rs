// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Actual vector SQL values over the original native datatype primitives.
use tidb_query_datatype::codec::mysql::{NativeVectorError, NativeVectorFloat32};
/// Original native vector SQL signature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeVectorOperation {
    /// Vector dimension count.
    Dims,
    /// Native L1 distance.
    L1Distance,
    /// Native L2 distance.
    L2Distance,
    /// Native negative inner product.
    NegativeInnerProduct,
    /// Native cosine distance.
    CosineDistance,
    /// Native L2 norm.
    L2Norm,
    /// Strict native vector text parsing.
    FromText,
    /// Native vector text rendering.
    AsText,
}
impl NativeVectorOperation {
    /// Original function spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Dims => "vec_dims",
            Self::L1Distance => "vec_l1_distance",
            Self::L2Distance => "vec_l2_distance",
            Self::NegativeInnerProduct => "vec_negative_inner_product",
            Self::CosineDistance => "vec_cosine_distance",
            Self::L2Norm => "vec_l2_norm",
            Self::FromText => "vec_from_text",
            Self::AsText => "vec_as_text",
        }
    }
    /// Original eager source arity.
    pub fn arity(self) -> usize {
        if matches!(
            self,
            Self::L1Distance | Self::L2Distance | Self::NegativeInnerProduct | Self::CosineDistance
        ) {
            2
        } else {
            1
        }
    }
}
/// Finite work, contract and actual native vector SQL errors remain distinct.
#[derive(Debug)]
pub enum NativeVectorValueError {
    /// Work refused before parsing/rendering/scanning.
    Resource,
    /// An actual-value entry was used with an incompatible operation.
    Contract,
    /// Original native datatype error and diagnostic.
    Vector(NativeVectorError),
}
/// Real SQL output without an expression program or encoded carrier.
#[derive(Debug)]
pub enum NativeVectorValue {
    /// Actual NULL, including the original NaN-to-NULL policy.
    Null,
    /// Signed dimension count.
    Int(i64),
    /// Actual IEEE754 metric, preserving infinity and signed zero.
    Real(f64),
    /// Actual native vector text.
    Text(String),
}
use NativeVectorValueError as E;
/// Bound only demanded native work, not existing borrowed storage.
pub fn native_vector_work(maximum: usize, n: usize) -> Result<(), E> {
    if n > maximum {
        Err(E::Resource)
    } else {
        Ok(())
    }
}
fn extent(maximum: usize, n: usize, mul: usize) -> Result<(), E> {
    native_vector_work(
        maximum,
        n.checked_mul(mul)
            .and_then(|n| n.checked_add(4096))
            .ok_or(E::Resource)?,
    )
}
/// Same strict UTF8/parser owner as the explicit native wire worker.
pub fn native_vector_from_text_value(
    input: Option<&[u8]>,
    maximum: usize,
) -> Result<Option<NativeVectorFloat32>, E> {
    native_vector_work(maximum, 0)?;
    let Some(input) = input else { return Ok(None) };
    extent(maximum, input.len(), 128)?;
    NativeVectorFloat32::parse_bytes(input)
        .map(Some)
        .map_err(E::Vector)
}
/// Borrow actual vectors. Conversion and its original left-to-right NULL demand
/// belong to the caller; this entry owns original metric/text result policy.
pub fn native_vector_value(
    op: NativeVectorOperation,
    left: Option<&NativeVectorFloat32>,
    right: Option<&NativeVectorFloat32>,
    maximum: usize,
) -> Result<NativeVectorValue, E> {
    use NativeVectorOperation as O;
    use NativeVectorValue as V;
    native_vector_work(maximum, 0)?;
    if op == O::FromText {
        return Err(E::Contract);
    }
    let Some(left) = left else { return Ok(V::Null) };
    if op == O::Dims {
        return Ok(V::Int(left.len() as i64));
    }
    if op == O::AsText {
        extent(maximum, left.len(), 128)?;
        return Ok(V::Text(left.to_string()));
    }
    let right = if op.arity() == 2 {
        let Some(right) = right else {
            return Ok(V::Null);
        };
        Some(right)
    } else {
        None
    };
    // A dimension mismatch is a constant-time original error, before any scan.
    if right.is_none_or(|right| left.len() == right.len()) {
        extent(maximum, left.len(), 64)?;
    }
    let value = match op {
        O::L1Distance => left.l1_distance(right.ok_or(E::Contract)?),
        O::L2Distance => left.l2_distance(right.ok_or(E::Contract)?),
        O::NegativeInnerProduct => left.negative_inner_product(right.ok_or(E::Contract)?),
        O::CosineDistance => left.cosine_distance(right.ok_or(E::Contract)?),
        O::L2Norm => Ok(left.l2_norm()),
        _ => return Err(E::Contract),
    }
    .map_err(E::Vector)?;
    Ok(if value.is_nan() {
        V::Null
    } else {
        V::Real(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_vector_values_keep_native_bits_null_dimension_errors_and_work() {
        use NativeVectorOperation as O;
        use NativeVectorValue as V;
        let a = NativeVectorFloat32::must_create(vec![3.0, 4.0]);
        let b = NativeVectorFloat32::must_create(vec![0.0, 0.0]);
        for (op, expected) in [
            (O::L1Distance, 7.0_f64),
            (O::L2Distance, 5.0),
            (O::NegativeInnerProduct, -0.0),
            (O::L2Norm, 5.0),
        ] {
            let V::Real(value) = native_vector_value(op, Some(&a), Some(&b), 8192).unwrap() else {
                panic!()
            };
            assert_eq!(value.to_bits(), expected.to_bits());
        }
        assert!(matches!(
            native_vector_value(O::CosineDistance, Some(&a), Some(&b), 8192),
            Ok(V::Null)
        ));
        for op in [
            O::Dims,
            O::L1Distance,
            O::L2Distance,
            O::NegativeInnerProduct,
            O::CosineDistance,
            O::L2Norm,
            O::AsText,
        ] {
            assert!(matches!(
                native_vector_value(op, None, None, 0),
                Ok(V::Null)
            ));
            assert!(native_vector_value(op, None, None, usize::MAX).is_ok());
        }
        assert!(matches!(
            native_vector_value(O::FromText, None, None, 0),
            Err(E::Contract)
        ));
        assert!(matches!(
            native_vector_value(O::L2Norm, Some(&a), None, 0),
            Err(E::Resource)
        ));
        let huge = NativeVectorFloat32::init(1 << 18);
        assert!(matches!(
            native_vector_value(O::Dims, Some(&huge), None, 0),
            Ok(V::Int(262144))
        ));
        assert!(matches!(
            native_vector_value(O::AsText, Some(&huge), None, 8192),
            Err(E::Resource)
        ));
        assert!(matches!(
            native_vector_value(O::L2Distance, Some(&huge), None, 0),
            Ok(V::Null)
        ));
        let Err(E::Vector(e)) = native_vector_value(O::L2Distance, Some(&huge), Some(&a), 0) else {
            panic!()
        };
        assert_eq!(
            e.to_string(),
            "vectors have different dimensions: 262144 and 2"
        );
        assert!(matches!(native_vector_from_text_value(None, 0), Ok(None)));
        assert!(matches!(
            native_vector_from_text_value(Some(&vec![b'x'; 1 << 20]), 8192),
            Err(E::Resource)
        ));
        let parsed = native_vector_from_text_value(Some(b"[-0,1e-45]"), 8192)
            .unwrap()
            .unwrap();
        assert_eq!(parsed.elements()[0].to_bits(), (-0.0f32).to_bits());
        assert_eq!(parsed.elements()[1].to_bits(), 1);
        let Err(E::Vector(e)) = native_vector_from_text_value(Some(&[255]), 8192) else {
            panic!()
        };
        assert_eq!(
            e.to_string(),
            "invalid utf-8 sequence of 1 bytes from index 0"
        );
        let mut raw = NativeVectorFloat32::init(1);
        for bits in [0x7fc01234, 0x7f800000, 0xff800000, 0x80000000] {
            raw.elements_mut()[0] = f32::from_bits(bits);
            let actual = native_vector_value(O::L2Norm, Some(&raw), None, 8192).unwrap();
            if bits == 0x7fc01234 {
                assert!(matches!(actual, V::Null));
            } else {
                let V::Real(actual) = actual else { panic!() };
                assert_eq!(actual.to_bits(), raw.l2_norm().to_bits());
            }
            let V::Text(actual) = native_vector_value(O::AsText, Some(&raw), None, 8192).unwrap()
            else {
                panic!()
            };
            assert_eq!(actual, raw.to_string());
        }
    }
}
