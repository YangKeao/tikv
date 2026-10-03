// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native FORMAT rounding over the shared locale formatter. SQL coercion and
//! warning delivery stay with callers; no locale status is encoded in output.

/// Formats actual numeric text at the original clamped precision. A SQL-NULL
/// locale selects en_US; the caller retains its distinct warning timing.
pub(crate) fn format_locale(number: &str, precision: i64, locale: Option<&str>) -> Vec<u8> {
    let precision = precision.clamp(0, 30) as usize;
    let rounded = round_format_args(number, precision);
    let (formatted, _) = tidb_query_datatype::codec::mysql::locale::format_by_locale(
        &rounded,
        &precision.to_string(),
        locale.unwrap_or("en_US"),
    )
    .unwrap_or_else(|never| match never {});
    formatted
}

fn round_format_args(number: &str, precision: usize) -> String {
    let (negative, number) = number
        .strip_prefix('-')
        .map_or((false, number), |n| (true, n));
    let (mut integer, fraction) = number.split_once('.').unwrap_or((number, ""));
    if !integer.bytes().all(|digit| digit.is_ascii_digit())
        || !fraction.bytes().all(|digit| digit.is_ascii_digit())
    {
        integer = "0";
    }
    let mut fraction: Vec<u8> = fraction.bytes().take(precision).collect();
    while fraction.len() < precision {
        fraction.push(b'0');
    }
    let round_up = number
        .split_once('.')
        .and_then(|(_, f)| f.as_bytes().get(precision))
        .is_some_and(|d| *d >= b'5');
    if round_up {
        let mut carry = true;
        for digit in fraction.iter_mut().rev() {
            if *digit == b'9' {
                *digit = b'0';
            } else {
                *digit += 1;
                carry = false;
                break;
            }
        }
        if carry {
            let mut digits = integer.as_bytes().to_vec();
            for digit in digits.iter_mut().rev() {
                if *digit == b'9' {
                    *digit = b'0';
                } else {
                    *digit += 1;
                    carry = false;
                    break;
                }
            }
            if carry {
                return format_number_parts(
                    negative,
                    format!("1{}", "0".repeat(integer.len())),
                    fraction,
                );
            }
            return format_number_parts(negative, String::from_utf8(digits).unwrap(), fraction);
        }
    }
    format_number_parts(negative, integer.to_string(), fraction)
}

fn format_number_parts(negative: bool, integer: String, fraction: Vec<u8>) -> String {
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&integer);
    if !fraction.is_empty() {
        out.push('.');
        out.push_str(std::str::from_utf8(&fraction).unwrap());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_format_keeps_rounding_clamps_and_locale_fallback() {
        assert_eq!(round_format_args("999.995", 2), "1000.00");
        assert_eq!(round_format_args("-0.004", 2), "-0.00");
        assert_eq!(round_format_args("1e20", 2), "0.00");
        assert_eq!(format_locale("1234.565", 2, Some("en_US")), b"1,234.57");
        assert_eq!(format_locale("1234.565", 2, Some("de_DE")), b"1.234,57");
        assert_eq!(format_locale("1234.565", 2, None), b"1,234.57");
        assert_eq!(format_locale("1234.565", 2, Some("unknown")), b"1,234.57");
        assert_eq!(format_locale("1234.5", i64::MIN, Some("en_US")), b"1,235");
        assert_eq!(
            format_locale("1", i64::MAX, Some("en_US")),
            b"1.000000000000000000000000000000",
        );
    }
}
