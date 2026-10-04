// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Shared lexical boundaries of the native temporal parser. These helpers do
//! not validate calendar fields or timezone values, and do not parse a Time.

/// Parsed trailing timezone fields from a native temporal literal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimezoneSuffix {
    /// Byte index at which the suffix begins.
    pub index: usize,
    /// `+` or `-`, absent for `Z`.
    pub sign: Option<char>,
    /// Two-digit hour, absent for `Z`.
    pub hour: Option<String>,
    /// Whether the source used `:`.
    pub has_colon: bool,
    /// Two-digit minute when present.
    pub minute: Option<String>,
}

/// Shared public name; retain the original struct name in derived Debug text.
pub type NativeTimezoneSuffix = TimezoneSuffix;

/// The single ASCII punctuation predicate used by both lexical domains.
pub const fn native_time_is_ascii_punctuation(byte: u8) -> bool {
    matches!(byte, 0x21..=0x2f | 0x3a..=0x40 | 0x5b..=0x60 | 0x7b..=0x7e)
}

/// Recognizes the original trailing `Z`, signed `HH`, `HHMM`, and `HH:MM`
/// shapes. Neither the preceding literal nor the numeric ranges are validated.
pub fn native_get_timezone(value: &str) -> Option<NativeTimezoneSuffix> {
    let bytes = value.as_bytes();
    if bytes.last() == Some(&b'Z') {
        return Some(NativeTimezoneSuffix {
            index: bytes.len() - 1,
            sign: None,
            hour: None,
            has_colon: false,
            minute: None,
        });
    }
    for suffix_length in [6_usize, 5, 3] {
        if bytes.len() < suffix_length {
            continue;
        }
        let index = bytes.len() - suffix_length;
        let sign = match bytes[index] {
            b'+' => '+',
            b'-' => '-',
            _ => continue,
        };
        let suffix = &bytes[index + 1..];
        let (hour, has_colon, minute) = match suffix_length {
            3 if suffix.iter().all(u8::is_ascii_digit) => (&suffix[..2], false, None),
            5 if suffix.iter().all(u8::is_ascii_digit) => {
                (&suffix[..2], false, Some(&suffix[2..4]))
            }
            6 if suffix[2] == b':'
                && suffix[..2].iter().all(u8::is_ascii_digit)
                && suffix[3..].iter().all(u8::is_ascii_digit) =>
            {
                (&suffix[..2], true, Some(&suffix[3..5]))
            }
            _ => continue,
        };
        return Some(NativeTimezoneSuffix {
            index,
            sign: Some(sign),
            hour: Some(String::from_utf8(hour.to_vec()).expect("ASCII digits")),
            has_colon,
            minute: minute
                .map(|minute| String::from_utf8(minute.to_vec()).expect("ASCII timezone minute")),
        });
    }
    None
}

/// Byte index of the fraction dot, or -1. Scan backwards before a recognized
/// timezone suffix and stop at the first punctuation other than `+` or `-`.
pub fn native_get_frac_index(value: &str) -> isize {
    let bytes = value.as_bytes();
    let end = native_get_timezone(value).map_or(bytes.len(), |timezone| timezone.index);
    for index in (0..end).rev() {
        let byte = bytes[index];
        if byte != b'+' && byte != b'-' && native_time_is_ascii_punctuation(byte) {
            return if byte == b'.' { index as isize } else { -1 };
        }
    }
    -1
}

/// Count every source byte after the selected dot, including trailing text and
/// timezone bytes, before capping at six; this is not a count of numeric
/// digits.
pub fn native_get_time_fsp(value: &str) -> u8 {
    let index = native_get_frac_index(value);
    if index < 0 {
        return 0;
    }
    (value.len() - index as usize - 1).min(6) as u8
}

const fn is_digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

const fn is_valid_separator(byte: u8, prev_parts: usize) -> bool {
    if native_time_is_ascii_punctuation(byte) {
        return true;
    }
    if prev_parts == 2 && matches!(byte, b'T' | b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        return true;
    }
    prev_parts > 4 && !is_digit(byte)
}

/// Split the original native date-format fields. Keep Rust `trim`, byte-based
/// scanning, separator runs and lossy field lifting exactly as before. The
/// outer scan does not examine the last byte, so it can join a trailing field
/// even when it is not a digit; separator-run consumption can still reach it.
#[must_use]
pub fn native_parse_date_format(format: &str) -> Option<Vec<String>> {
    let format = format.trim();
    let bytes = format.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    if !is_digit(bytes[0]) {
        return None;
    }
    let mut seps: Vec<String> = Vec::with_capacity(6);
    let mut start = 0usize;
    let mut i = 1usize;
    while i + 1 < bytes.len() {
        if is_valid_separator(bytes[i], seps.len()) {
            let prev_parts = seps.len();
            seps.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
            start = i + 1;
            let mut j = i + 1;
            while j < bytes.len() {
                if !is_valid_separator(bytes[j], prev_parts) {
                    break;
                }
                start += 1;
                i += 1;
                j += 1;
            }
            i += 1;
            continue;
        }
        if !is_digit(bytes[i]) {
            return None;
        }
        i += 1;
    }
    seps.push(String::from_utf8_lossy(&bytes[start..]).into_owned());
    Some(seps)
}

/// The native lexical date-only shape predicate, not calendar validation.
pub fn native_is_date_format(format: &str) -> bool {
    let format = format.trim();
    match native_parse_date_format(format).map_or(0, |parts| parts.len()) {
        1 => matches!(format.len(), 5 | 6 | 8),
        3 => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_temporal_lexical_tables_preserve_suffix_bytes_and_last_byte_boundaries() {
        for (input, expected) in [
            ("2020-10-10T10:10:10Z", Some((19, None, None, false, None))),
            ("2020-10-10T10:10:10", None),
            (
                "2020-10-10T10:10:10-08",
                Some((19, Some('-'), Some("08"), false, None)),
            ),
            (
                "2020-10-10T10:10:10-0700",
                Some((19, Some('-'), Some("07"), false, Some("00"))),
            ),
            (
                "2020-10-10T10:10:10+08:20",
                Some((19, Some('+'), Some("08"), true, Some("20"))),
            ),
            (
                "2020-10-10T10:10:10+08:10",
                Some((19, Some('+'), Some("08"), true, Some("10"))),
            ),
            ("2020-10-10T10:10:10+8:00", None),
            ("2020-10-10T10:10:10+082:10", None),
            ("2020-10-10T10:10:10+08:101", None),
            ("2020-10-10T10:10:10+T8:11", None),
            (
                "2020-09-06T05:49:13.293Z",
                Some((23, None, None, false, None)),
            ),
            ("2020-09-06T05:49:13.293", None),
            (
                "宽+99:99",
                Some((3, Some('+'), Some("99"), true, Some("99"))),
            ),
            ("宽Z", Some((3, None, None, false, None))),
            ("Z", Some((0, None, None, false, None))),
            ("z", None),
            ("+08 ", None),
            ("+０８", None),
            ("2011-11-11", Some((7, Some('-'), Some("11"), false, None))),
            ("", None),
        ] {
            let actual = native_get_timezone(input);
            assert_eq!(
                actual.as_ref().map(|suffix| (
                    suffix.index,
                    suffix.sign,
                    suffix.hour.as_deref(),
                    suffix.has_colon,
                    suffix.minute.as_deref(),
                )),
                expected,
                "{input:?}"
            );
        }
        assert_eq!(
            format!("{:?}", native_get_timezone("Z").unwrap()),
            "TimezoneSuffix { index: 0, sign: None, hour: None, has_colon: false, minute: None }"
        );
        for (input, index, fsp) in [
            ("2012-01-01 00:00:00", -1, 0),
            ("2012-01-01 00:00:00.1", 19, 1),
            ("00:00:00.1234567", 8, 6),
            ("1.2e3", 1, 3),
            ("2019.01.01 00:00:00", -1, 0),
            ("2019.01.01 00:00:00.1", 19, 1),
            ("12345.6", 5, 1),
            ("2020-01-01 12:00:00.123456 +0600 PST", 19, 6),
            ("2020-01-01 12:00:00.123456 -0600 PST", 19, 6),
            ("2020-01-01 12:00:00.1+05:00", 19, 6),
            ("2020-01-01 12:00:00.5xyz", 19, 4),
            ("宽.12Z", 3, 3),
            ("宽.界", 3, 3),
            ("1.2!", -1, 0),
            ("1.2-", 1, 2),
        ] {
            assert_eq!(native_get_frac_index(input), index, "{input:?}");
            assert_eq!(native_get_time_fsp(input), fsp, "{input:?}");
        }
        let parts = |values: &[&str]| {
            Some(
                values
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect::<Vec<_>>(),
            )
        };
        for (input, expected) in [
            (
                "2011-11-11 10:10:10.123456",
                parts(&["2011", "11", "11", "10", "10", "10", "123456"]),
            ),
            (
                "  2011-11-11 10:10:10.123456  ",
                parts(&["2011", "11", "11", "10", "10", "10", "123456"]),
            ),
            ("2011-11-11 10", parts(&["2011", "11", "11", "10"])),
            (
                "2011-11-11T10:10:10.123456",
                parts(&["2011", "11", "11", "10", "10", "10", "123456"]),
            ),
            (
                "2011:11:11T10:10:10.123456",
                parts(&["2011", "11", "11", "10", "10", "10", "123456"]),
            ),
            (
                "2011-11-11  10:10:10",
                parts(&["2011", "11", "11", "10", "10", "10"]),
            ),
            ("xx2011-11-11 10:10:10", None),
            ("T10:10:10", None),
            ("2011-11-11x", parts(&["2011", "11", "11x"])),
            ("xxx 10:10:10", None),
            ("\u{2003}2011-11-11x\u{2003}", parts(&["2011", "11", "11x"])),
            ("1x", parts(&["1x"])),
            ("1xy", None),
            ("1é", None),
            ("1-", parts(&["1-"])),
            ("1--", parts(&["1", ""])),
            ("", None),
        ] {
            assert_eq!(native_parse_date_format(input), expected, "{input:?}");
        }
        for separator in ['\n', '\x0c', '\x0b', '\r', '\t'] {
            assert_eq!(
                native_parse_date_format(&format!("2022-02-01{separator}16:33:00")),
                parts(&["2022", "02", "01", "16", "33", "00"])
            );
        }
        for (input, expected) in [
            ("1234:321", false),
            ("2019-04-01", true),
            ("2019-4-1", true),
            ("20129", true),
            ("1234x", true),
            ("2011-11-11x", true),
            ("", false),
        ] {
            assert_eq!(native_is_date_format(input), expected, "{input:?}");
        }
        for byte in 0..=255u8 {
            assert_eq!(
                native_time_is_ascii_punctuation(byte),
                byte.is_ascii_punctuation()
            );
        }
    }
}
