// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Ordinary CHAR/BINARY cast control over the existing encoding core and the
//! original datatype SQL stringification actuator. This does not implement a
//! second general datum stringifier or change the wire charset policy.
use tidb_query_datatype::codec::collation::native_encoding::{TransformOp, find_encoding};
pub use tidb_query_datatype::codec::native_string_type::NativeStringTypeCode as NativeCastStringTypeCode;

#[derive(Clone, Copy, Debug)]
pub enum NativeCastStringInput<'a> {
    Int(i64),
    UInt(u64),
    String(&'a [u8]),
    Bytes(&'a [u8]),
    BinaryLiteral(&'a [u8]),
    Bit(&'a [u8]),
    Other,
}
#[derive(Clone, Copy, Debug)]
pub struct NativeCastStringSource<'a> {
    pub code: NativeCastStringTypeCode,
    pub collation: &'a str,
}
#[derive(Clone, Copy, Debug)]
pub enum NativeCastStringTarget<'a> {
    Char {
        len: Option<u32>,
        charset: Option<&'a str>,
    },
    Binary {
        len: Option<u32>,
    },
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeCastStringResult {
    Null,
    Text(String),
    Bytes(Vec<u8>),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeCastStringError<E> {
    Child(E),
    InvalidUtf8StringCoercion,
}

fn report_data_too_long(
    data_len: usize,
    field_len: usize,
    mut append_warning: impl FnMut(u16, &str),
) {
    if data_len > field_len {
        append_warning(
            1406,
            &format!("Data Too Long, field len {field_len}, data len {data_len}"),
        );
    }
}
fn datum_sql_string<E, SE>(
    sql_string: impl FnOnce() -> Result<String, SE>,
) -> Result<String, NativeCastStringError<E>> {
    sql_string().map_err(|_| NativeCastStringError::InvalidUtf8StringCoercion)
}
fn year_zero_string(
    input: NativeCastStringInput<'_>,
    source: Option<NativeCastStringSource<'_>>,
) -> Option<String> {
    if source.map(|source| source.code) != Some(NativeCastStringTypeCode::Year) {
        return None;
    }
    matches!(
        input,
        NativeCastStringInput::Int(0) | NativeCastStringInput::UInt(0)
    )
    .then(|| "0000".to_owned())
}
fn string_source_text<E, SE>(
    input: NativeCastStringInput<'_>,
    source: Option<NativeCastStringSource<'_>>,
    sql_string: impl FnOnce() -> Result<String, SE>,
) -> Result<String, NativeCastStringError<E>> {
    match year_zero_string(input, source) {
        Some(text) => Ok(text),
        None => datum_sql_string(sql_string),
    }
}
fn datum_binary_bytes<E, SE>(
    input: NativeCastStringInput<'_>,
    sql_string: impl FnOnce() -> Result<String, SE>,
) -> Result<Vec<u8>, NativeCastStringError<E>> {
    use NativeCastStringInput as I;
    match input {
        I::String(bytes) | I::Bytes(bytes) | I::BinaryLiteral(bytes) | I::Bit(bytes) => {
            Ok(bytes.to_vec())
        }
        _ => Ok(datum_sql_string(sql_string)?.into_bytes()),
    }
}
fn binary_pad_truncate(bytes: &[u8], len: usize) -> Vec<u8> {
    let mut output: Vec<u8> = bytes.iter().copied().take(len).collect();
    output.resize(len, 0);
    output
}

/// Native callbacks supply original effects, not a selected prefix, warning
/// decision or precomputed string. The exact uppercase BINARY target branch
/// precedes both YEAR rendering and the lazy connection charset read.
#[allow(clippy::too_many_arguments)]
pub fn native_cast_string<'a, E, SE>(
    input: NativeCastStringInput<'_>,
    target: NativeCastStringTarget<'a>,
    source: Option<NativeCastStringSource<'_>>,
    connection_charset: impl FnOnce() -> &'a str,
    sql_string: impl FnOnce() -> Result<String, SE>,
    max_allowed_packet: impl FnOnce() -> u64,
    allowed_packet_overflow: impl FnOnce(&str) -> Result<(), E>,
    mut append_warning: impl FnMut(u16, &str),
) -> Result<NativeCastStringResult, NativeCastStringError<E>> {
    match target {
        NativeCastStringTarget::Char { len, charset } => {
            if charset == Some("BINARY") {
                let mut bytes = datum_binary_bytes(input, sql_string)?;
                if let Some(len) = len {
                    report_data_too_long(bytes.len(), len as usize, &mut append_warning);
                    bytes.truncate(len as usize);
                }
                return Ok(NativeCastStringResult::Bytes(bytes));
            }
            let target_charset = charset.unwrap_or_else(connection_charset);
            let text = if source
                .is_some_and(|source| source.code.is_binary_string(source.collation))
                && !target_charset.eq_ignore_ascii_case("binary")
            {
                let bytes = datum_binary_bytes(input, sql_string)?;
                let encoding = find_encoding(target_charset);
                // Preserve the actual encoding error's name and invalid bytes;
                // the cast's warning deliberately quotes the WHOLE source.
                let (decoded, error) = encoding.transform(&bytes, TransformOp::DECODE, |invalid| {
                    (encoding.name(), invalid.to_vec())
                });
                if error.is_some() {
                    let hex = bytes
                        .iter()
                        .map(|byte| format!("{byte:02X}"))
                        .collect::<String>();
                    append_warning(
                        3854,
                        &format!("Cannot convert string '{hex}' from binary to {target_charset}"),
                    );
                }
                String::from_utf8_lossy(&decoded).into_owned()
            } else {
                string_source_text(input, source, sql_string)?
            };
            Ok(NativeCastStringResult::Text(match len {
                Some(len) => {
                    report_data_too_long(text.chars().count(), len as usize, &mut append_warning);
                    text.chars().take(len as usize).collect()
                }
                None => text,
            }))
        }
        NativeCastStringTarget::Binary { len } => {
            let bytes = match year_zero_string(input, source) {
                Some(text) => text.into_bytes(),
                None => datum_binary_bytes(input, sql_string)?,
            };
            Ok(NativeCastStringResult::Bytes(match len {
                Some(len) => {
                    report_data_too_long(bytes.len(), len as usize, &mut append_warning);
                    if bytes.len() < (len as usize) && u64::from(len) > max_allowed_packet() {
                        // Invoke the original handler: it may re-read the packet
                        // limit and statement policy. Never reuse our first read
                        // to synthesize that handler's warning or error.
                        allowed_packet_overflow("cast_as_binary")
                            .map_err(NativeCastStringError::Child)?;
                        return Ok(NativeCastStringResult::Null);
                    }
                    binary_pad_truncate(&bytes, len as usize)
                }
                None => bytes,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use NativeCastStringError as E;
    use NativeCastStringInput as I;
    use NativeCastStringResult as R;
    use NativeCastStringTarget as T;
    use NativeCastStringTypeCode as C;

    use super::*;
    fn source(code: C, collation: &str) -> NativeCastStringSource<'_> {
        NativeCastStringSource { code, collation }
    }
    #[test]
    fn char_cast_keeps_exact_binary_branch_year_identity_and_encoding_warning_order() {
        let calls = RefCell::new(Vec::new());
        let value = native_cast_string::<(), ()>(
            I::Int(0),
            T::Char {
                len: None,
                charset: Some("BINARY"),
            },
            Some(source(C::Year, "binary")),
            || panic!("charset"),
            || {
                calls.borrow_mut().push("sql");
                Ok("0".into())
            },
            || panic!("packet"),
            |_| panic!("handler"),
            |_, _| panic!("warning"),
        );
        assert_eq!(value, Ok(R::Bytes(b"0".to_vec())));
        assert_eq!(*calls.borrow(), ["sql"]);
        let value = native_cast_string::<(), ()>(
            I::UInt(0),
            T::Char {
                len: None,
                charset: Some("binary"),
            },
            Some(source(C::Year, "binary")),
            || panic!("charset"),
            || panic!("year SQL string"),
            || panic!("packet"),
            |_| panic!("handler"),
            |_, _| panic!("warning"),
        );
        assert_eq!(value, Ok(R::Text("0000".into())));
        let value = native_cast_string::<(), ()>(
            I::Int(0),
            T::Binary { len: None },
            Some(source(C::Other(13), "binary")),
            || panic!("charset"),
            || Ok("0".into()),
            || panic!("packet"),
            |_| panic!("handler"),
            |_, _| panic!("warning"),
        );
        assert_eq!(value, Ok(R::Bytes(b"0".to_vec()))); // Other(13) is not YEAR
        let warnings = RefCell::new(Vec::new());
        let value = native_cast_string::<(), ()>(
            I::Bytes(&[b'a', b'b', 0xff]),
            T::Char {
                len: Some(1),
                charset: Some("utf8"),
            },
            Some(source(C::VarString, "binary")),
            || panic!("charset"),
            || panic!("SQL string"),
            || panic!("packet"),
            |_| panic!("handler"),
            |code, message| warnings.borrow_mut().push((code, message.to_owned())),
        );
        assert_eq!(value, Ok(R::Text("a".into())));
        assert_eq!(
            *warnings.borrow(),
            [
                (
                    3854,
                    "Cannot convert string '6162FF' from binary to utf8".to_owned()
                ),
                (1406, "Data Too Long, field len 1, data len 2".to_owned())
            ]
        );
        let value = native_cast_string::<(), ()>(
            I::Bytes(&[0xff]),
            T::Char {
                len: None,
                charset: Some("BiNaRy"),
            },
            Some(source(C::VarString, "binary")),
            || panic!("charset"),
            || Err(()),
            || panic!("packet"),
            |_| panic!("handler"),
            |_, _| panic!("no decode"),
        );
        assert_eq!(value, Err(E::InvalidUtf8StringCoercion));
        let value = native_cast_string::<(), ()>(
            I::Bytes(&[0xff]),
            T::Char {
                len: None,
                charset: Some("utf8"),
            },
            Some(source(C::Other(0), "binary")),
            || panic!("charset"),
            || Err(()),
            || panic!("packet"),
            |_| panic!("handler"),
            |_, _| panic!("no decode"),
        );
        assert_eq!(value, Err(E::InvalidUtf8StringCoercion)); // unknown zero is not Unspecified
        let value = native_cast_string::<(), ()>(
            I::Bytes(&[0xff]),
            T::Char {
                len: None,
                charset: Some("unknown"),
            },
            Some(source(C::Unspecified, "binary")),
            || panic!("charset"),
            || panic!("SQL string"),
            || panic!("packet"),
            |_| panic!("handler"),
            |_, _| panic!("binary encoding fallback is not an error"),
        );
        assert_eq!(value, Ok(R::Text("�".into())));
        let order = RefCell::new(Vec::new());
        let value = native_cast_string::<(), ()>(
            I::Other,
            T::Char {
                len: Some(2),
                charset: None,
            },
            None,
            || {
                order.borrow_mut().push("charset");
                "utf8mb4"
            },
            || {
                order.borrow_mut().push("sql");
                Ok("中文a".into())
            },
            || panic!("packet"),
            |_| panic!("handler"),
            |code, message| {
                assert_eq!(
                    (code, message),
                    (1406, "Data Too Long, field len 2, data len 3")
                );
                order.borrow_mut().push("warning");
            },
        );
        assert_eq!(value, Ok(R::Text("中文".into())));
        assert_eq!(*order.borrow(), ["charset", "sql", "warning"]);
        let value = native_cast_string::<(), ()>(
            I::String(&[0xff]),
            T::Char {
                len: None,
                charset: Some("BINARY"),
            },
            None,
            || panic!("charset"),
            || panic!("SQL string"),
            || panic!("packet"),
            |_| panic!("handler"),
            |_, _| panic!("warning"),
        );
        assert_eq!(value, Ok(R::Bytes(vec![0xff])));
        let value = native_cast_string::<(), ()>(
            I::Bytes(b"ok"),
            T::Char {
                len: None,
                charset: None,
            },
            Some(source(C::VarString, "binary")),
            || "BINARY",
            || Ok("ok".into()),
            || panic!("packet"),
            |_| panic!("handler"),
            |_, _| panic!("warning"),
        );
        assert_eq!(value, Ok(R::Text("ok".into()))); // uppercase from the connection is not the explicit-target byte shortcut
    }
    #[test]
    fn binary_cast_keeps_raw_bytes_year_padding_and_lazy_packet_handler() {
        let value = native_cast_string::<(), ()>(
            I::Int(0),
            T::Binary { len: Some(5) },
            Some(source(C::Year, "binary")),
            || panic!("charset"),
            || panic!("year SQL string"),
            || 5,
            |_| panic!("handler"),
            |_, _| panic!("warning"),
        );
        assert_eq!(value, Ok(R::Bytes(b"0000\0".to_vec())));
        let warnings = RefCell::new(Vec::new());
        let value = native_cast_string::<(), ()>(
            I::String("你好world".as_bytes()),
            T::Binary { len: Some(5) },
            None,
            || panic!("charset"),
            || panic!("SQL string"),
            || panic!("no packet read while truncating"),
            |_| panic!("handler"),
            |code, message| warnings.borrow_mut().push((code, message.to_owned())),
        );
        assert_eq!(value, Ok(R::Bytes("你好world".as_bytes()[..5].to_vec())));
        assert_eq!(
            *warnings.borrow(),
            [(1406, "Data Too Long, field len 5, data len 11".to_owned())]
        );
        for input in [
            I::BinaryLiteral(&[0xff, 0]),
            I::Bit(&[0xff, 0]),
            I::Bytes(&[0xff, 0]),
        ] {
            let value = native_cast_string::<(), ()>(
                input,
                T::Binary { len: Some(2) },
                None,
                || panic!("charset"),
                || panic!("SQL string"),
                || panic!("equal width"),
                |_| panic!("handler"),
                |_, _| panic!("warning"),
            );
            assert_eq!(value, Ok(R::Bytes(vec![0xff, 0])));
        }
        let value = native_cast_string::<(), ()>(
            I::Bytes(b"a"),
            T::Char {
                len: Some(u32::MAX),
                charset: Some("BINARY"),
            },
            None,
            || panic!("charset"),
            || panic!("SQL string"),
            || panic!("CHAR binary never pads"),
            |_| panic!("handler"),
            |_, _| panic!("warning"),
        );
        assert_eq!(value, Ok(R::Bytes(b"a".to_vec())));
        let reads = Cell::new(0);
        let order = RefCell::new(Vec::new());
        let value = native_cast_string::<(), ()>(
            I::Bytes(b"a"),
            T::Binary {
                len: Some(u32::MAX),
            },
            None,
            || panic!("charset"),
            || panic!("SQL string"),
            || {
                reads.set(reads.get() + 1);
                order.borrow_mut().push("limit");
                8
            },
            |function| {
                assert_eq!(function, "cast_as_binary");
                reads.set(reads.get() + 1);
                order.borrow_mut().push("original handler re-read");
                Ok(())
            },
            |_, _| panic!("no 1406 when padding"),
        );
        assert_eq!(value, Ok(R::Null));
        assert_eq!(reads.get(), 2);
        assert_eq!(*order.borrow(), ["limit", "original handler re-read"]);
        let value = native_cast_string::<&str, ()>(
            I::Bytes(b"a"),
            T::Binary { len: Some(10) },
            None,
            || panic!("charset"),
            || panic!("SQL string"),
            || 1,
            |_| Err("packet policy error"),
            |_, _| panic!("warning"),
        );
        assert_eq!(value, Err(E::Child("packet policy error")));
        let value = native_cast_string::<(), &str>(
            I::Other,
            T::Binary { len: Some(10) },
            None,
            || panic!("charset"),
            || Err("actual datatype stringification error"),
            || panic!("error precedes packet read"),
            |_| panic!("handler"),
            |_, _| panic!("error precedes warning"),
        );
        assert_eq!(value, Err(E::InvalidUtf8StringCoercion));
    }
}
