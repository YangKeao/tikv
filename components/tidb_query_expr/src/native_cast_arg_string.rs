// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! String argument wrapping and its source-backed return metadata. Identity is
//! an explicit action: it retains native storage/collation without hidden tags.
use tidb_query_datatype::codec::{
    native_eval_type::NativeEvalType, native_sql_string::NativeSqlStringInput,
    native_type_name::NativeTypeNameCode,
};

use crate::native_coerce_string::native_coerce_bytes;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeArgStringResult {
    Original,
    Binary(Vec<u8>),
    String(Vec<u8>),
    Null,
}

/// Preserve native string kinds and string-typed hybrids as actual original
/// datums. BIT alone becomes binary storage; other scalars use byte coercion.
pub fn native_cast_arg_as_string(
    input: NativeSqlStringInput<'_>,
) -> Result<NativeArgStringResult, &'static str> {
    use NativeSqlStringInput as I;
    Ok(match input {
        I::Null | I::String(_) | I::Bytes(_) | I::Enum(_) | I::Set(_) | I::BinaryLiteral(_) => {
            NativeArgStringResult::Original
        }
        I::Bit(bytes) => NativeArgStringResult::Binary(bytes.to_vec()),
        _ => native_coerce_bytes(input)?
            .map_or(NativeArgStringResult::Null, NativeArgStringResult::String),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeArgStringSource<'a> {
    pub eval_type: NativeEvalType,
    pub code: NativeTypeNameCode,
    pub flen: i64,
    pub decimal: i64,
    pub charset: &'a str,
    pub collation: &'a str,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeArgStringType<'a> {
    Original,
    VarString {
        flen: i64,
        charset: &'a str,
        collation: &'a str,
    },
}

/// Source WrapWithCastAsString width, also used by CONCAT metadata. Do not
/// replace the unspecified sentinel or unchecked arithmetic with clamping.
pub fn native_string_cast_flen(
    eval_type: NativeEvalType,
    code: NativeTypeNameCode,
    flen: i64,
    decimal: i64,
) -> i64 {
    use NativeEvalType as E;
    use NativeTypeNameCode::Known;
    const UNSPECIFIED: i64 = -1;
    const MAX_LONG_BLOB_WIDTH: i64 = 4_294_967_295;
    let requested = match eval_type {
        E::String => return flen,
        E::Int if code == Known(16) => (flen + 7) / 8,
        E::Int => 20,
        E::Decimal if flen != UNSPECIFIED => flen + 3,
        E::Real => UNSPECIFIED,
        _ => flen,
    };
    if requested != UNSPECIFIED {
        return requested;
    }
    let with_fraction = |base: i64| {
        if decimal > 0 {
            base + 1 + decimal
        } else {
            base
        }
    };
    match eval_type {
        E::Real if code == Known(4) => 87,
        E::Real => 370,
        E::Datetime | E::Timestamp if code == Known(10) => 10,
        E::Datetime | E::Timestamp => with_fraction(19),
        E::Duration => with_fraction(10),
        E::Json => MAX_LONG_BLOB_WIDTH,
        _ => UNSPECIFIED,
    }
}

/// Metadata identity precedes collation policy. Otherwise explicit collation
/// wins even for BIT, followed by BIT binary metadata and connection metadata.
pub fn native_cast_arg_as_string_type<'a>(
    source: NativeArgStringSource<'a>,
    explicit_collation: bool,
    connection: (&'a str, &'a str),
) -> NativeArgStringType<'a> {
    if source.eval_type == NativeEvalType::String {
        return NativeArgStringType::Original;
    }
    let (charset, collation) = if explicit_collation {
        (source.charset, source.collation)
    } else if source.code == NativeTypeNameCode::Known(16) {
        ("binary", "binary")
    } else {
        connection
    };
    NativeArgStringType::VarString {
        flen: native_string_cast_flen(source.eval_type, source.code, source.flen, source.decimal),
        charset,
        collation,
    }
}

#[cfg(test)]
mod tests {
    use NativeSqlStringInput as I;

    use super::*;
    #[test]
    fn byte_coercion_and_string_argument_keep_raw_storage_rust_floats_and_identity() {
        let bytes = &[0xff, 0, 0x80];
        for input in [
            I::String(bytes),
            I::Bytes(bytes),
            I::BinaryLiteral(bytes),
            I::Bit(bytes),
            I::Enum(bytes),
            I::Set(bytes),
            I::Raw(bytes),
        ] {
            assert_eq!(native_coerce_bytes(input), Ok(Some(bytes.to_vec())));
        }
        for input in [
            I::Null,
            I::String(bytes),
            I::Bytes(bytes),
            I::Enum(bytes),
            I::Set(bytes),
            I::BinaryLiteral(bytes),
        ] {
            assert_eq!(
                native_cast_arg_as_string(input),
                Ok(NativeArgStringResult::Original)
            );
        }
        assert_eq!(
            native_cast_arg_as_string(I::Bit(bytes)),
            Ok(NativeArgStringResult::Binary(bytes.to_vec()))
        );
        assert_eq!(
            native_cast_arg_as_string(I::Raw(bytes)),
            Ok(NativeArgStringResult::String(bytes.to_vec()))
        );
        for (input, expected) in [
            (I::Real(f64::INFINITY), "inf"),
            (I::Real(f64::NEG_INFINITY), "-inf"),
            (I::Real(-0.0), "-0"),
            (I::Real(1e20), "100000000000000000000"),
            (I::Float32(16_777_217.0), "16777216"),
            (I::Float32(f64::MAX), "inf"),
            (I::Int(-1), "-1"),
            (I::UInt(u64::MAX), "18446744073709551615"),
        ] {
            assert_eq!(
                native_coerce_bytes(input),
                Ok(Some(expected.as_bytes().to_vec()))
            );
            assert_eq!(
                native_cast_arg_as_string(input),
                Ok(NativeArgStringResult::String(expected.as_bytes().to_vec()))
            );
        }
        assert_eq!(native_coerce_bytes(I::Null), Ok(None));
        for input in [I::MinNotNull, I::MaxValue] {
            assert_eq!(
                native_coerce_bytes(input),
                Err("range sentinel byte coercion")
            );
            assert_eq!(
                native_cast_arg_as_string(input),
                Err("range sentinel byte coercion")
            );
        }
        assert_eq!(
            native_coerce_bytes(I::Json {
                type_code: 11,
                value: &[0, 0, 0, 0, 0, 0, 240, 63]
            }),
            Ok(Some(b"1.0".to_vec()))
        );
        assert!(
            std::panic::catch_unwind(|| native_cast_arg_as_string(I::Json {
                type_code: 11,
                value: &[0, 0, 0, 0, 0, 0, 240, 127]
            }))
            .is_err()
        );
    }
    #[test]
    fn string_argument_metadata_keeps_collation_precedence_and_source_width_rules() {
        use NativeEvalType as E;
        use NativeTypeNameCode::{Known, Unknown};
        for (eval_type, code, flen, decimal, expected) in [
            (E::String, Known(253), -1, 0, -1),
            (E::String, Known(253), 16, 0, 16),
            (E::Int, Known(1), 4, 0, 20),
            (E::Int, Known(16), 8, 0, 1),
            (E::Int, Known(16), 9, 0, 2),
            (E::Int, Known(16), -1, 0, 0),
            (E::Int, Unknown(16), 8, 0, 20),
            (E::Decimal, Known(246), 10, 2, 13),
            (E::Decimal, Known(246), -1, 2, -1),
            (E::Real, Known(4), 12, 0, 87),
            (E::Real, Known(5), 22, 0, 370),
            (E::Real, Unknown(4), -1, 0, 370),
            (E::Datetime, Known(10), -1, 6, 10),
            (E::Datetime, Known(12), -1, 0, 19),
            (E::Datetime, Known(12), -1, 3, 23),
            (E::Timestamp, Known(7), -1, 6, 26),
            (E::Datetime, Known(12), 30, 6, 30),
            (E::Duration, Known(11), -1, 2, 13),
            (E::Duration, Known(11), -1, -1, 10),
            (E::Json, Known(245), -1, 0, 4_294_967_295),
            (E::Json, Known(245), 123, 0, 123),
            (E::VectorFloat32, Known(225), -1, 0, -1),
        ] {
            assert_eq!(
                native_string_cast_flen(eval_type, code, flen, decimal),
                expected
            );
        }
        let source = NativeArgStringSource {
            eval_type: E::Int,
            code: Known(16),
            flen: 8,
            decimal: 0,
            charset: "latin1",
            collation: "latin1_bin",
        };
        let connection = ("utf8mb4", "utf8mb4_bin");
        assert_eq!(
            native_cast_arg_as_string_type(source, true, connection),
            NativeArgStringType::VarString {
                flen: 1,
                charset: "latin1",
                collation: "latin1_bin"
            }
        );
        assert_eq!(
            native_cast_arg_as_string_type(source, false, connection),
            NativeArgStringType::VarString {
                flen: 1,
                charset: "binary",
                collation: "binary"
            }
        );
        assert_eq!(
            native_cast_arg_as_string_type(
                NativeArgStringSource {
                    code: Known(8),
                    ..source
                },
                false,
                connection
            ),
            NativeArgStringType::VarString {
                flen: 20,
                charset: "utf8mb4",
                collation: "utf8mb4_bin"
            }
        );
        assert_eq!(
            native_cast_arg_as_string_type(
                NativeArgStringSource {
                    eval_type: E::String,
                    flen: i64::MAX,
                    ..source
                },
                true,
                connection
            ),
            NativeArgStringType::Original
        );
        if cfg!(debug_assertions) {
            assert!(
                std::panic::catch_unwind(|| native_string_cast_flen(
                    E::Int,
                    Known(16),
                    i64::MAX,
                    0
                ))
                .is_err()
            );
            assert!(
                std::panic::catch_unwind(|| native_string_cast_flen(
                    E::Decimal,
                    Known(246),
                    i64::MAX,
                    0
                ))
                .is_err()
            );
        }
    }
}
