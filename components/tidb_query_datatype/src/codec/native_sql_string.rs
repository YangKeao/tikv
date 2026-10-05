// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native Datum.ToString byte/string selection. These are borrowed source
//! views, not validated or normalized replacements. General diagnostic/literal
//! selectors remain outside this module; only their shared scientific float
//! primitive is exposed alongside the SQL byte/string operations.
use std::{fmt, str::Utf8Error};

use super::mysql::{
    Decimal, Duration, NativeDecimalParseRef, NativeVectorFloat32, Time,
    json::write_native_binary_json_text,
    time::{NativeTemporalValue, TimeType},
};

#[derive(Clone, Copy, Debug)]
pub enum NativeSqlStringInput<'a> {
    Int(i64),
    UInt(u64),
    Real(f64),
    Float32(f64),
    Decimal(NativeDecimalParseRef<'a>),
    String(&'a [u8]),
    Bytes(&'a [u8]),
    BinaryLiteral(&'a [u8]),
    Bit(&'a [u8]),
    Duration { nanoseconds: i64, fsp: i64 },
    Enum(&'a [u8]),
    Set(&'a [u8]),
    Time(NativeTemporalValue),
    Json { type_code: u8, value: &'a [u8] },
    Raw(&'a [u8]),
    VectorFloat32(&'a NativeVectorFloat32),
    Null,
    MinNotNull,
    MaxValue,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeSqlStringError {
    InvalidUtf8(Utf8Error),
    MinNotNull,
    MaxValue,
}

// Keep the original Display::to_string boundary, particularly for root JSON
// nonfinite doubles whose fmt::Error must retain the standard to_string panic.
struct NativeSqlDisplay<'a>(NativeSqlStringInput<'a>);
impl fmt::Display for NativeSqlDisplay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        use NativeSqlStringInput as I;
        match self.0 {
            I::Decimal(value) => formatter.write_str(&Decimal::native_format_visible(
                value.negative,
                value.digits,
                value.scale,
                value.storage_scale,
            )),
            I::Duration { nanoseconds, fsp } => {
                Duration::write_native_display(nanoseconds, fsp, formatter)
            }
            I::Time(value) => Time::write_native_core_display(
                value.raw,
                value.kind == TimeType::Date,
                value.fsp,
                formatter,
            ),
            I::Json { type_code, value } => {
                write_native_binary_json_text(formatter, type_code, value)
            }
            I::VectorFloat32(value) => fmt::Display::fmt(value, formatter),
            _ => unreachable!("structured SQL display input"),
        }
    }
}
fn decode_bytes(bytes: &[u8]) -> Result<String, NativeSqlStringError> {
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(NativeSqlStringError::InvalidUtf8)
}

/// Byte-authoritative SQL rendering. Only Raw validates UTF-8 here, including
/// its original temporary String allocation before cloning the source bytes.
/// Ordinary strings, bytes, ENUM/SET names and literals remain arbitrary bytes.
pub fn native_sql_bytes(input: NativeSqlStringInput<'_>) -> Result<Vec<u8>, NativeSqlStringError> {
    use NativeSqlStringInput as I;
    Ok(match input {
        I::Int(value) => value.to_string().into_bytes(),
        I::UInt(value) => value.to_string().into_bytes(),
        I::Real(value) => format_go_float_f(value).into_bytes(),
        I::Float32(value) => format_go_float_f(value as f32).into_bytes(),
        I::String(bytes)
        | I::Bytes(bytes)
        | I::BinaryLiteral(bytes)
        | I::Bit(bytes)
        | I::Enum(bytes)
        | I::Set(bytes) => bytes.to_vec(),
        I::Decimal(_) | I::Duration { .. } | I::Time(_) | I::Json { .. } | I::VectorFloat32(_) => {
            NativeSqlDisplay(input).to_string().into_bytes()
        }
        I::Raw(bytes) => {
            decode_bytes(bytes)?;
            bytes.to_vec()
        }
        I::Null => Vec::new(),
        I::MinNotNull => return Err(NativeSqlStringError::MinNotNull),
        I::MaxValue => return Err(NativeSqlStringError::MaxValue),
    })
}
/// Strict UTF-8 projection of the byte operation, not a lossy formatter.
pub fn native_sql_string(input: NativeSqlStringInput<'_>) -> Result<String, NativeSqlStringError> {
    let bytes = native_sql_bytes(input)?;
    decode_bytes(&bytes)
}

trait GoScientificFloat: fmt::Display + fmt::LowerExp + Copy {
    fn special(self) -> Option<&'static str>;
}
impl GoScientificFloat for f32 {
    fn special(self) -> Option<&'static str> {
        if self.is_nan() {
            Some("NaN")
        } else if self == Self::INFINITY {
            Some("+Inf")
        } else if self == Self::NEG_INFINITY {
            Some("-Inf")
        } else {
            None
        }
    }
}
impl GoScientificFloat for f64 {
    fn special(self) -> Option<&'static str> {
        if self.is_nan() {
            Some("NaN")
        } else if self == Self::INFINITY {
            Some("+Inf")
        } else if self == Self::NEG_INFINITY {
            Some("-Inf")
        } else {
            None
        }
    }
}
fn format_go_float_f<T: GoScientificFloat>(value: T) -> String {
    value
        .special()
        .map_or_else(|| value.to_string(), str::to_owned)
}
fn format_go_float_e<T: GoScientificFloat>(value: T) -> String {
    if let Some(special) = value.special() {
        return special.to_owned();
    }
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("Rust scientific float contains an exponent");
    let exponent: i32 = exponent.parse().expect("Rust float exponent is numeric");
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exponent.unsigned_abs())
}
/// Existing scientific literal-format primitive only; callers still own their
/// original literal/restore selector. Float32 narrows before either formatting
/// or special-value classification.
pub fn native_sql_float32_scientific(value: f64) -> String {
    format_go_float_e(value as f32)
}
/// Existing scientific primitive for a true binary64 source.
pub fn native_sql_float64_scientific(value: f64) -> String {
    format_go_float_e(value)
}

#[cfg(test)]
mod tests {
    use NativeSqlStringError as E;
    use NativeSqlStringInput as I;

    use super::*;
    #[test]
    fn sql_selector_keeps_all_source_kinds_raw_validation_and_borrowed_structures() {
        let vector = NativeVectorFloat32::default();
        let integer_json = 7_i64.to_le_bytes();
        let decimal = NativeDecimalParseRef {
            negative: false,
            digits: b"12345",
            scale: 2,
            storage_scale: 4,
            declared_shape: Some((10, 2)),
        };
        let cases = [
            (I::Int(-2), b"-2".as_slice()),
            (I::UInt(u64::MAX), b"18446744073709551615".as_slice()),
            (I::Real(-0.0), b"-0".as_slice()),
            (I::Float32(-3.1111111), b"-3.1111112".as_slice()),
            (I::Decimal(decimal), b"1.23".as_slice()),
            (I::String(b"a\0b"), b"a\0b".as_slice()),
            (I::Bytes(&[0xff]), [0xff].as_slice()),
            (I::BinaryLiteral(&[0xfe]), [0xfe].as_slice()),
            (I::Bit(&[0xfd]), [0xfd].as_slice()),
            (I::Enum(&[0xfc]), [0xfc].as_slice()),
            (I::Set(&[0xfb]), [0xfb].as_slice()),
            (
                I::Duration {
                    nanoseconds: -1,
                    fsp: -2,
                },
                b"-00:00:00".as_slice(),
            ),
            (
                I::Time(NativeTemporalValue {
                    raw: 0,
                    kind: TimeType::Date,
                    fsp: u8::MAX,
                }),
                b"0000-00-00".as_slice(),
            ),
            (
                I::Json {
                    type_code: 0x09,
                    value: &integer_json,
                },
                b"7".as_slice(),
            ),
            (I::Raw(b"raw\0"), b"raw\0".as_slice()),
            (I::VectorFloat32(&vector), b"[]".as_slice()),
            (I::Null, b"".as_slice()),
        ];
        for (input, expected) in cases {
            assert_eq!(native_sql_bytes(input).unwrap(), expected);
            match std::str::from_utf8(expected) {
                Ok(text) => assert_eq!(native_sql_string(input).unwrap(), text),
                Err(error) => assert_eq!(native_sql_string(input), Err(E::InvalidUtf8(error))),
            }
        }
        assert_eq!(native_sql_bytes(I::MinNotNull), Err(E::MinNotNull));
        assert_eq!(native_sql_string(I::MaxValue), Err(E::MaxValue));
        let invalid = [b'a', 0xe2, 0x82];
        let error = String::from_utf8(invalid.to_vec())
            .unwrap_err()
            .utf8_error();
        assert_eq!(error.valid_up_to(), 1);
        assert_eq!(error.error_len(), None);
        assert_eq!(
            native_sql_bytes(I::Raw(&invalid)),
            Err(E::InvalidUtf8(error))
        );
        assert_eq!(
            native_sql_string(I::Raw(&invalid)),
            Err(E::InvalidUtf8(error))
        );
        assert_eq!(native_sql_bytes(I::Bytes(&invalid)).unwrap(), invalid);
        assert_eq!(
            native_sql_string(I::Bytes(&invalid)),
            Err(E::InvalidUtf8(error))
        );
        let mut raw_vector = NativeVectorFloat32::init(2);
        raw_vector
            .elements_mut()
            .copy_from_slice(&[f32::NAN, f32::INFINITY]);
        assert_eq!(
            native_sql_string(I::VectorFloat32(&raw_vector)).unwrap(),
            "[NaN,inf]"
        ); // borrowed raw bits bypass constructor validation and retain vector-specific spelling
        // Raw metadata is not reconstructed through DATE/FSP validation.
        assert!(
            std::panic::catch_unwind(|| native_sql_bytes(I::Time(NativeTemporalValue {
                raw: 0,
                kind: TimeType::DateTime,
                fsp: 7
            })))
            .is_err()
        );
        assert!(
            std::panic::catch_unwind(|| native_sql_bytes(I::Duration {
                nanoseconds: 0,
                fsp: 7
            }))
            .is_err()
        );
        // Malformed containers retain their original empty display, while a
        // root nonfinite JSON number retains the actual to_string panic.
        assert_eq!(
            native_sql_string(I::Json {
                type_code: 0x01,
                value: &[]
            })
            .unwrap(),
            ""
        );
        assert_eq!(
            native_sql_string(I::Json {
                type_code: 0x03,
                value: &[]
            })
            .unwrap(),
            ""
        );
        let nonfinite = f64::INFINITY.to_le_bytes();
        let panic = std::panic::catch_unwind(|| {
            native_sql_bytes(I::Json {
                type_code: 0x0b,
                value: &nonfinite,
            })
        })
        .unwrap_err();
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap();
        assert!(message.contains("a Display implementation returned an error unexpectedly"));
    }
    #[test]
    fn fixed_and_scientific_float_primitives_keep_width_specials_and_exponents() {
        for (value, text) in [
            (f64::NAN, "NaN"),
            (f64::INFINITY, "+Inf"),
            (f64::NEG_INFINITY, "-Inf"),
        ] {
            assert_eq!(native_sql_string(I::Real(value)).unwrap(), text);
            assert_eq!(native_sql_string(I::Float32(value)).unwrap(), text);
            assert_eq!(native_sql_float64_scientific(value), text);
            assert_eq!(native_sql_float32_scientific(value), text);
        }
        assert_eq!(native_sql_string(I::Real(-0.0)).unwrap(), "-0");
        assert_eq!(native_sql_string(I::Float32(-0.0)).unwrap(), "-0");
        assert_eq!(native_sql_float64_scientific(-0.0), "-0e+00");
        assert_eq!(native_sql_float32_scientific(-0.0), "-0e+00");
        assert_eq!(
            native_sql_string(I::Real(1e20)).unwrap(),
            "100000000000000000000"
        );
        assert_eq!(native_sql_float64_scientific(1e20), "1e+20");
        assert_eq!(native_sql_float64_scientific(1e-9), "1e-09");
        assert_eq!(native_sql_float64_scientific(1e300), "1e+300");
        assert_eq!(
            native_sql_string(I::Real(-3.1111111)).unwrap(),
            "-3.1111111"
        );
        assert_eq!(
            native_sql_string(I::Float32(-3.1111111)).unwrap(),
            "-3.1111112"
        );
        assert_eq!(native_sql_float32_scientific(-3.1111111), "-3.1111112e+00");
        assert_eq!(native_sql_string(I::Float32(f64::MAX)).unwrap(), "+Inf");
        assert_eq!(native_sql_float32_scientific(f64::MAX), "+Inf");
    }
}
