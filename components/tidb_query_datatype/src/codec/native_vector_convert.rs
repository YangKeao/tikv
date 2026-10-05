// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native datum-to-vector conversion, separate from expression error policy.

use std::str::Utf8Error;

use super::mysql::{NativeVectorError, NativeVectorFloat32};

/// Actual source storage; Other is any kind unsupported by this conversion.
#[derive(Clone, Copy, Debug)]
pub enum NativeVectorConvertInput<'a> {
    Vector(&'a NativeVectorFloat32),
    String(&'a [u8]),
    Bytes(&'a [u8]),
    Other,
}

/// Keeps UTF-8, vector parsing/dimension errors and unsupported kinds distinct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeVectorConvertError {
    Unsupported,
    InvalidUtf8(Utf8Error),
    Vector(NativeVectorError),
}

/// Converts a non-NULL native source. The caller's outer NULL rule stays
/// separate. Existing vectors are cloned without validating their elements or
/// global dimension limit; only the requested column dimension is checked.
pub fn native_convert_to_vector(
    input: NativeVectorConvertInput<'_>,
    flen: i64,
) -> Result<NativeVectorFloat32, NativeVectorConvertError> {
    let value = match input {
        NativeVectorConvertInput::Vector(value) => value.clone(),
        NativeVectorConvertInput::String(bytes) | NativeVectorConvertInput::Bytes(bytes) => {
            let text = std::str::from_utf8(bytes).map_err(NativeVectorConvertError::InvalidUtf8)?;
            NativeVectorFloat32::parse(text).map_err(NativeVectorConvertError::Vector)?
        }
        NativeVectorConvertInput::Other => return Err(NativeVectorConvertError::Unsupported),
    };
    let expected = (flen != -1).then(|| usize::try_from(flen).unwrap_or(usize::MAX));
    value
        .check_dims_fit_column(expected)
        .map_err(NativeVectorConvertError::Vector)?;
    Ok(value)
}

#[cfg(test)]
#[test]
fn native_vector_conversion_keeps_source_error_order_and_raw_vector_identity() {
    use NativeVectorConvertError as E;
    use NativeVectorConvertInput as I;
    for input in [I::String(b" [1,2.5] "), I::Bytes(b" [1,2.5] ")] {
        assert_eq!(
            native_convert_to_vector(input, 2).unwrap().elements(),
            &[1.0, 2.5]
        );
        assert_eq!(
            native_convert_to_vector(input, -1).unwrap().elements(),
            &[1.0, 2.5]
        );
        let E::Vector(error) = native_convert_to_vector(input, -2).unwrap_err() else {
            panic!("expected dimension error")
        };
        assert_eq!(
            error.to_string(),
            format!(
                "vector has 2 dimensions, does not fit VECTOR({})",
                usize::MAX
            )
        );
    }
    assert!(
        native_convert_to_vector(I::Bytes(b"[]"), 0)
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        native_convert_to_vector(I::String(&[0xff]), 0),
        Err(E::InvalidUtf8(_))
    ));
    assert!(matches!(
        native_convert_to_vector(I::Bytes(&[0xff]), -2),
        Err(E::InvalidUtf8(_))
    ));
    assert_eq!(
        native_convert_to_vector(I::Other, -2).unwrap_err(),
        E::Unsupported
    );
    let E::Vector(error) = native_convert_to_vector(I::String(b"null"), -2).unwrap_err() else {
        panic!("expected parse error")
    };
    assert_eq!(error.to_string(), "Invalid vector text: null");
    let mut raw = NativeVectorFloat32::init(3);
    raw.elements_mut()
        .copy_from_slice(&[f32::from_bits(0x7fc0_0123), f32::INFINITY, -0.0]);
    let copy = native_convert_to_vector(I::Vector(&raw), 3).unwrap();
    assert_eq!(
        copy.elements()
            .iter()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>(),
        vec![0x7fc0_0123, f32::INFINITY.to_bits(), (-0.0_f32).to_bits()]
    );
    assert_ne!(copy.elements().as_ptr(), raw.elements().as_ptr());
    let large = NativeVectorFloat32::init(super::mysql::NATIVE_MAX_VECTOR_DIMENSION + 1);
    assert_eq!(
        native_convert_to_vector(I::Vector(&large), -1)
            .unwrap()
            .len(),
        large.len()
    );
    assert!(matches!(
        native_convert_to_vector(I::Vector(&large), 0),
        Err(E::Vector(_))
    ));
}
