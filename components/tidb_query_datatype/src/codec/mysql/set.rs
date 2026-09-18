// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    cmp::Ordering,
    fmt::{Display, Formatter},
};

use codec::prelude::*;
use tipb::FieldType;

use crate::{
    FieldTypeTp,
    codec::{
        Error, Result,
        convert::{ToInt, ToStringValue},
    },
    expr::EvalContext,
};

/// `Set` stores a MySQL SET value.
///
/// A MySQL SET value has two equivalent representations:
/// - `value`: the bit mask whose bit `i` is set when the `i`-th declared
///   element (`FieldType.elems[i]`) is selected.
/// - `name`: the comma-joined names of the selected elements, in declaration
///   order. A zero value has an empty name.
#[derive(Clone, Debug)]
pub struct Set {
    name: Vec<u8>,
    value: u64,
}

impl Set {
    pub fn new(name: Vec<u8>, value: u64) -> Self {
        if value == 0 {
            Self {
                name: vec![],
                value,
            }
        } else {
            Self { name, value }
        }
    }

    pub fn value(&self) -> u64 {
        self.value
    }

    pub fn value_ref(&self) -> &u64 {
        &self.value
    }

    pub fn name(&self) -> &[u8] {
        self.name.as_slice()
    }

    pub fn as_ref(&self) -> SetRef<'_> {
        SetRef {
            name: &self.name,
            value: &self.value,
        }
    }

    /// Reconstructs the comma-joined element name of the bit mask `value` from
    /// the declaration-ordered `elems`.
    ///
    /// This mirrors TiDB's `types.ParseSetValue`: elements are emitted in
    /// declaration order, and a bit set outside the declared element range is
    /// an error rather than silent truncation.
    fn get_value_name(value: u64, elems: &[String]) -> Result<Vec<u8>> {
        if value == 0 {
            return Ok(Vec::new());
        }
        let mut name = Vec::new();
        let mut remaining = value;
        // `value` is a `u64`, so elements beyond the 64th declared one have no
        // representable bit and are skipped (TiDB refuses a 65th element).
        for (idx, elem) in elems.iter().take(64).enumerate() {
            let bit = 1u64 << idx;
            if remaining & bit == 0 {
                continue;
            }
            if !name.is_empty() {
                name.push(b',');
            }
            name.extend_from_slice(elem.as_bytes());
            remaining &= !bit;
        }
        if remaining != 0 {
            return Err(Error::InvalidDataType(format!(
                "invalid number {} for Set",
                remaining
            )));
        }
        Ok(name)
    }
}

impl Display for Set {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.as_ref().fmt(f)
    }
}

impl Eq for Set {}

impl PartialEq for Set {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Ord for Set {
    fn cmp(&self, other: &Self) -> Ordering {
        self.value.cmp(&other.value)
    }
}

impl PartialOrd for Set {
    fn partial_cmp(&self, right: &Self) -> Option<Ordering> {
        Some(self.cmp(right))
    }
}

impl crate::codec::data_type::AsMySqlBool for Set {
    #[inline]
    fn as_mysql_bool(&self, _context: &mut crate::expr::EvalContext) -> crate::codec::Result<bool> {
        Ok(self.value > 0)
    }
}

impl std::hash::Hash for Set {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.value.hash(state)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SetRef<'a> {
    name: &'a [u8],
    value: &'a u64,
}

impl<'a> SetRef<'a> {
    pub fn new(name: &'a [u8], value: &'a u64) -> Self {
        if *value == 0 {
            Self { name: b"", value }
        } else {
            Self { name, value }
        }
    }

    pub fn to_owned(self) -> Set {
        Set {
            name: self.name.to_owned(),
            value: *self.value,
        }
    }

    /// Whether the element at `idx` (declaration order) is selected.
    pub fn is_set(&self, idx: usize) -> bool {
        self.value & (1 << idx) != 0
    }

    pub fn is_empty(&self) -> bool {
        *self.value == 0
    }

    pub fn value(&self) -> u64 {
        *self.value
    }

    pub fn value_ref(&self) -> &'a u64 {
        self.value
    }

    pub fn name(&self) -> &'a [u8] {
        self.name
    }

    pub fn as_str(&self) -> Result<&str> {
        Ok(std::str::from_utf8(self.name)?)
    }

    pub fn len(&self) -> usize {
        8 + self.name.len()
    }
}

impl Display for SetRef<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if *self.value == 0 {
            return Ok(());
        }

        write!(f, "{}", String::from_utf8_lossy(self.name))
    }
}

impl Eq for SetRef<'_> {}

impl PartialEq for SetRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Ord for SetRef<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.value.cmp(other.value)
    }
}

impl PartialOrd for SetRef<'_> {
    fn partial_cmp(&self, right: &Self) -> Option<Ordering> {
        Some(self.cmp(right))
    }
}

impl ToInt for SetRef<'_> {
    fn to_int(&self, _ctx: &mut EvalContext, _tp: FieldTypeTp) -> Result<i64> {
        Ok(*self.value as i64)
    }

    fn to_uint(&self, _ctx: &mut EvalContext, _tp: FieldTypeTp) -> Result<u64> {
        Ok(*self.value)
    }
}

impl ToStringValue for SetRef<'_> {
    fn to_string_value(&self) -> String {
        String::from_utf8_lossy(self.name).to_string()
    }
}

pub trait SetEncoder: NumberEncoder {
    #[inline]
    fn write_set_uint(&mut self, data: SetRef<'_>) -> Result<()> {
        self.write_u64(data.value())?;
        Ok(())
    }

    #[inline]
    fn write_set_to_chunk(&mut self, value: u64, name: &[u8]) -> Result<()> {
        self.write_u64_le(value)?;
        self.write_bytes(name)?;
        Ok(())
    }
}

impl<T: BufferWriter> SetEncoder for T {}

pub trait SetDatumPayloadChunkEncoder: NumberEncoder + SetEncoder {
    #[inline]
    fn write_set_to_chunk_by_datum_payload_compact_bytes(
        &mut self,
        mut src_payload: &[u8],
        field_type: &FieldType,
    ) -> Result<()> {
        let vn = src_payload.read_var_i64()? as usize;
        let mut data = src_payload.read_bytes(vn)?;
        let value = data.read_var_u64()?;
        let name = Set::get_value_name(value, field_type.get_elems())?;
        self.write_set_to_chunk(value, &name)
    }

    #[inline]
    fn write_set_to_chunk_by_datum_payload_uint(
        &mut self,
        mut src_payload: &[u8],
        field_type: &FieldType,
    ) -> Result<()> {
        let value = src_payload.read_u64()?;
        let name = Set::get_value_name(value, field_type.get_elems())?;
        self.write_set_to_chunk(value, &name)
    }

    #[inline]
    fn write_set_to_chunk_by_datum_payload_var_uint(
        &mut self,
        mut src_payload: &[u8],
        field_type: &FieldType,
    ) -> Result<()> {
        let value = src_payload.read_var_u64()?;
        let name = Set::get_value_name(value, field_type.get_elems())?;
        self.write_set_to_chunk(value, &name)
    }
}

impl<T: BufferWriter> SetDatumPayloadChunkEncoder for T {}

pub trait SetDecoder: NumberDecoder {
    #[inline]
    fn read_set_compact_bytes(&mut self, field_type: &FieldType) -> Result<Set> {
        let vn = self.read_var_i64()? as usize;
        let mut data = self.read_bytes(vn)?;
        let value = data.read_var_u64()?;
        let name = Set::get_value_name(value, field_type.get_elems())?;
        Ok(Set::new(name, value))
    }

    #[inline]
    fn read_set_uint(&mut self, field_type: &FieldType) -> Result<Set> {
        let value = self.read_u64()?;
        let name = Set::get_value_name(value, field_type.get_elems())?;
        Ok(Set::new(name, value))
    }

    #[inline]
    fn read_set_var_uint(&mut self, field_type: &FieldType) -> Result<Set> {
        let value = self.read_var_u64()?;
        let name = Set::get_value_name(value, field_type.get_elems())?;
        Ok(Set::new(name, value))
    }

    /// Reads `[u64 little-endian bit mask][name bytes]` from a chunk cell.
    ///
    /// The name bytes are preserved verbatim: element names are not required to
    /// be valid UTF-8, and the native encoder copies the raw bytes.
    #[inline]
    fn read_set_from_chunk(&mut self) -> Result<Set> {
        let value = self.read_u64_le()?;
        let name = self.bytes().to_vec();
        Ok(Set::new(name, value))
    }
}

impl<T: BufferReader> SetDecoder for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_string() {
        let cases = vec![
            ("a", 0b001, "a"),
            ("a,b", 0b011, "a,b"),
            ("a,c", 0b101, "a,c"),
            ("", 0, ""),
        ];

        for (name, value, expect) in cases {
            let s = Set::new(name.as_bytes().to_vec(), value);
            assert_eq!(s.to_string(), expect.to_string())
        }
    }

    #[test]
    fn test_as_str() {
        let cases = vec![("a", &1, "a"), ("a,b", &3, "a,b")];

        for (name, value, expect) in cases {
            let s = SetRef::new(name.as_bytes(), value);
            assert_eq!(s.as_str().expect("get str correctly"), expect)
        }
    }

    #[test]
    fn test_is_empty() {
        let s = Set::new("a,b".as_bytes().to_vec(), 0b11);
        assert!(!s.as_ref().is_empty());

        let s = Set::new("a,b".as_bytes().to_vec(), 0);
        assert!(s.as_ref().is_empty());
        // A zero value always has an empty name.
        assert_eq!(s.name(), b"");
    }

    #[test]
    fn test_is_set_and_value() {
        let s = Set::new("a,b,c".as_bytes().to_vec(), 0b101);
        assert!(s.as_ref().is_set(0));
        assert!(!s.as_ref().is_set(1));
        assert!(s.as_ref().is_set(2));
        assert_eq!(s.value(), 0b101);
        assert_eq!(*s.value_ref(), 0b101);
    }

    fn get_set_field_type() -> FieldType {
        let mut field_type = FieldType::new();
        field_type.set_tp(FieldTypeTp::Set.to_u8().unwrap() as i32);

        let elems = protobuf::RepeatedField::from_slice(&[
            String::from("a"),
            String::from("b"),
            String::from("c"),
        ]);
        field_type.set_elems(elems);

        field_type
    }

    #[test]
    fn test_get_value_name() {
        let elems: Vec<String> = vec!["a".into(), "b".into(), "c".into()];

        // Empty selection.
        assert_eq!(Set::get_value_name(0, &elems).unwrap(), b"");
        // Single element.
        assert_eq!(Set::get_value_name(0b001, &elems).unwrap(), b"a");
        // Multi element, declaration order (low bit first).
        assert_eq!(Set::get_value_name(0b101, &elems).unwrap(), b"a,c");
        assert_eq!(Set::get_value_name(0b111, &elems).unwrap(), b"a,b,c");
        // A bit outside the declared element range is an error, not silent
        // truncation (mirrors TiDB's ParseSetValue).
        assert!(Set::get_value_name(0b1000, &elems).is_err());
    }

    #[test]
    fn test_read_set_uint() {
        let field_type = get_set_field_type();

        let mut data: &[u8] = &[
            0, 0, 0, 0, 0, 0, 0, 5, // 1st: a,c
            0, 0, 0, 0, 0, 0, 0, 3, // 2nd: a,b
            0, 0, 0, 0, 0, 0, 0, 1, // 3rd: a
        ];
        let result = [
            Set::new("a,c".as_bytes().to_owned(), 5),
            Set::new("a,b".as_bytes().to_owned(), 3),
            Set::new("a".as_bytes().to_owned(), 1),
        ];
        for res in result.iter() {
            let got = data.read_set_uint(&field_type).expect("read_set_uint");
            assert_eq!(&got, res);
        }
    }

    #[test]
    fn test_read_set_var_uint() {
        let field_type = get_set_field_type();

        let mut data: &[u8] = &[
            5, // 1st: a,c
            3, // 2nd: a,b
            1, // 3rd: a
        ];
        let result = [
            Set::new("a,c".as_bytes().to_owned(), 5),
            Set::new("a,b".as_bytes().to_owned(), 3),
            Set::new("a".as_bytes().to_owned(), 1),
        ];
        for res in result.iter() {
            let got = data
                .read_set_var_uint(&field_type)
                .expect("read_set_var_uint");
            assert_eq!(&got, res);
        }
    }

    #[test]
    fn test_read_set_compact_bytes() {
        let field_type = get_set_field_type();

        let mut data: &[u8] = &[
            2, 5, // 1st: a,c
            2, 3, // 2nd: a,b
            2, 1, // 3rd: a
        ];
        let result = [
            Set::new("a,c".as_bytes().to_owned(), 5),
            Set::new("a,b".as_bytes().to_owned(), 3),
            Set::new("a".as_bytes().to_owned(), 1),
        ];
        for res in result.iter() {
            let got = data
                .read_set_compact_bytes(&field_type)
                .expect("read_set_compact_bytes");
            assert_eq!(&got, res);
        }
    }

    #[test]
    fn test_write_set_to_chunk() {
        let data = [
            (b"a,c".as_slice(), 5u64),
            (b"a,b".as_slice(), 3),
            (b"a".as_slice(), 1),
        ];
        let res: &[u8] = &[
            5, 0, 0, 0, 0, 0, 0, 0, 97, 44, 99, // 1st: a,c
            3, 0, 0, 0, 0, 0, 0, 0, 97, 44, 98, // 2nd: a,b
            1, 0, 0, 0, 0, 0, 0, 0, 97, // 3rd: a
        ];

        let mut buf = Vec::new();
        for datum in &data {
            buf.write_set_to_chunk(datum.1, datum.0)
                .expect("write_set_to_chunk");
        }
        assert_eq!(buf.as_slice(), res);
    }

    #[test]
    fn test_read_set_from_chunk() {
        // The chunk cell reader must preserve the raw name bytes, including
        // non-UTF8 element names.
        let cell: &[u8] = &[
            5, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xfe, // value 5, name 0xff 0xfe
        ];
        let mut data = cell;
        let got = data.read_set_from_chunk().expect("read_set_from_chunk");
        assert_eq!(got.value(), 5);
        assert_eq!(got.name(), &[0xff, 0xfe]);
    }

    #[test]
    fn test_write_set_to_chunk_by_payload_uint() {
        let field_type = get_set_field_type();

        let src: [&[u8]; 3] = [
            &[0, 0, 0, 0, 0, 0, 0, 5], // 1st: a,c
            &[0, 0, 0, 0, 0, 0, 0, 3], // 2nd: a,b
            &[0, 0, 0, 0, 0, 0, 0, 1], // 3rd: a
        ];
        let mut dest = Vec::new();

        let res: &[u8] = &[
            5, 0, 0, 0, 0, 0, 0, 0, 97, 44, 99, // 1st
            3, 0, 0, 0, 0, 0, 0, 0, 97, 44, 98, // 2nd
            1, 0, 0, 0, 0, 0, 0, 0, 97, // 3rd
        ];
        for data in &src {
            dest.write_set_to_chunk_by_datum_payload_uint(data, &field_type)
                .expect("write_set_to_chunk_by_payload_uint");
        }
        assert_eq!(&dest, res);
    }

    #[test]
    fn test_write_set_to_chunk_by_payload_var_uint() {
        let field_type = get_set_field_type();

        let src: [&[u8]; 3] = [
            &[5], // 1st: a,c
            &[3], // 2nd: a,b
            &[1], // 3rd: a
        ];
        let mut dest = Vec::new();

        let res: &[u8] = &[
            5, 0, 0, 0, 0, 0, 0, 0, 97, 44, 99, // 1st
            3, 0, 0, 0, 0, 0, 0, 0, 97, 44, 98, // 2nd
            1, 0, 0, 0, 0, 0, 0, 0, 97, // 3rd
        ];
        for data in &src {
            dest.write_set_to_chunk_by_datum_payload_var_uint(data, &field_type)
                .expect("write_set_to_chunk_by_payload_var_uint");
        }
        assert_eq!(&dest, res);
    }

    #[test]
    fn test_write_set_to_chunk_by_payload_compact_bytes() {
        let field_type = get_set_field_type();

        let src: [&[u8]; 3] = [
            &[2, 5], // 1st: a,c
            &[2, 3], // 2nd: a,b
            &[2, 1], // 3rd: a
        ];
        let mut dest = Vec::new();

        let res: &[u8] = &[
            5, 0, 0, 0, 0, 0, 0, 0, 97, 44, 99, // 1st
            3, 0, 0, 0, 0, 0, 0, 0, 97, 44, 98, // 2nd
            1, 0, 0, 0, 0, 0, 0, 0, 97, // 3rd
        ];
        for data in &src {
            dest.write_set_to_chunk_by_datum_payload_compact_bytes(data, &field_type)
                .expect("write_set_to_chunk_by_payload_compact_bytes");
        }
        assert_eq!(&dest, res);
    }

    #[test]
    fn test_write_set_empty_selection() {
        let field_type = get_set_field_type();
        let src: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 0];
        let mut dest = Vec::new();
        dest.write_set_to_chunk_by_datum_payload_uint(src, &field_type)
            .expect("write empty set");
        assert_eq!(dest.as_slice(), &[0, 0, 0, 0, 0, 0, 0, 0]);
    }
}
