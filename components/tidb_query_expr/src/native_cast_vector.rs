// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Ordinary VECTOR cast control over the native datatype conversion kernel.
//! Actual NULL retains the original conversion framework's short circuit;
//! caller range guards are unchanged. Type names use the actual effective
//! source code (including ARRAY's JSON override), not an element code or a
//! canonicalized unknown byte. No native conversion callback or target builder
//! is needed here.
use tidb_query_datatype::codec::{
    mysql::NativeVectorFloat32,
    native_type_name::{NativeTypeNameCode, native_type_str},
    native_vector_convert::{
        NativeVectorConvertError, NativeVectorConvertInput, native_convert_to_vector,
    },
};

/// Select the original source name before conversion. Only Unsupported uses
/// that name; UTF-8 and vector errors retain the datatype error's own Display.
/// An omitted width is the original unspecified field length, not zero.
/// None input is actual SQL NULL, distinct from an unsupported non-NULL kind.
pub fn native_cast_vector(
    input: Option<NativeVectorConvertInput<'_>>,
    dimensions: Option<u32>,
    source: Option<NativeTypeNameCode>,
) -> Result<Option<NativeVectorFloat32>, String> {
    let source_name = source.map(native_type_str).unwrap_or("unspecified");
    let Some(input) = input else {
        return Ok(None);
    };
    let flen = dimensions.map(i64::from).unwrap_or(-1);
    native_convert_to_vector(input, flen)
        .map(Some)
        .map_err(|error| match error {
            NativeVectorConvertError::Unsupported => {
                format!("cannot cast from {source_name} to vector")
            }
            NativeVectorConvertError::InvalidUtf8(error) => error.to_string(),
            NativeVectorConvertError::Vector(error) => error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use NativeTypeNameCode as C;
    use NativeVectorConvertInput as I;

    use super::*;
    #[test]
    fn vector_cast_keeps_source_names_raw_clone_dimension_policy_and_error_display() {
        for dimensions in [None, Some(0), Some(u32::MAX)] {
            for source in [None, Some(C::Known(245)), Some(C::Unknown(13))] {
                assert_eq!(native_cast_vector(None, dimensions, source), Ok(None));
            }
        }
        for (source, name) in [
            (None, "unspecified"),
            (Some(C::Known(0)), "unspecified"),
            (Some(C::Unknown(13)), ""),
            (Some(C::Known(14)), ""),
            (Some(C::Known(245)), "json"),
            (Some(C::Known(252)), "text"),
        ] {
            assert_eq!(
                native_cast_vector(Some(I::Other), None, source).unwrap_err(),
                format!("cannot cast from {name} to vector")
            );
        }
        // Effective ARRAY metadata arrives as Known(JSON=245), not its element
        // code; an unknown YEAR-valued byte above remains unnamed.
        let vector = native_cast_vector(Some(I::String(b"[1,2]")), Some(2), None)
            .unwrap()
            .unwrap();
        assert_eq!(vector.to_string(), "[1,2]");
        assert_eq!(
            native_cast_vector(Some(I::Bytes(b"[]")), Some(0), None)
                .unwrap()
                .unwrap()
                .to_string(),
            "[]"
        );
        assert_eq!(
            native_cast_vector(Some(I::Vector(&vector)), None, None)
                .unwrap()
                .unwrap()
                .to_string(),
            "[1,2]"
        );
        // Frozen error contracts: vector_native.rs check_dims_fit_column/parse
        // and the existing strict UTF-8 contract, not another kernel invocation.
        assert_eq!(
            native_cast_vector(Some(I::Vector(&vector)), Some(3), Some(C::Known(3))).unwrap_err(),
            "vector has 2 dimensions, does not fit VECTOR(3)"
        );
        assert_eq!(
            native_cast_vector(Some(I::Bytes(&[0xff])), None, Some(C::Unknown(0))).unwrap_err(),
            "invalid utf-8 sequence of 1 bytes from index 0"
        );
        assert_eq!(
            native_cast_vector(Some(I::String(b"invalid vector")), None, None).unwrap_err(),
            "Invalid vector text: invalid vector"
        );
        let mut raw = NativeVectorFloat32::init(1);
        raw.elements_mut()[0] = f32::from_bits(0x7fc0_1234);
        let cloned = native_cast_vector(Some(I::Vector(&raw)), Some(1), None)
            .unwrap()
            .unwrap();
        assert_eq!(cloned.elements()[0].to_bits(), 0x7fc0_1234); // clone, not validated reconstruction
        let large = NativeVectorFloat32::init(
            tidb_query_datatype::codec::mysql::NATIVE_MAX_VECTOR_DIMENSION + 1,
        );
        let large_copy = native_cast_vector(Some(I::Vector(&large)), None, None)
            .unwrap()
            .unwrap();
        assert_eq!(large_copy.elements().len(), large.elements().len()); // no new dimension cap for an existing vector
    }
}
