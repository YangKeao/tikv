// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Actual native TIMESTAMPDIFF values; wire validation remains with its owner.
use tidb_query_datatype::codec::{
    mysql::{
        Time,
        time::{NativeTimestampDiffTextEndpoint as Endpoint, NativeTimestampInterval},
    },
    native_sql_string::NativeSqlStringInput,
};

use crate::{NativeStringCoercionError as Error, native_coerce_string_bounded};
fn check(held: usize, work: usize, maximum: usize) -> Result<(), Error> {
    if held.checked_add(work).is_none_or(|n| n > maximum) {
        Err(Error::ResourceLimit)
    } else {
        Ok(())
    }
}
fn endpoint(text: &str, held: usize, maximum: usize) -> Result<Option<Endpoint>, Error> {
    // The original date splitter grows a String and a Vec<(u32, usize)>.
    // Charge its real growth and small allocation floors only when demanded.
    let work = if text.is_empty() {
        0
    } else {
        text.len()
            .checked_mul(32)
            .and_then(|n| n.checked_add(256))
            .ok_or(Error::ResourceLimit)?
    };
    check(held, work, maximum)?;
    Ok(Endpoint::parse(text))
}
/// Coerce ALL three original inputs in order, including after actual NULL.
/// Then parse endpoints and compute the delta before folding the unit.
pub fn native_timestamp_diff_text_values(
    inputs: [NativeSqlStringInput<'_>; 3],
    maximum: usize,
) -> Result<Option<i64>, Error> {
    let mut held = 0usize;
    let mut values = [None, None, None];
    for (value, input) in values.iter_mut().zip(inputs) {
        let remaining = maximum.checked_sub(held).ok_or(Error::ResourceLimit)?;
        if matches!(input, NativeSqlStringInput::Null) {
            continue;
        }
        let text = native_coerce_string_bounded(input, remaining)?;
        held = held
            .checked_add(text.capacity())
            .ok_or(Error::ResourceLimit)?;
        *value = Some(text);
    }
    let [Some(unit), Some(left), Some(right)] = values else {
        return Ok(None);
    };
    let Some(left) = endpoint(&left, held, maximum)? else {
        return Ok(None);
    };
    let Some(right) = endpoint(&right, held, maximum)? else {
        return Ok(None);
    };
    let difference = left.difference(right);
    check(held, unit.len(), maximum)?;
    Ok(Some(difference.finish(&unit)))
}
/// Actual optional complete calendar bits and exact raw unit bytes. No input
/// cloning, text rendering or LE64 report is needed.
pub fn native_timestamp_diff_core_values(
    unit: &[u8],
    left: Option<u64>,
    right: Option<u64>,
) -> Option<i64> {
    let (Some(left), Some(right)) = (left, right) else {
        return None;
    };
    if left == 0 || right == 0 {
        return None;
    }
    let Some(interval) = NativeTimestampInterval::from_uppercase_bytes(unit) else {
        return Some(0);
    };
    Some(Time::native_core_timestamp_diff(left, right, interval))
}

#[cfg(test)]
mod tests {
    use NativeSqlStringInput as I;
    use tidb_query_datatype::codec::mysql::NativeDecimalParseRef;

    use super::*;
    #[test]
    fn actual_values_preserve_null_coercion_errors_distinct_core_and_late_work() {
        assert_eq!(
            native_timestamp_diff_text_values(
                [
                    I::String(b"day"),
                    I::String(b"0000-01-01"),
                    I::String(b"0000-03-01")
                ],
                8192
            ),
            Ok(Some(60))
        );
        let jan = Time::native_core_from_fields(0, 1, 1, 0, 0, 0, 0);
        let mar = Time::native_core_from_fields(0, 3, 1, 0, 0, 0, 0);
        assert_eq!(
            native_timestamp_diff_core_values(b"DAY", Some(jan), Some(mar)),
            Some(59)
        );
        assert_eq!(
            native_timestamp_diff_core_values(b"day", Some(jan), Some(mar)),
            Some(0)
        );
        assert_eq!(
            native_timestamp_diff_core_values(b"\xff", Some(0), Some(mar)),
            None
        );
        assert_eq!(
            native_timestamp_diff_core_values(b"SECOND", Some(1), Some(15)),
            Some(0)
        );
        assert_eq!(
            native_timestamp_diff_text_values([I::Null, I::Bytes(b"\xff"), I::Null], 8192),
            Err(Error::Unsupported("invalid UTF-8 byte datum"))
        );
        let unit = "x".repeat(700);
        // All strings fit, but folding the unknown unit would exceed 800.
        // Empty left fails before folding, so that skipped copy is not charged.
        assert_eq!(
            native_timestamp_diff_text_values(
                [
                    I::String(unit.as_bytes()),
                    I::String(b""),
                    I::String(b"2020-01-01")
                ],
                800
            ),
            Ok(None)
        );
        assert_eq!(
            native_timestamp_diff_text_values([I::Null, I::Null, I::Null], 0),
            Ok(None)
        );
    }
    #[test]
    fn actual_values_gate_demanded_decimal_float_and_parser_storage() {
        let huge = I::Decimal(NativeDecimalParseRef {
            negative: false,
            digits: b"1",
            scale: u32::MAX,
            storage_scale: 0,
            declared_shape: None,
        });
        assert_eq!(
            native_timestamp_diff_text_values([I::Null, huge, I::Null], 8192),
            Err(Error::ResourceLimit)
        );
        assert_eq!(
            native_timestamp_diff_text_values([I::Bytes(b"\xff"), huge, I::Null], 8192),
            Err(Error::Unsupported("invalid UTF-8 byte datum"))
        );
        assert_eq!(
            native_timestamp_diff_text_values([I::Real(f64::MAX), I::Null, I::Null], 128),
            Err(Error::ResourceLimit)
        );
        assert_eq!(
            native_timestamp_diff_text_values(
                [
                    I::String(b"DAY"),
                    I::String(b"2020-01-01"),
                    I::String(b"2020-01-02")
                ],
                100
            ),
            Err(Error::ResourceLimit)
        );
        assert_eq!(
            native_timestamp_diff_text_values(
                [
                    I::String(b"DAY"),
                    I::String(b"2020-01-01"),
                    I::String(b"2020-01-02")
                ],
                8192
            ),
            Ok(Some(1))
        );
    }
}
