// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Checked transport, not SQL conversion. Keep native values and hidden scale.
use tidb_query_datatype::codec::mysql::{
    DecimalDecoder, DecimalEncoder, JsonDecoder, JsonEncoder, JsonType, TimeDecoder, TimeEncoder,
};

use super::{DateTime, Decimal, Duration, Error, Json};

/// Decode the native-endian 40-byte TiDB/TiKV decimal chunk layout, preserving
/// storage fraction and result fraction independently. This is not a stable
/// ABI. Unlike the engine's trusted chunk decoder, this checks the Rust bool
/// and decimal word invariants before constructing a native Decimal. Go's
/// zero-digit zero is normalized to one integer digit, preserving result scale,
/// because native decimal arithmetic assumes at least one storage word.
pub fn decimal_from_chunk(bytes: &[u8]) -> Result<Decimal, Error> {
    if bytes.len() != 40 {
        return Err(Error::invalid(
            "decimal chunk must contain exactly 40 bytes",
        ));
    }
    let ints = usize::from(bytes[0]);
    let frac = usize::from(bytes[1]);
    let int_words = ints.div_ceil(9);
    let frac_words = frac.div_ceil(9);
    if bytes[3] > 1 || int_words + frac_words > 9 || bytes[2] > 30 {
        return Err(Error::invalid("invalid decimal chunk header"));
    }
    let words: Vec<u32> = bytes[4..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| u32::from_ne_bytes(*word))
        .collect();
    if words.iter().any(|&word| word >= 1_000_000_000)
        || (!ints.is_multiple_of(9) && words[0] >= 10u32.pow((ints % 9) as u32))
        || (!frac.is_multiple_of(9)
            && !words[int_words + frac_words - 1].is_multiple_of(10u32.pow((9 - frac % 9) as u32)))
    {
        return Err(Error::invalid("invalid decimal chunk digits"));
    }
    if int_words + frac_words == 0 {
        if words.iter().any(|&word| word != 0) {
            return Err(Error::invalid("zero-digit decimal has nonzero storage"));
        }
        // Go's default decimal uses no digit words. TiKV's digit_bounds assumes
        // at least one word (e.g. shift(1) underflows on a zero-word value).
        // Canonicalize only this zero representation, retaining result scale.
        let mut canonical = [0u8; 40];
        canonical[0] = 1;
        canonical[2] = bytes[2];
        canonical[3] = bytes[3];
        return canonical
            .as_slice()
            .read_decimal_from_chunk()
            .map_err(Error::from);
    }
    let mut input = bytes;
    input.read_decimal_from_chunk().map_err(Error::from)
}

/// Encode every native decimal field without formatting/rounding through text.
pub fn decimal_to_chunk(value: &Decimal) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::with_capacity(40);
    bytes.write_decimal_to_chunk(value)?;
    Ok(bytes)
}

/// Decode an eight-byte little-endian TiDB Time chunk. Unlike packed timestamp
/// constants this preserves wall time, type and FSP without a timezone
/// conversion.
pub fn date_time_from_chunk(bytes: &[u8]) -> Result<DateTime, Error> {
    if bytes.len() != 8 {
        return Err(Error::invalid(
            "datetime chunk must contain exactly eight bytes",
        ));
    }
    let mut input = bytes;
    let value = input.read_time_from_chunk()?;
    validate_time(value)?;
    Ok(value)
}

/// Encode the TiDB Time chunk representation, without a timezone conversion.
pub fn date_time_to_chunk(value: &DateTime) -> Result<Vec<u8>, Error> {
    validate_time(*value)?;
    let mut bytes = Vec::with_capacity(8);
    bytes.write_time(*value)?;
    Ok(bytes)
}

pub(super) fn validate_time(value: DateTime) -> Result<(), Error> {
    // Zero components and invalid calendar dates can be intentional SQL values;
    // only reject components outside the engine's representable domain.
    if value.year() > 9999
        || value.month() > 12
        || value.day() > 31
        || value.hour() > 23
        || value.minute() > 59
        || value.second() > 59
        || value.micro() > 999_999
        || value.fsp() > 6
    {
        return Err(Error::invalid("invalid datetime components"));
    }
    Ok(())
}

/// Decode TiDB binary JSON (type byte followed by payload) after checking all
/// offsets and nested shapes. The engine's decoder assumes trusted payloads.
pub fn json_from_binary(bytes: &[u8]) -> Result<Json, Error> {
    let (&tag, payload) = bytes
        .split_first()
        .ok_or_else(|| Error::invalid("empty JSON"))?;
    validate_json_payload(tag, payload, 0, true)?;
    let mut input = bytes;
    input.read_json().map_err(Error::from)
}

/// Encode binary JSON without changing number tags, opaque data or temporal
/// data.
pub fn json_to_binary(value: &Json) -> Result<Vec<u8>, Error> {
    validate_json(value)?;
    let mut bytes = Vec::new();
    bytes.write_json(value.as_ref())?;
    Ok(bytes)
}

pub(super) fn validate_json(value: &Json) -> Result<(), Error> {
    validate_json_payload(value.get_type() as u8, &value.value, 0, true).map(|_| ())
}

fn invalid_json() -> Error {
    Error::invalid("invalid binary JSON shape")
}
fn u32_at(bytes: &[u8], offset: usize) -> Result<usize, Error> {
    let data = bytes
        .get(offset..offset.checked_add(4).ok_or_else(invalid_json)?)
        .ok_or_else(invalid_json)?;
    Ok(u32::from_le_bytes(data.try_into().unwrap()) as usize)
}
fn variable_len(bytes: &[u8]) -> Result<(usize, usize), Error> {
    let mut value = 0u64;
    for (i, &b) in bytes.iter().take(10).enumerate() {
        if i == 9 && b > 1 {
            return Err(invalid_json());
        }
        value |= u64::from(b & 127) << (7 * i);
        if b < 128 {
            return Ok((usize::try_from(value).map_err(|_| invalid_json())?, i + 1));
        }
    }
    Err(invalid_json())
}

// Return consumed length so nested values can be checked without trusting their
// offsets. Require nonoverlapping forward payloads to bound traversal by bytes.
fn validate_json_payload(tag: u8, bytes: &[u8], depth: usize, exact: bool) -> Result<usize, Error> {
    if depth > 64 {
        return Err(Error::invalid("binary JSON nesting exceeds 64"));
    }
    let tp = JsonType::try_from(tag)?;
    let len = match tp {
        JsonType::Object | JsonType::Array => {
            let count = u32_at(bytes, 0)?;
            let size = u32_at(bytes, 4)?;
            let data = bytes.get(..size).ok_or_else(invalid_json)?;
            let object = tp == JsonType::Object;
            let entries = count
                .checked_mul(if object { 11 } else { 5 })
                .and_then(|n| n.checked_add(8))
                .ok_or_else(invalid_json)?;
            if entries > size {
                return Err(invalid_json());
            }
            let mut end = entries;
            let mut previous_key: Option<&[u8]> = None;
            if object {
                for i in 0..count {
                    let pos = 8 + i * 6;
                    let offset = u32_at(data, pos)?;
                    let len =
                        u16::from_le_bytes(data[pos + 4..pos + 6].try_into().unwrap()) as usize;
                    let key_end = offset.checked_add(len).ok_or_else(invalid_json)?;
                    if offset < end {
                        return Err(invalid_json());
                    }
                    let key = data.get(offset..key_end).ok_or_else(invalid_json)?;
                    std::str::from_utf8(key).map_err(|_| invalid_json())?;
                    if previous_key.is_some_and(|previous| previous >= key) {
                        return Err(invalid_json());
                    }
                    previous_key = Some(key);
                    end = key_end;
                }
            }
            let start = 8 + if object { count * 6 } else { 0 };
            for i in 0..count {
                let pos = start + i * 5;
                let tag = data[pos];
                if tag == JsonType::Literal as u8 {
                    validate_json_payload(tag, &data[pos + 1..pos + 2], depth + 1, true)?;
                } else {
                    let offset = u32_at(data, pos + 1)?;
                    if offset < end {
                        return Err(invalid_json());
                    }
                    let value = data.get(offset..).ok_or_else(invalid_json)?;
                    let len = validate_json_payload(tag, value, depth + 1, false)?;
                    end = offset.checked_add(len).ok_or_else(invalid_json)?;
                }
            }
            size
        }
        JsonType::Literal => {
            if bytes.first().is_none_or(|&v| v > 2) {
                return Err(invalid_json());
            }
            1
        }
        JsonType::I64 | JsonType::U64 => 8,
        JsonType::Double => {
            let value = bytes.get(..8).ok_or_else(invalid_json)?;
            if !f64::from_le_bytes(value.try_into().unwrap()).is_finite() {
                return Err(invalid_json());
            }
            8
        }
        JsonType::String | JsonType::Opaque => {
            let prefix = usize::from(tp == JsonType::Opaque);
            let (len, header) = variable_len(bytes.get(prefix..).ok_or_else(invalid_json)?)?;
            let start = prefix + header;
            let end = start.checked_add(len).ok_or_else(invalid_json)?;
            let payload = bytes.get(start..end).ok_or_else(invalid_json)?;
            if tp == JsonType::String {
                std::str::from_utf8(payload).map_err(|_| invalid_json())?;
            }
            end
        }
        JsonType::Date | JsonType::Datetime | JsonType::Timestamp => {
            let mut input = bytes;
            validate_time(input.read_time_from_chunk()?)?;
            8
        }
        JsonType::Time => {
            let value = bytes.get(..12).ok_or_else(invalid_json)?;
            let nanos = i64::from_le_bytes(value[..8].try_into().unwrap());
            let fsp = u32_at(value, 8)?;
            if fsp > 6 {
                return Err(invalid_json());
            }
            Duration::from_nanos(nanos, fsp as i8)?;
            12
        }
    };
    if len > bytes.len() || (exact && len != bytes.len()) {
        return Err(invalid_json());
    }
    Ok(len)
}
