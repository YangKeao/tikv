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

/// Expression byte coercion preserves byte-bearing storage verbatim. Other
/// scalars retain expression Rust Display rather than SQL float formatting.
pub fn native_coerce_bytes(
    input: NativeSqlStringInput<'_>,
) -> Result<Option<Vec<u8>>, &'static str> {
    use NativeSqlStringInput as I;
    match input {
        I::String(bytes)
        | I::Bytes(bytes)
        | I::BinaryLiteral(bytes)
        | I::Bit(bytes)
        | I::Enum(bytes)
        | I::Set(bytes)
        | I::Raw(bytes) => Ok(Some(bytes.to_vec())),
        I::MinNotNull | I::MaxValue => Err("range sentinel byte coercion"),
        other => native_coerce_string(other).map(|value| value.map(String::into_bytes)),
    }
}

/// Demanded string formatting failures, distinct from SQL NULL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeStringCoercionError {
    /// Original expression UTF-8 or sentinel error.
    Unsupported(&'static str),
    /// Actual owned text or formatting work exceeds its finite allowance.
    ResourceLimit,
}
fn resource() -> NativeStringCoercionError {
    NativeStringCoercionError::ResourceLimit
}
fn check(bytes: usize, maximum: usize) -> Result<(), NativeStringCoercionError> {
    if bytes > maximum {
        Err(resource())
    } else {
        Ok(())
    }
}
fn checked_extent(n: usize, m: usize, extra: usize) -> Result<usize, NativeStringCoercionError> {
    n.checked_mul(m)
        .and_then(|n| n.checked_add(extra))
        .ok_or_else(resource)
}
/// Preserve the original kind-specific coercion and gate each actual copy or
/// formatting operation before it runs. NULL does not allocate string storage.
pub fn native_coerce_string_bounded(
    input: NativeSqlStringInput<'_>,
    maximum: usize,
) -> Result<String, NativeStringCoercionError> {
    let work = native_string_coercion_work(input)?;
    check(work, maximum)?;
    let text = crate::native_coerce_string(input)
        .map_err(NativeStringCoercionError::Unsupported)?
        .unwrap_or_default();
    check(text.capacity(), maximum)?;
    Ok(text)
}
/// Borrowed preflight for the original kind-specific formatter. This validates
/// original UTF-8/sentinel failures but neither formats nor allocates storage.
pub fn native_string_coercion_work(
    input: NativeSqlStringInput<'_>,
) -> Result<usize, NativeStringCoercionError> {
    use NativeSqlStringInput as Input;
    // Preserve byte-kind UTF-8 failures BEFORE copying and later format work.
    let bytes = match input {
        Input::String(v) => Some((v, "invalid UTF-8 string datum")),
        Input::Bytes(v) => Some((v, "invalid UTF-8 byte datum")),
        Input::BinaryLiteral(v) | Input::Bit(v) => Some((v, "invalid UTF-8 binary literal")),
        Input::Enum(v) => Some((v, "invalid UTF-8 ENUM name")),
        Input::Set(v) => Some((v, "invalid UTF-8 SET name")),
        Input::Raw(v) => Some((v, "invalid UTF-8 raw datum")),
        _ => None,
    };
    Ok(if let Some((bytes, error)) = bytes {
        std::str::from_utf8(bytes).map_err(|_| NativeStringCoercionError::Unsupported(error))?;
        bytes.len()
    } else {
        match input {
            Input::MinNotNull | Input::MaxValue => {
                return Err(NativeStringCoercionError::Unsupported(
                    "range sentinel string coercion",
                ));
            }
            Input::Null => 0,
            Input::Int(_) | Input::UInt(_) => 128,
            Input::Real(_) | Input::Float32(_) => 4096,
            Input::Decimal(v) => checked_extent(
                v.digits
                    .len()
                    .checked_add(v.scale.max(v.storage_scale) as usize)
                    .ok_or_else(resource)?,
                16,
                1024,
            )?,
            Input::Json { value, .. } => checked_extent(value.len(), 128, 1024)?,
            Input::VectorFloat32(v) => checked_extent(v.len(), 512, 1024)?,
            Input::Duration { fsp, .. } => checked_extent(
                usize::try_from(fsp.max(64)).map_err(|_| resource())?,
                16,
                1024,
            )?,
            Input::Time(_) => 1024,
            _ => unreachable!("byte inputs handled above"),
        }
    })
}

/// Actual byte coercion work: preserve every byte-bearing kind without UTF-8
/// validation; other kinds use the exact expression formatter and errors.
pub fn native_byte_coercion_work(
    input: NativeSqlStringInput<'_>,
) -> Result<usize, NativeStringCoercionError> {
    use NativeSqlStringInput as I;
    match input {
        I::String(v)
        | I::Bytes(v)
        | I::BinaryLiteral(v)
        | I::Bit(v)
        | I::Enum(v)
        | I::Set(v)
        | I::Raw(v) => Ok(v.len()),
        I::MinNotNull | I::MaxValue => Err(NativeStringCoercionError::Unsupported(
            "range sentinel byte coercion",
        )),
        other => native_string_coercion_work(other),
    }
}
/// Bounded original byte coercion, including its nullable result and raw
/// octets.
pub fn native_coerce_bytes_bounded(
    input: NativeSqlStringInput<'_>,
    maximum: usize,
) -> Result<Option<Vec<u8>>, NativeStringCoercionError> {
    check(native_byte_coercion_work(input)?, maximum)?;
    let bytes = native_coerce_bytes(input).map_err(NativeStringCoercionError::Unsupported)?;
    check(bytes.as_ref().map_or(0, Vec::capacity), maximum)?;
    Ok(bytes)
}
/// Finite SQL Display allowance for the original duration argument wrapper.
/// NULL/raw Duration bypass formatting. Invalid byte UTF-8 is deliberately not
/// rejected here: the original SQL formatter/converter owns that disposition.
pub fn native_duration_argument_cast_work(
    input: NativeSqlStringInput<'_>,
) -> Result<usize, NativeStringCoercionError> {
    use NativeSqlStringInput as I;
    let work = match input {
        I::Null | I::Duration { .. } => return Ok(0),
        I::String(v)
        | I::Bytes(v)
        | I::BinaryLiteral(v)
        | I::Bit(v)
        | I::Enum(v)
        | I::Set(v)
        | I::Raw(v) => v.len(),
        I::MinNotNull | I::MaxValue => 128,
        other => native_string_coercion_work(other)?,
    };
    checked_extent(work, 32, 4096)
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
