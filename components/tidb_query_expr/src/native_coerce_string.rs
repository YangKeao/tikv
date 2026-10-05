// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native expression string coercion, distinct from SQL stringification.
//! Float formatting and each byte-bearing kind's UTF-8 error remain the
//! expression helper's own policy. All other scalar Display implementations
//! are the same existing datatype SDK implementations used for SQL strings.
use tidb_query_datatype::codec::native_sql_string::{NativeSqlStringInput, native_sql_string};

fn decode(bytes: &[u8], error: &'static str) -> Result<Option<String>, &'static str> {
    std::str::from_utf8(bytes)
        .map(|text| Some(text.to_owned()))
        .map_err(|_| error)
}

/// Coerce actual native storage to expression text. This has no context reads,
/// callbacks, lossy decoding or range normalization. JSON Display errors retain
/// their original to_string panic instead of becoming a new coercion error.
pub fn native_coerce_string(
    input: NativeSqlStringInput<'_>,
) -> Result<Option<String>, &'static str> {
    use NativeSqlStringInput as I;
    match input {
        I::String(bytes) => decode(bytes, "invalid UTF-8 string datum"),
        I::Bytes(bytes) => decode(bytes, "invalid UTF-8 byte datum"),
        I::BinaryLiteral(bytes) | I::Bit(bytes) => decode(bytes, "invalid UTF-8 binary literal"),
        I::Enum(bytes) => decode(bytes, "invalid UTF-8 ENUM name"),
        I::Set(bytes) => decode(bytes, "invalid UTF-8 SET name"),
        I::Raw(bytes) => decode(bytes, "invalid UTF-8 raw datum"),
        I::Real(value) => Ok(Some(value.to_string())),
        I::Float32(value) => Ok(Some((value as f32).to_string())),
        I::Null => Ok(None),
        I::MinNotNull | I::MaxValue => Err("range sentinel string coercion"),
        // Only Int/UInt/Decimal/Duration/Time/Json/Vector remain. Their existing
        // Display produces UTF-8 String storage, so the SQL helper's checked
        // byte-to-string projection cannot return a UTF-8/sentinel error.
        other @ (I::Int(_)
        | I::UInt(_)
        | I::Decimal(_)
        | I::Duration { .. }
        | I::Time(_)
        | I::Json { .. }
        | I::VectorFloat32(_)) => Ok(Some(
            native_sql_string(other).expect("non-text scalar Display produces UTF-8"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use NativeSqlStringInput as I;
    use tidb_query_datatype::codec::mysql::{
        NativeDecimalParseRef, NativeVectorFloat32,
        time::{NativeTemporalValue, TimeType},
    };

    use super::*;
    #[test]
    fn expression_string_coercion_covers_actual_nineteen_storage_kinds() {
        let decimal = NativeDecimalParseRef {
            negative: false,
            digits: b"125",
            scale: 2,
            storage_scale: 2,
            declared_shape: None,
        };
        let vector = NativeVectorFloat32::must_create(vec![1.0, 2.0]);
        let date = NativeTemporalValue {
            raw: 0,
            kind: TimeType::Date,
            fsp: 0,
        };
        for (input, expected) in [
            (I::Null, None),
            (I::Int(-7), Some("-7")),
            (I::UInt(u64::MAX), Some("18446744073709551615")),
            (I::Decimal(decimal), Some("1.25")),
            (I::Real(1.0), Some("1")),
            (I::Float32(1.0), Some("1")),
            (I::String(b"text"), Some("text")),
            (I::Bytes(b"a\0b"), Some("a\0b")),
            (I::BinaryLiteral(b"literal"), Some("literal")),
            (I::Bit(b"bit"), Some("bit")),
            (I::Time(date), Some("0000-00-00")),
            (
                I::Duration {
                    nanoseconds: 1_500_000_000,
                    fsp: 1,
                },
                Some("00:00:01.5"),
            ),
            (I::Enum(b"name"), Some("name")),
            (I::Set(b"a,b"), Some("a,b")),
            (
                I::Json {
                    type_code: 4,
                    value: &[1],
                },
                Some("true"),
            ),
            (I::Raw(b"raw"), Some("raw")),
            (I::VectorFloat32(&vector), Some("[1,2]")),
        ] {
            assert_eq!(native_coerce_string(input).unwrap().as_deref(), expected);
        }
        for input in [I::MinNotNull, I::MaxValue] {
            assert_eq!(
                native_coerce_string(input),
                Err("range sentinel string coercion")
            );
        }
    }
    #[test]
    fn expression_string_coercion_keeps_rust_floats_utf8_classes_and_json_panic_boundary() {
        for (input, expected) in [
            (I::Real(f64::INFINITY), "inf"),
            (I::Real(f64::NEG_INFINITY), "-inf"),
            (I::Real(f64::NAN), "NaN"),
            (I::Real(-0.0), "-0"),
            (I::Real(1e20), "100000000000000000000"),
            (I::Real(16_777_217.0), "16777217"),
            (I::Float32(16_777_217.0), "16777216"),
            (I::Float32(f64::MAX), "inf"),
            (I::Float32(f64::NAN), "NaN"),
            (I::Float32(-0.0), "-0"),
        ] {
            assert_eq!(native_coerce_string(input).unwrap(), Some(expected.into()));
        }
        for (input, error) in [
            (I::String(&[0xff]), "invalid UTF-8 string datum"),
            (I::Bytes(&[0xff]), "invalid UTF-8 byte datum"),
            (I::BinaryLiteral(&[0xff]), "invalid UTF-8 binary literal"),
            (I::Bit(&[0xff]), "invalid UTF-8 binary literal"),
            (I::Enum(&[0xff]), "invalid UTF-8 ENUM name"),
            (I::Set(&[0xff]), "invalid UTF-8 SET name"),
            (I::Raw(&[0xff]), "invalid UTF-8 raw datum"),
        ] {
            assert_eq!(native_coerce_string(input), Err(error));
        }
        assert_eq!(
            native_coerce_string(I::Json {
                type_code: 11,
                value: &[0, 0, 0, 0, 0, 0, 240, 63]
            }),
            Ok(Some("1.0".into()))
        );
        assert_eq!(
            native_coerce_string(I::Json {
                type_code: 3,
                value: &[0xff]
            }),
            Ok(Some(String::new()))
        );
        let inf = [0, 0, 0, 0, 0, 0, 240, 127];
        assert!(
            std::panic::catch_unwind(|| native_coerce_string(I::Json {
                type_code: 11,
                value: &inf
            }))
            .is_err()
        );
    }
}
