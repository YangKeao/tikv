// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Closed native expression JSON coercion policies. Ordinary/typed/value CAST,
//! argument modes, document admission and scalar/text helpers intentionally
//! retain different source rules. Datatype construction, display and parsing
//! use existing SDK cores; there are no host policy callbacks.
use std::borrow::Cow;

use serde_json::{Number, Value as Json};
use tidb_query_datatype::codec::{
    mysql::{
        Time,
        json::{NativeJsonError, parse_native_json_document},
    },
    native_json_construct::native_json_from_opaque,
    native_json_parse::native_json_parse,
    native_mysql_json::{
        NativeDatumJsonSource, native_datum_to_mysql_json, native_datum_to_mysql_json_with_source,
    },
    native_sql_string::{NativeSqlStringInput, native_sql_string},
    native_type_name::NativeTypeNameCode,
};

use crate::native_json_format;

#[derive(Clone, Copy, Debug)]
pub struct NativeJsonCoercionSource<'a> {
    pub datum: NativeDatumJsonSource<'a>,
    pub flags: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeJsonStringArgument {
    Document,
    Value,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeJsonCoercionError {
    Unsupported(&'static str),
    FloatOverflow,
    EmptyText,
    InvalidText,
    InvalidTypeForJson {
        argument: usize,
        function: &'static str,
    },
}
use NativeJsonCoercionError as E;
use NativeSqlStringInput as I;

fn boolean_flagged_int(input: I<'_>, source: Option<NativeJsonCoercionSource<'_>>) -> Option<i64> {
    if !source.is_some_and(|source| source.flags & (1_u64 << 19) != 0) {
        return None;
    }
    match input {
        I::Int(value) => Some(value),
        I::UInt(value) => Some(value as i64),
        _ => None,
    }
}
fn is_binary_datum(input: I<'_>, source: Option<NativeJsonCoercionSource<'_>>) -> bool {
    matches!(input, I::Bytes(_) | I::String(_))
        && source.is_some_and(|source| {
            source
                .datum
                .string_code
                .is_binary_string(source.datum.collation)
        })
}
// The JSON/decimal variants render through the original Display boundary;
// rendered text is already a String. JSON fmt::Error still panics inside that
// original to_string implementation, before the infallible UTF-8 projection.
fn display(input: I<'_>) -> String {
    native_sql_string(input).expect("a Display implementation returned an error unexpectedly")
}
fn binary_opaque_json(input: I<'_>, source: NativeJsonCoercionSource<'_>) -> Result<Json, E> {
    let (type_code, value) = native_datum_to_mysql_json_with_source(input, source.datum)
        .map_err(|_| E::Unsupported("datum JSON conversion"))?;
    native_parse_json_expression(&display(I::Json {
        type_code,
        value: &value,
    }))
}

/// Borrow an SQL string only for the actual String/Bytes kinds, checking UTF-8.
pub fn native_json_sql_string(input: I<'_>) -> Result<Option<&str>, E> {
    let bytes = match input {
        I::String(bytes) | I::Bytes(bytes) => bytes,
        _ => return Ok(None),
    };
    std::str::from_utf8(bytes)
        .map(Some)
        .map_err(|_| E::Unsupported("invalid UTF-8 string datum"))
}
/// JSON cells use their original owned Display; SQL strings remain borrowed.
pub fn native_json_document_string(input: I<'_>) -> Result<Option<Cow<'_, str>>, E> {
    if matches!(input, I::Json { .. }) {
        return Ok(Some(Cow::Owned(display(input))));
    }
    Ok(native_json_sql_string(input)?.map(Cow::Borrowed))
}
/// Ordinary CAST: SQL NULL passes through and typed binary identities bypass
/// the text-domain round trip used for the remaining sources.
pub fn native_cast_as_json(input: I<'_>) -> Result<Option<(u8, Vec<u8>)>, E> {
    if matches!(input, I::Null) {
        return Ok(None);
    }
    if let Some(typed) = typed_cast_json(input) {
        return typed.map(Some);
    }
    let json = match native_json_sql_string(input)? {
        Some(text) => native_parse_json_expression(text)?,
        None => datum_json_scalar(input)?,
    };
    native_binary_json_datum(json).map(Some)
}
fn typed_cast_json(input: I<'_>) -> Option<Result<(u8, Vec<u8>), E>> {
    match input {
        I::Int(_) | I::UInt(_) | I::Real(_) | I::Float32(_) | I::Decimal(_) | I::Json { .. } => {
            Some(
                native_datum_to_mysql_json(input)
                    .map_err(|_| E::Unsupported("datum JSON conversion")),
            )
        }
        I::Time(mut value) => {
            if value.set_fsp(6).is_err() {
                return Some(Err(E::Unsupported("datum JSON conversion")));
            }
            Some(
                native_datum_to_mysql_json(I::Time(value))
                    .map_err(|_| E::Unsupported("datum JSON conversion")),
            )
        }
        I::Duration { nanoseconds, .. } => Some(
            Time::native_normalize_fsp(6)
                .ok_or(E::Unsupported("datum JSON conversion"))
                .and_then(|fsp| {
                    native_datum_to_mysql_json(I::Duration { nanoseconds, fsp })
                        .map_err(|_| E::Unsupported("datum JSON conversion"))
                }),
        ),
        I::BinaryLiteral(bytes) => Some(Ok(native_json_from_opaque(253, bytes))),
        _ => None,
    }
}
/// Retain FORMAT then lenient BinaryJSON.parse, including its error folding.
/// Direct serde-value encoding is not equivalent to this original round trip.
pub fn native_binary_json_datum(json: Json) -> Result<(u8, Vec<u8>), E> {
    native_json_parse(&native_json_format(&json)).map_err(|_| E::InvalidText)
}
/// Typed CAST checks NULL, actual binary-source metadata, then the boolean
/// flag.
pub fn native_cast_as_json_typed(
    input: I<'_>,
    source: Option<NativeJsonCoercionSource<'_>>,
) -> Result<Option<(u8, Vec<u8>)>, E> {
    if matches!(input, I::Null) {
        return Ok(None);
    }
    if let Some(source) = source {
        if is_binary_datum(input, Some(source)) {
            return native_datum_to_mysql_json_with_source(input, source.datum)
                .map(Some)
                .map_err(|_| E::Unsupported("datum JSON conversion"));
        }
    }
    if let Some(value) = boolean_flagged_int(input, source) {
        return native_binary_json_datum(Json::Bool(value != 0)).map(Some);
    }
    native_cast_as_json(input)
}
/// Value CAST disables document parsing for SQL strings except when their
/// actual effective source type is JSON. Typed binary identities remain typed.
pub fn native_cast_as_json_value_typed(
    input: I<'_>,
    source: Option<NativeJsonCoercionSource<'_>>,
) -> Result<Option<(u8, Vec<u8>)>, E> {
    if matches!(input, I::Null) {
        return Ok(None);
    }
    if let Some(source) = source {
        if is_binary_datum(input, Some(source)) {
            return native_datum_to_mysql_json_with_source(input, source.datum)
                .map(Some)
                .map_err(|_| E::Unsupported("datum JSON conversion"));
        }
    }
    if let Some(value) = boolean_flagged_int(input, source) {
        return native_binary_json_datum(Json::Bool(value != 0)).map(Some);
    }
    if let Some(typed) = typed_cast_json(input) {
        return typed.map(Some);
    }
    native_binary_json_datum(native_json_argument(
        input,
        NativeJsonStringArgument::Value,
        source,
    )?)
    .map(Some)
}
/// Mutation/value argument coercion. SQL NULL becomes JSON null here. Real,
/// Float32 and Decimal deliberately retain their different original pathways.
pub fn native_json_argument(
    input: I<'_>,
    string: NativeJsonStringArgument,
    source: Option<NativeJsonCoercionSource<'_>>,
) -> Result<Json, E> {
    if let Some(source) = source {
        if is_binary_datum(input, Some(source)) {
            return binary_opaque_json(input, source);
        }
    }
    if let Some(value) = boolean_flagged_int(input, source) {
        return Ok(Json::Bool(value != 0));
    }
    if let Some(text) = native_json_sql_string(input)? {
        if source.is_some_and(|source| source.datum.code == NativeTypeNameCode::Known(245)) {
            return native_parse_json_expression(text);
        }
        return match string {
            NativeJsonStringArgument::Document => native_parse_json_expression(text),
            NativeJsonStringArgument::Value => Ok(Json::String(text.to_owned())),
        };
    }
    match input {
        I::Null => Ok(Json::Null),
        I::Int(value) => Ok(Json::Number(value.into())),
        I::UInt(value) => Ok(Json::Number(value.into())),
        I::Real(value) => Number::from_f64(value)
            .map(Json::Number)
            .ok_or(E::FloatOverflow),
        I::Decimal(_) => native_parse_json_expression(&display(input)),
        I::MinNotNull | I::MaxValue => Err(E::Unsupported("range sentinel JSON value")),
        other => datum_json_scalar(other),
    }
}
/// Ordinary document admission: only the original four numeric kinds are
/// promoted. Float32, despite its numeric storage, is not one of those kinds.
pub fn native_parse_json_document_argument(input: I<'_>) -> Result<Option<Json>, E> {
    match input {
        I::Null => Ok(None),
        I::String(_) | I::Bytes(_) => {
            native_parse_json_expression(native_json_sql_string(input)?.unwrap_or_default())
                .map(Some)
        }
        I::Int(_) | I::UInt(_) | I::Decimal(_) | I::Real(_) => datum_json_scalar(input).map(Some),
        I::Json { .. } => native_parse_json_expression(&display(input)).map(Some),
        I::MinNotNull | I::MaxValue => Err(E::Unsupported("JSON document requires string")),
        I::Float32(_)
        | I::BinaryLiteral(_)
        | I::Duration { .. }
        | I::Enum(_)
        | I::Bit(_)
        | I::Set(_)
        | I::Time(_)
        | I::Raw(_)
        | I::VectorFloat32(_) => Err(E::Unsupported("JSON document requires JSON or string")),
    }
}
/// Prepare the original document text without parsing it, notably for DEPTH.
pub fn native_json_document_text_argument(input: I<'_>) -> Result<Option<String>, E> {
    match input {
        I::Null => Ok(None),
        I::String(_) | I::Bytes(_) => Ok(native_json_sql_string(input)?.map(str::to_owned)),
        I::Int(_) | I::UInt(_) | I::Decimal(_) | I::Real(_) => {
            let (type_code, value) = native_datum_to_mysql_json(input)
                .map_err(|_| E::Unsupported("datum JSON conversion"))?;
            Ok(Some(display(I::Json {
                type_code,
                value: &value,
            })))
        }
        I::Json { .. } => Ok(Some(display(input))),
        I::MinNotNull | I::MaxValue => Err(E::Unsupported("JSON document requires string")),
        I::Float32(_)
        | I::BinaryLiteral(_)
        | I::Duration { .. }
        | I::Enum(_)
        | I::Bit(_)
        | I::Set(_)
        | I::Time(_)
        | I::Raw(_)
        | I::VectorFloat32(_) => Err(E::Unsupported("JSON document requires JSON or string")),
    }
}
/// Strict document signatures reject only their original four numeric kinds,
/// retaining the caller's argument number and exact static function spelling.
pub fn native_parse_json_document_argument_strict(
    input: I<'_>,
    argument: usize,
    function: &'static str,
) -> Result<Option<Json>, E> {
    if matches!(input, I::Int(_) | I::UInt(_) | I::Decimal(_) | I::Real(_)) {
        return Err(E::InvalidTypeForJson { argument, function });
    }
    native_parse_json_document_argument(input)
}
fn datum_json_scalar(input: I<'_>) -> Result<Json, E> {
    let (type_code, value) =
        native_datum_to_mysql_json(input).map_err(|_| E::Unsupported("datum JSON conversion"))?;
    native_parse_json_expression(&display(I::Json {
        type_code,
        value: &value,
    }))
}
/// Strict expression parser/error mapping, distinct from the lenient datatype
/// parser used only after formatting an already-formed JSON value.
pub fn native_parse_json_expression(text: &str) -> Result<Json, E> {
    parse_native_json_document(text).map_err(|error| match error {
        NativeJsonError::EmptyText => E::EmptyText,
        NativeJsonError::InvalidText | NativeJsonError::InvalidBinary => E::InvalidText,
    })
}

#[cfg(test)]
mod tests {
    use NativeJsonStringArgument as Mode;
    use tidb_query_datatype::codec::{
        mysql::{
            NativeDecimalParseRef,
            time::{NativeTemporalValue, TimeType},
        },
        native_string_type::NativeStringTypeCode as S,
    };

    use super::*;
    fn source(
        code: NativeTypeNameCode,
        string_code: S,
        collation: &str,
        flen: i64,
        flags: u64,
    ) -> NativeJsonCoercionSource<'_> {
        NativeJsonCoercionSource {
            datum: NativeDatumJsonSource {
                code,
                string_code,
                collation,
                flen,
            },
            flags,
        }
    }
    #[test]
    fn json_coercion_preserves_modes_null_boolean_and_actual_binary_metadata() {
        assert_eq!(native_cast_as_json(I::Null), Ok(None));
        assert_eq!(
            native_json_argument(I::Null, Mode::Value, None),
            Ok(Json::Null)
        );
        assert_eq!(
            native_cast_as_json(I::String(b"1")),
            Ok(Some((9, vec![1, 0, 0, 0, 0, 0, 0, 0])))
        );
        assert_eq!(
            native_cast_as_json_value_typed(I::String(b"1"), None),
            Ok(Some((12, vec![1, 49])))
        );
        assert_eq!(
            native_json_argument(I::Bytes(b"1"), Mode::Document, None),
            Ok(Json::from(1))
        );
        assert_eq!(
            native_json_argument(I::Bytes(b"1"), Mode::Value, None),
            Ok(Json::String("1".into()))
        );
        let json = source(
            NativeTypeNameCode::Known(245),
            S::Other(245),
            "binary",
            -1,
            0,
        );
        let unknown_json = source(
            NativeTypeNameCode::Unknown(245),
            S::Other(245),
            "binary",
            -1,
            0,
        );
        assert_eq!(
            native_json_argument(I::String(b"1"), Mode::Value, Some(json)),
            Ok(Json::from(1))
        );
        assert_eq!(
            native_json_argument(I::String(b"1"), Mode::Value, Some(unknown_json)),
            Ok(Json::String("1".into()))
        );
        assert_eq!(
            native_cast_as_json_value_typed(I::String(b"1"), Some(json)),
            Ok(Some((9, vec![1, 0, 0, 0, 0, 0, 0, 0])))
        );
        let binary = source(NativeTypeNameCode::Known(254), S::String, "binary", 3, 0);
        assert_eq!(
            native_cast_as_json_typed(I::Bytes(b"ab"), Some(binary)),
            Ok(Some((13, vec![254, 3, 97, 98, 0])))
        );
        assert_eq!(
            native_json_argument(I::Bytes(b"ab"), Mode::Document, Some(binary)),
            Ok(Json::String("base64:type254:YWIA".into()))
        );
        let nonbinary = source(
            NativeTypeNameCode::Known(253),
            S::VarString,
            "utf8mb4_bin",
            -1,
            0,
        );
        assert_eq!(
            native_json_argument(I::Bytes(b"ab"), Mode::Value, Some(nonbinary)),
            Ok(Json::String("ab".into()))
        ); // unlike datatype with_source, Bytes is not unconditional opaque here
        let boolean = source(
            NativeTypeNameCode::Known(3),
            S::Other(3),
            "binary",
            -1,
            1_u64 << 19,
        );
        assert_eq!(
            native_cast_as_json_typed(I::Int(0), Some(boolean)),
            Ok(Some((4, vec![2])))
        );
        assert_eq!(
            native_cast_as_json_value_typed(I::UInt(u64::MAX), Some(boolean)),
            Ok(Some((4, vec![1])))
        );
        assert_eq!(
            native_json_argument(I::UInt(u64::MAX), Mode::Value, Some(boolean)),
            Ok(Json::Bool(true))
        );
        assert_eq!(native_cast_as_json_typed(I::Null, Some(boolean)), Ok(None));
        assert_eq!(
            native_cast_as_json(I::BinaryLiteral(b"A")),
            Ok(Some((13, vec![253, 1, 65])))
        );
        assert_eq!(
            native_cast_as_json(I::Bit(b"A")),
            Ok(Some((12, vec![1, 65])))
        );
        assert_eq!(
            native_json_argument(I::BinaryLiteral(b"A"), Mode::Value, None),
            Ok(Json::String("A".into()))
        );
        assert_eq!(native_json_sql_string(I::BinaryLiteral(b"A")), Ok(None));
        assert!(matches!(
            native_json_document_string(I::String(b"{}")),
            Ok(Some(Cow::Borrowed("{}")))
        ));
    }
    #[test]
    fn json_coercion_preserves_numeric_precision_raw_float_width_and_temporal_restamps() {
        assert_eq!(
            native_json_argument(I::Real(f64::NAN), Mode::Value, None),
            Err(E::FloatOverflow)
        );
        assert_eq!(
            native_json_argument(I::Float32(f64::NAN), Mode::Value, None),
            Err(E::Unsupported("datum JSON conversion"))
        );
        assert_eq!(
            native_cast_as_json(I::Real(f64::INFINITY)),
            Err(E::Unsupported("datum JSON conversion"))
        );
        assert_eq!(
            native_cast_as_json(I::Float32(16_777_217.0)),
            Ok(Some((11, vec![0, 0, 0, 16, 0, 0, 112, 65])))
        );
        assert_eq!(
            native_cast_as_json(I::UInt(1)),
            Ok(Some((10, vec![1, 0, 0, 0, 0, 0, 0, 0])))
        );
        let decimal = NativeDecimalParseRef {
            negative: false,
            digits: b"9007199254740993",
            scale: 0,
            storage_scale: 0,
            declared_shape: None,
        };
        assert_eq!(
            native_json_argument(I::Decimal(decimal), Mode::Value, None),
            Ok(Json::from(9_007_199_254_740_993_i64))
        );
        assert_eq!(
            native_cast_as_json(I::Decimal(decimal)),
            Ok(Some((11, vec![0, 0, 0, 0, 0, 0, 64, 67])))
        );
        assert_eq!(
            native_parse_json_document_argument(I::Decimal(decimal)),
            Ok(Some(Json::from(9_007_199_254_740_992.0_f64)))
        );
        assert_eq!(
            native_json_document_text_argument(I::Int(7)),
            Ok(Some("7".into()))
        );
        let date = NativeTemporalValue {
            raw: 0,
            kind: TimeType::Date,
            fsp: u8::MAX,
        };
        assert_eq!(
            native_cast_as_json(I::Time(date)),
            Ok(Some((14, vec![0; 8])))
        );
        let timestamp = NativeTemporalValue {
            raw: u64::MAX,
            kind: TimeType::Timestamp,
            fsp: u8::MAX,
        };
        assert_eq!(
            native_cast_as_json(I::Time(timestamp)),
            Ok(Some((16, vec![255; 8])))
        );
        assert_eq!(
            native_cast_as_json(I::Duration {
                nanoseconds: i64::MAX,
                fsp: -123
            }),
            Ok(Some((
                17,
                vec![255, 255, 255, 255, 255, 255, 255, 127, 6, 0, 0, 0]
            )))
        ); // no wire-duration range validation
        let raw = I::Json {
            type_code: 3,
            value: &[0xff],
        };
        assert_eq!(native_cast_as_json(raw), Ok(Some((3, vec![255]))));
        assert_eq!(
            native_json_document_text_argument(raw),
            Ok(Some(String::new()))
        );
        assert_eq!(
            native_json_argument(raw, Mode::Value, None),
            Err(E::EmptyText)
        );
        assert!(
            matches!(native_json_document_string(raw),Ok(Some(Cow::Owned(text))) if text.is_empty())
        );
        let inf = f64::INFINITY.to_le_bytes();
        assert!(
            std::panic::catch_unwind(|| native_parse_json_document_argument(I::Json {
                type_code: 11,
                value: &inf
            }))
            .is_err()
        );
    }
    #[test]
    fn json_coercion_preserves_admission_error_priority_and_format_parse_roundtrip() {
        assert_eq!(
            native_json_sql_string(I::Bytes(&[0xff])),
            Err(E::Unsupported("invalid UTF-8 string datum"))
        );
        assert_eq!(
            native_cast_as_json(I::Bytes(&[0xff])),
            Err(E::Unsupported("invalid UTF-8 string datum"))
        );
        assert_eq!(
            native_json_argument(I::BinaryLiteral(&[0xff]), Mode::Value, None),
            Err(E::Unsupported("datum JSON conversion"))
        );
        let binary = source(
            NativeTypeNameCode::Known(253),
            S::VarString,
            "binary",
            -1,
            0,
        );
        assert_eq!(
            native_cast_as_json_typed(I::Bytes(&[0xff]), Some(binary)),
            Ok(Some((13, vec![253, 1, 255])))
        );
        assert_eq!(
            native_parse_json_document_argument_strict(I::Int(1), 2, "member of"),
            Err(E::InvalidTypeForJson {
                argument: 2,
                function: "member of"
            })
        );
        assert_eq!(
            native_parse_json_document_argument_strict(I::Float32(1.0), 2, "member of"),
            Err(E::Unsupported("JSON document requires JSON or string"))
        );
        assert_eq!(
            native_parse_json_document_argument(I::MinNotNull),
            Err(E::Unsupported("JSON document requires string"))
        );
        assert_eq!(
            native_json_document_text_argument(I::MaxValue),
            Err(E::Unsupported("JSON document requires string"))
        );
        assert_eq!(
            native_cast_as_json(I::MinNotNull),
            Err(E::Unsupported("datum JSON conversion"))
        );
        assert_eq!(
            native_cast_as_json_value_typed(I::MinNotNull, None),
            Err(E::Unsupported("range sentinel JSON value"))
        );
        assert_eq!(native_parse_json_document_argument(I::Null), Ok(None));
        assert_eq!(native_parse_json_expression(" \t"), Err(E::EmptyText));
        assert_eq!(
            native_parse_json_expression(r#""\uD800""#),
            Err(E::InvalidText)
        ); // no datatype surrogate repair on expression input
        assert_eq!(
            native_cast_as_json(I::String(br#""\uD800""#)),
            Err(E::InvalidText)
        );
        assert_eq!(
            native_binary_json_datum(Json::from(1.0)),
            Ok((11, vec![0, 0, 0, 0, 0, 0, 240, 63]))
        ); // FORMAT retains the .0 spelling before lenient reparsing
        let mut object = serde_json::Map::new();
        object.insert("x".repeat(65_536), Json::Null);
        assert_eq!(
            native_binary_json_datum(Json::Object(object)),
            Err(E::InvalidText)
        ); // folds datatype KeyTooLong instead of leaking it
    }
}
