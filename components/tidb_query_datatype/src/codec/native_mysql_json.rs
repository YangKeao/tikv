// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native Datum.ToMysqlJSON selection, distinct from expression CAST policy.

use std::str::Utf8Error;

use super::{
    native_json_construct::{
        NativeJsonConstructError, native_json_from_duration, native_json_from_f64,
        native_json_from_i64, native_json_from_opaque, native_json_from_string,
        native_json_from_time, native_json_from_u64, native_json_literal,
    },
    native_sql_string::{NativeSqlStringInput, native_sql_string},
    native_string_type::NativeStringTypeCode,
    native_type_name::NativeTypeNameCode,
};

/// Actual effective source type and metadata, without a host policy decision.
#[derive(Clone, Copy, Debug)]
pub struct NativeDatumJsonSource<'a> {
    pub code: NativeTypeNameCode,
    pub string_code: NativeStringTypeCode,
    pub collation: &'a str,
    pub flen: i64,
}

/// Direct byte decoding and fallback SQL-string errors deliberately differ.
#[derive(Debug)]
pub enum NativeDatumJsonError {
    InvalidUtf8(Utf8Error),
    Unsupported,
    Construct(NativeJsonConstructError),
}

/// Converts actual datum storage to a binary-JSON type/payload pair. Unlike
/// typed BinaryJSONValue::Binary construction, an existing JSON datum is cloned
/// without decoding or validating its bytes. Float32 retains its raw f64 value.
pub fn native_datum_to_mysql_json(
    input: NativeSqlStringInput<'_>,
) -> Result<(u8, Vec<u8>), NativeDatumJsonError> {
    use NativeSqlStringInput as I;
    match input {
        I::Json { type_code, value } => Ok((type_code, value.to_vec())),
        I::Int(value) => Ok(native_json_from_i64(value)),
        I::UInt(value) => Ok(native_json_from_u64(value)),
        I::Real(value) | I::Float32(value) => {
            native_json_from_f64(value).map_err(NativeDatumJsonError::Construct)
        }
        I::Decimal(value) => {
            native_json_from_f64(value.to_f64()).map_err(NativeDatumJsonError::Construct)
        }
        I::String(bytes) | I::Bytes(bytes) | I::BinaryLiteral(bytes) | I::Bit(bytes) => {
            let text = std::str::from_utf8(bytes).map_err(NativeDatumJsonError::InvalidUtf8)?;
            Ok(native_json_from_string(text))
        }
        I::Null => Ok(native_json_literal(0)),
        I::Time(value) => Ok(native_json_from_time(value)),
        I::Duration { nanoseconds, fsp } => Ok(native_json_from_duration(nanoseconds, fsp)),
        other => {
            let text = native_sql_string(other).map_err(|_| NativeDatumJsonError::Unsupported)?;
            Ok(native_json_from_string(&text))
        }
    }
}

/// Aggregate-style opaque conversion: Bytes is unconditional, while String
/// requires the native binary-string predicate. This is not expression CAST's
/// typed-string rule. Named fixed CHAR resizes in either direction when flen>0.
pub fn native_datum_to_mysql_json_with_source(
    input: NativeSqlStringInput<'_>,
    source: NativeDatumJsonSource<'_>,
) -> Result<(u8, Vec<u8>), NativeDatumJsonError> {
    let bytes = match input {
        NativeSqlStringInput::Bytes(bytes) => bytes,
        NativeSqlStringInput::String(bytes)
            if source.string_code.is_binary_string(source.collation) =>
        {
            bytes
        }
        other => return native_datum_to_mysql_json(other),
    };
    let mut bytes = bytes.to_vec();
    if source.code == NativeTypeNameCode::Known(254) && source.flen > 0 {
        bytes.resize(source.flen as usize, 0);
    }
    let (NativeTypeNameCode::Known(type_code) | NativeTypeNameCode::Unknown(type_code)) =
        source.code;
    Ok(native_json_from_opaque(type_code, &bytes))
}

#[cfg(test)]
#[test]
fn native_mysql_json_keeps_raw_kinds_error_classes_and_source_resize_policy() {
    use NativeDatumJsonError as E;
    use NativeSqlStringInput as I;
    use NativeTypeNameCode::{Known, Unknown};

    use super::mysql::{
        NativeVectorFloat32,
        time::{NativeTemporalValue, TimeType},
    };
    assert_eq!(
        native_datum_to_mysql_json(I::Null).unwrap(),
        (0x04, vec![0])
    );
    assert_eq!(
        native_datum_to_mysql_json(I::Int(-1)).unwrap(),
        (0x09, vec![0xff; 8])
    );
    assert_eq!(
        native_datum_to_mysql_json(I::UInt(1)).unwrap(),
        (0x0a, vec![1, 0, 0, 0, 0, 0, 0, 0])
    );
    for input in [I::Float32(16_777_217.0), I::Real(16_777_217.0)] {
        assert_eq!(
            native_datum_to_mysql_json(input).unwrap(),
            (0x0b, 16_777_217.0_f64.to_bits().to_le_bytes().to_vec())
        );
    }
    let decimal = super::mysql::NativeDecimalParseRef {
        negative: false,
        digits: b"125",
        scale: 1,
        storage_scale: 2,
        declared_shape: Some((20, 8)),
    };
    assert_eq!(
        native_datum_to_mysql_json(I::Decimal(decimal)).unwrap(),
        (0x0b, 1.3_f64.to_bits().to_le_bytes().to_vec())
    );
    assert!(matches!(
        native_datum_to_mysql_json(I::Real(f64::INFINITY)),
        Err(E::Construct(NativeJsonConstructError::InvalidText))
    ));
    assert_eq!(
        native_datum_to_mysql_json(I::Json {
            type_code: 0x03,
            value: &[0xff]
        })
        .unwrap(),
        (0x03, vec![0xff])
    );
    for input in [
        I::String(&[0xff]),
        I::Bytes(&[0xff]),
        I::BinaryLiteral(&[0xff]),
        I::Bit(&[0xff]),
    ] {
        assert!(matches!(
            native_datum_to_mysql_json(input),
            Err(E::InvalidUtf8(_))
        ));
    }
    for input in [
        I::Enum(&[0xff]),
        I::Set(&[0xff]),
        I::Raw(&[0xff]),
        I::MinNotNull,
        I::MaxValue,
    ] {
        assert!(matches!(
            native_datum_to_mysql_json(input),
            Err(E::Unsupported)
        ));
    }
    for input in [
        I::String(b"ab"),
        I::Bytes(b"ab"),
        I::BinaryLiteral(b"ab"),
        I::Bit(b"ab"),
        I::Enum(b"ab"),
        I::Set(b"ab"),
        I::Raw(b"ab"),
    ] {
        assert_eq!(
            native_datum_to_mysql_json(input).unwrap(),
            (0x0c, vec![2, b'a', b'b'])
        );
    }
    let vector = NativeVectorFloat32::must_create(vec![1.0, 2.0]);
    assert_eq!(
        native_datum_to_mysql_json(I::VectorFloat32(&vector)).unwrap(),
        (0x0c, b"\x05[1,2]".to_vec())
    );
    assert_eq!(
        native_datum_to_mysql_json(I::Time(NativeTemporalValue {
            raw: u64::MAX,
            kind: TimeType::Timestamp,
            fsp: u8::MAX
        }))
        .unwrap(),
        (0x10, vec![0xff; 8])
    );
    assert_eq!(
        native_datum_to_mysql_json(I::Duration {
            nanoseconds: -1,
            fsp: -1
        })
        .unwrap(),
        (0x11, vec![0xff; 12])
    );
    let source = NativeDatumJsonSource {
        code: Known(254),
        string_code: NativeStringTypeCode::String,
        collation: "binary",
        flen: 3,
    };
    assert_eq!(
        native_datum_to_mysql_json_with_source(I::String(b"ab"), source).unwrap(),
        (0x0d, vec![254, 3, b'a', b'b', 0])
    );
    assert_eq!(
        native_datum_to_mysql_json_with_source(I::Bytes(b"abcd"), source).unwrap(),
        (0x0d, vec![254, 3, b'a', b'b', b'c'])
    );
    assert_eq!(
        native_datum_to_mysql_json_with_source(
            I::String(b"ab"),
            NativeDatumJsonSource {
                collation: "BINARY",
                ..source
            }
        )
        .unwrap(),
        (0x0c, vec![2, b'a', b'b'])
    );
    let unknown = NativeDatumJsonSource {
        code: Unknown(254),
        string_code: NativeStringTypeCode::Other(254),
        ..source
    };
    assert_eq!(
        native_datum_to_mysql_json_with_source(I::Bytes(b"ab"), unknown).unwrap(),
        (0x0d, vec![254, 2, b'a', b'b'])
    );
    assert_eq!(
        native_datum_to_mysql_json_with_source(I::String(b"ab"), unknown).unwrap(),
        (0x0c, vec![2, b'a', b'b'])
    );
    let array_effective = NativeDatumJsonSource {
        code: Known(245),
        string_code: NativeStringTypeCode::Other(245),
        ..source
    };
    assert_eq!(
        native_datum_to_mysql_json_with_source(I::Bytes(&[0xff]), array_effective).unwrap(),
        (0x0d, vec![245, 1, 0xff])
    );
    assert!(matches!(
        native_datum_to_mysql_json_with_source(I::String(&[0xff]), array_effective),
        Err(E::InvalidUtf8(_))
    ));
}
