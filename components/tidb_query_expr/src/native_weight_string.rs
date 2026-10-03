// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Fixed native WEIGHT_STRING policies over actual bytes and original metadata.
//! Host warnings/getter demand stay outside; padding, packet overflow, and key
//! construction are recomputed here rather than transported as outcomes.

use tidb_query_common::Result;
use tidb_query_datatype::codec::collation::{KeyOptions, native::NativeCollation};

fn padding_increment(source_length: usize, length: i64) -> Option<u64> {
    usize::try_from(length)
        .unwrap_or(0)
        .checked_sub(source_length)
        .map(|increment| increment as u64)
}

/// None means rune truncation. Some, including zero, requires the caller's
/// actual packet-budget getter. No padded answer or collation key is built.
pub fn native_weight_char_padding(bytes: &[u8], length: i64) -> Option<u64> {
    padding_increment(String::from_utf8_lossy(bytes).chars().count(), length)
}

/// None means byte truncation. Some, including zero, requires a packet budget.
pub fn native_weight_binary_padding(bytes: &[u8], length: i64) -> Option<u64> {
    padding_increment(bytes.len(), length)
}

/// Checks actual non-NULL bytes and `[original tag, demanded global flag]`.
pub fn native_weight_string_args_valid(value: Option<&[u8]>, meta: Option<&[u8]>) -> bool {
    value.is_some() && meta.and_then(plain_metadata).is_some()
}

/// Checks CHAR's original length, demanded budget, collation and global state.
pub fn native_weight_char_args_valid(value: Option<&[u8]>, meta: Option<&[u8]>) -> bool {
    match (value, meta) {
        (Some(value), Some(meta)) => padded_metadata(value, meta, Padding::Char).is_some(),
        _ => false,
    }
}

/// Checks BINARY's metadata without normalizing its original collation tag.
pub fn native_weight_binary_args_valid(value: Option<&[u8]>, meta: Option<&[u8]>) -> bool {
    match (value, meta) {
        (Some(value), Some(meta)) => padded_metadata(value, meta, Padding::Binary).is_some(),
        _ => false,
    }
}

/// Numeric-null signature selection carries the actual source FieldType code,
/// not a fabricated SQL-NULL witness or an evaluated numeric operand.
pub fn native_weight_numeric_type_valid(field_type: Option<i64>) -> bool {
    matches!(field_type, Some(1 | 2 | 3 | 4 | 5 | 8 | 9 | 16 | 246))
}

fn plain_metadata(meta: &[u8]) -> Option<(NativeCollation, bool)> {
    if meta.len() != 2 || meta[1] > 1 {
        return None;
    }
    Some((NativeCollation::from_tag(i64::from(meta[0]))?, meta[1] != 0))
}

#[derive(Clone, Copy)]
enum Padding {
    Char,
    Binary,
}

impl Padding {
    fn increment(self, value: &[u8], length: i64) -> Option<u64> {
        match self {
            Self::Char => native_weight_char_padding(value, length),
            Self::Binary => native_weight_binary_padding(value, length),
        }
    }
}

struct PaddedMetadata {
    length: usize,
    increment: Option<u64>,
    budget: Option<u64>,
    collation: NativeCollation,
    global: u8,
}

fn padded_metadata(value: &[u8], meta: &[u8], padding: Padding) -> Option<PaddedMetadata> {
    if meta.len() != 19 || meta[8] > 1 || meta[18] > 2 {
        return None;
    }
    let raw_length = i64::from_le_bytes(meta[..8].try_into().ok()?);
    let raw_budget = u64::from_le_bytes(meta[9..17].try_into().ok()?);
    let budget = if meta[8] == 1 {
        Some(raw_budget)
    } else {
        if raw_budget != 0 {
            return None;
        }
        None
    };
    let collation = NativeCollation::from_tag(i64::from(meta[17]))?;
    let increment = padding.increment(value, raw_length);
    if increment.is_some() != budget.is_some() {
        return None;
    }
    let overflow = increment
        .zip(budget)
        .is_some_and(|(growth, maximum)| growth > maximum);
    if (meta[18] == 2) != overflow {
        return None;
    }
    Some(PaddedMetadata {
        length: usize::try_from(raw_length).unwrap_or(0),
        increment,
        budget,
        collation,
        global: meta[18],
    })
}

fn key(value: &[u8], collation: NativeCollation, new_collation: bool) -> Result<Vec<u8>> {
    if new_collation {
        Ok(collation.key(value, KeyOptions::Default)?)
    } else {
        Ok(NativeCollation::Binary.key(value, KeyOptions::NoPad)?)
    }
}

pub(crate) fn weight_string(value: &[u8], meta: &[u8]) -> Result<Option<Vec<u8>>> {
    let (collation, new_collation) =
        plain_metadata(meta).ok_or_else(|| other_err!("Invalid native WEIGHT_STRING metadata"))?;
    key(value, collation, new_collation).map(Some)
}

pub(crate) fn weight_string_char(value: &[u8], meta: &[u8]) -> Result<Option<Vec<u8>>> {
    padded_weight(value, meta, Padding::Char)
}

pub(crate) fn weight_string_binary(value: &[u8], meta: &[u8]) -> Result<Option<Vec<u8>>> {
    padded_weight(value, meta, Padding::Binary)
}

fn padded_weight(value: &[u8], meta: &[u8], padding: Padding) -> Result<Option<Vec<u8>>> {
    let metadata = padded_metadata(value, meta, padding)
        .ok_or_else(|| other_err!("Invalid native padded WEIGHT_STRING metadata"))?;
    if metadata
        .increment
        .zip(metadata.budget)
        .is_some_and(|(growth, maximum)| growth > maximum)
    {
        return Ok(None);
    }
    let (padded, collation) = match padding {
        Padding::Char => {
            let padded = match metadata.increment {
                None => String::from_utf8_lossy(value)
                    .chars()
                    .take(metadata.length)
                    .collect::<String>()
                    .into_bytes(),
                Some(increment) => {
                    let mut padded = value.to_vec();
                    padded.extend(std::iter::repeat_n(b' ', increment as usize));
                    padded
                }
            };
            (padded, metadata.collation)
        }
        Padding::Binary => {
            let padded = if metadata.increment.is_none() {
                value[..metadata.length].to_vec()
            } else {
                let mut padded = value.to_vec();
                padded.resize(metadata.length, 0);
                padded
            };
            (padded, NativeCollation::Binary)
        }
    };
    key(&padded, collation, metadata.global == 1).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weight_metadata_padding_and_key_policies_keep_original_bytes() {
        let metadata = |length: i64, budget: Option<u64>, tag: u8, global: u8| {
            let mut meta = length.to_le_bytes().to_vec();
            meta.push(u8::from(budget.is_some()));
            meta.extend_from_slice(&budget.unwrap_or(0).to_le_bytes());
            meta.extend_from_slice(&[tag, global]);
            meta
        };
        assert_eq!(native_weight_char_padding("中a".as_bytes(), 1), None);
        assert_eq!(native_weight_binary_padding("中a".as_bytes(), 4), Some(0));
        assert_eq!(native_weight_char_padding(b"", -1), Some(0));
        assert_eq!(
            weight_string(b"ab ", &[6, 1]).unwrap(),
            Some(b"ab".to_vec())
        );
        assert_eq!(
            weight_string(b"ab ", &[6, 0]).unwrap(),
            Some(b"ab ".to_vec())
        );
        assert_eq!(weight_string(b"A", &[7, 1]).unwrap(), Some(vec![0, 65]));
        assert_eq!(weight_string(b"A", &[11, 0]).unwrap(), Some(b"A".to_vec()));
        assert!(native_weight_string_args_valid(Some(b"A"), Some(&[11, 1])));
        assert!(std::panic::catch_unwind(|| weight_string(b"A", &[11, 1])).is_err());
        assert_eq!(
            weight_string_char(&[255, b'a'], &metadata(1, None, 0, 0)).unwrap(),
            Some(vec![239, 191, 189])
        );
        assert_eq!(
            weight_string_char(&[255, b'a'], &metadata(3, Some(1), 0, 0)).unwrap(),
            Some(vec![255, b'a', b' '])
        );
        assert_eq!(
            weight_string_binary(b"ab", &metadata(4, Some(2), 11, 1)).unwrap(),
            Some(vec![b'a', b'b', 0, 0])
        );
        assert_eq!(
            weight_string_binary(b"ab", &metadata(-1, None, 11, 1)).unwrap(),
            Some(vec![])
        );
        let overflow = metadata(5, Some(1), 11, 2);
        assert!(native_weight_char_args_valid(Some(b"ab"), Some(&overflow)));
        assert_eq!(weight_string_char(b"ab", &overflow).unwrap(), None);
        for invalid in [
            metadata(2, None, 0, 0),
            metadata(1, Some(0), 0, 0),
            metadata(5, Some(1), 0, 1),
            metadata(2, Some(0), 0, 2),
            metadata(2, Some(0), 16, 0),
        ] {
            assert!(!native_weight_char_args_valid(Some(b"ab"), Some(&invalid)));
            assert!(weight_string_char(b"ab", &invalid).is_err());
        }
        let mut noncanonical = metadata(1, None, 0, 0);
        noncanonical[9] = 1;
        assert!(!native_weight_binary_args_valid(
            Some(b"ab"),
            Some(&noncanonical)
        ));
        assert!(!native_weight_string_args_valid(None, Some(&[0, 0])));
        assert!(!native_weight_string_args_valid(Some(b""), Some(&[0, 2])));
        for code in [1, 2, 3, 4, 5, 8, 9, 16, 246] {
            assert!(native_weight_numeric_type_valid(Some(code)));
        }
        assert!(!native_weight_numeric_type_valid(None));
        assert!(!native_weight_numeric_type_valid(Some(253)));
    }
}
