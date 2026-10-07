// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native ProduceStrWithSpecifiedTp, preserving byte storage and the original
//! Go-rune admission policy. This is separate from wire string conversion.
use super::{
    collation::{decode_utf8_rune_strict, utf8_rune_count},
    native_string_type::NativeStringTypeCode,
};

#[derive(Clone, Copy, Debug)]
pub struct NativeStringTargetDiagnostic {
    warning: bool,
    flen: usize,
    data_len: usize,
}
impl NativeStringTargetDiagnostic {
    pub fn is_warning(&self) -> bool {
        self.warning
    }
    /// Formatting stays behind the native Diagnostics warn/truncate closure.
    pub fn message(&self) -> String {
        if self.warning {
            format!(
                "Data truncated, field len {}, data len {}",
                self.flen, self.data_len
            )
        } else {
            format!(
                "Data Too Long, field len {}, data len {}",
                self.flen, self.data_len
            )
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeStringTargetValue {
    pub value: Vec<u8>,
    pub truncated: bool,
}
fn rune_width(bytes: &[u8]) -> usize {
    decode_utf8_rune_strict(bytes).map_or(1, |(_, width)| width)
}
fn utf8_split_at(bytes: &[u8], flen: usize) -> Option<usize> {
    let mut index = 0;
    for _ in 0..flen {
        if index >= bytes.len() {
            return None;
        }
        index += rune_width(&bytes[index..]);
    }
    (index < bytes.len()).then_some(index)
}
// The final accepted rune must be complete and valid; preceding bytes remain
// untouched, even if malformed. Do not validate the entire accepted prefix.
fn complete_utf8_prefix(bytes: &[u8], limit: usize) -> usize {
    let mut end = limit;
    while end > 0 {
        if bytes[end - 1].is_ascii() {
            return end;
        }
        let mut start = end - 1;
        let minimum_start = end.saturating_sub(4);
        while start > minimum_start && bytes[start] & 0xc0 == 0x80 {
            start -= 1;
        }
        let width = rune_width(&bytes[start..end]);
        if width > 1 && start + width == end {
            return end;
        }
        end -= 1;
    }
    end
}
/// diagnostics_enabled controls only the original logical-length counting
/// demand. Report calls remain at the original sites even when diagnostics are
/// disabled, and occur before truncating/padding the returned byte storage.
pub fn native_produce_string(
    mut value: Vec<u8>,
    flen: i64,
    code: NativeStringTypeCode,
    binary: bool,
    pad_zero: bool,
    diagnostics_enabled: bool,
    mut report: impl FnMut(NativeStringTargetDiagnostic),
) -> NativeStringTargetValue {
    if flen < 0 {
        return NativeStringTargetValue {
            value,
            truncated: false,
        };
    }
    let flen = flen as usize;
    let byte_limited = binary || code.is_blob();
    let split = if byte_limited {
        (value.len() > flen).then(|| {
            if binary {
                flen
            } else {
                complete_utf8_prefix(&value, flen)
            }
        })
    } else {
        utf8_split_at(&value, flen)
    };
    let mut truncated = false;
    if let Some(split) = split {
        // Keep the count before whitespace classification, including the quiet
        // fixed-CHAR branch, exactly as the original enabled-context policy.
        let data_len = if byte_limited || !diagnostics_enabled {
            value.len()
        } else {
            utf8_rune_count(&value)
        };
        let overflow = &value[split..];
        let whitespace_only = overflow
            .iter()
            .all(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'));
        if whitespace_only && !binary && code.is_char() {
            if code.is_varchar() {
                report(NativeStringTargetDiagnostic {
                    warning: true,
                    flen,
                    data_len,
                });
                truncated = true;
            }
        } else {
            report(NativeStringTargetDiagnostic {
                warning: false,
                flen,
                data_len,
            });
            truncated = true;
        }
        value.truncate(split);
    }
    if pad_zero && binary && code == NativeStringTypeCode::String && value.len() < flen {
        value.resize(flen, 0);
    }
    NativeStringTargetValue { value, truncated }
}

#[cfg(test)]
mod tests {
    use NativeStringTypeCode as C;

    use super::*;
    #[test]
    fn native_string_target_keeps_named_identity_go_runes_and_blob_terminal_boundaries() {
        const CHAR: bool = C::String.is_char();
        const VARCHAR: bool = C::VarString.is_varchar();
        const BLOB: bool = C::Blob.is_blob();
        assert!(CHAR && VARCHAR && BLOB);
        for code in [
            C::Year,
            C::Unspecified,
            C::VarChar,
            C::TinyBlob,
            C::MediumBlob,
            C::LongBlob,
            C::Blob,
            C::VarString,
            C::String,
        ] {
            assert_eq!(
                code.is_blob(),
                matches!(code, C::TinyBlob | C::MediumBlob | C::LongBlob | C::Blob)
            );
            assert_eq!(code.is_char(), matches!(code, C::String | C::VarChar));
            assert_eq!(code.is_varchar(), matches!(code, C::VarString | C::VarChar));
        }
        for raw in 0..=255 {
            let code = C::Other(raw);
            assert!(!code.is_blob());
            assert!(!code.is_char());
            assert!(!code.is_varchar());
        }
        for (bytes, flen, code, binary, expected, data_len) in [
            (&b"\xc3\xa9X"[..], 1, C::VarChar, false, &b"\xc3\xa9"[..], 2),
            (&b"\xc3\xa9X"[..], 1, C::Blob, false, &b""[..], 3),
            (&b"\xc3\xa9X"[..], 1, C::Blob, true, &b"\xc3"[..], 3),
            (
                &b"\xc3\xa9X"[..],
                1,
                C::Other(252),
                false,
                &b"\xc3\xa9"[..],
                2,
            ),
            (&b"\xc0\xafZ"[..], 1, C::String, false, &b"\xc0"[..], 3),
            (&b"ab\xe9Z"[..], 3, C::Blob, false, &b"ab"[..], 4),
            (&b"ab\xe9Z"[..], 3, C::VarChar, false, &b"ab\xe9"[..], 4),
            (&b"\xffaZ"[..], 2, C::Blob, false, &b"\xffa"[..], 3),
            (
                &b"\xff\xc3\xa9Z"[..],
                3,
                C::Blob,
                false,
                &b"\xff\xc3\xa9"[..],
                4,
            ),
            ("😀Z".as_bytes(), 3, C::Blob, false, &b""[..], 5),
            ("😀Z".as_bytes(), 4, C::Blob, false, "😀".as_bytes(), 5),
        ] {
            let mut calls = Vec::new();
            let value = native_produce_string(
                bytes.to_vec(),
                flen,
                code,
                binary,
                false,
                true,
                |diagnostic| calls.push(diagnostic),
            );
            assert_eq!(
                value,
                NativeStringTargetValue {
                    value: expected.to_vec(),
                    truncated: true
                }
            );
            assert_eq!(calls.len(), 1);
            assert!(!calls[0].is_warning());
            assert_eq!(
                calls[0].message(),
                format!("Data Too Long, field len {flen}, data len {data_len}")
            );
        }
        for enabled in [false, true] {
            let mut calls = Vec::new();
            let value = native_produce_string(
                b"\xc3\xa9\xffZ".to_vec(),
                2,
                C::VarString,
                false,
                false,
                enabled,
                |diagnostic| calls.push(diagnostic),
            );
            assert_eq!(value.value, b"\xc3\xa9\xff");
            assert!(value.truncated);
            // Raw fields prove the report can be received without constructing
            // message text. Enabled contexts count malformed bytes as one rune.
            assert_eq!(calls[0].data_len, if enabled { 3 } else { 4 });
            assert_eq!(
                calls[0].message(),
                format!(
                    "Data Too Long, field len 2, data len {}",
                    if enabled { 3 } else { 4 }
                )
            );
        }
    }
    #[test]
    fn native_string_target_keeps_whitespace_events_padding_and_original_diagnostic_sites() {
        for code in [
            C::Year,
            C::Unspecified,
            C::VarChar,
            C::TinyBlob,
            C::MediumBlob,
            C::LongBlob,
            C::Blob,
            C::VarString,
            C::String,
            C::Other(15),
            C::Other(254),
        ] {
            for binary in [false, true] {
                let mut calls = Vec::new();
                let value = native_produce_string(
                    b"x \t\n\r".to_vec(),
                    1,
                    code,
                    binary,
                    true,
                    false,
                    |diagnostic| calls.push(diagnostic),
                );
                let quiet = code == C::String && !binary;
                assert_eq!(value.value, b"x");
                assert_eq!(value.truncated, !quiet);
                if quiet {
                    assert!(calls.is_empty());
                } else {
                    assert_eq!(calls.len(), 1);
                    let warning = code == C::VarChar && !binary;
                    assert_eq!(calls[0].is_warning(), warning);
                    assert_eq!(
                        calls[0].message(),
                        if warning {
                            "Data truncated, field len 1, data len 5"
                        } else {
                            "Data Too Long, field len 1, data len 5"
                        }
                    );
                }
            }
        }
        for bytes in [&b"x\x0b"[..], &b"x\x0c"[..], "x\u{a0}".as_bytes()] {
            let mut calls = Vec::new();
            let value = native_produce_string(
                bytes.to_vec(),
                1,
                C::String,
                false,
                false,
                true,
                |diagnostic| calls.push(diagnostic),
            );
            assert!(value.truncated);
            assert_eq!(calls.len(), 1);
            assert!(!calls[0].is_warning());
        }
        for code in [C::String, C::VarChar, C::VarString, C::Blob, C::Other(254)] {
            for binary in [false, true] {
                for pad in [false, true] {
                    let value =
                        native_produce_string(b"x".to_vec(), 4, code, binary, pad, true, |_| {
                            panic!("padding is not a diagnostic")
                        });
                    let expected = if code == C::String && binary && pad {
                        &b"x\0\0\0"[..]
                    } else {
                        &b"x"[..]
                    };
                    assert_eq!(value.value, expected);
                    assert!(!value.truncated);
                }
            }
        }
        let mut original = Vec::with_capacity(16);
        original.extend_from_slice(b"\xffa");
        let pointer = original.as_ptr();
        let capacity = original.capacity();
        let value = native_produce_string(original, -2, C::String, true, true, true, |_| {
            panic!("negative length returns first")
        });
        assert_eq!(value.value.as_ptr(), pointer);
        assert_eq!(value.value.capacity(), capacity);
        assert!(!value.truncated);
        let value = native_produce_string(Vec::new(), 0, C::String, true, true, true, |_| {
            panic!("empty value fits")
        });
        assert!(value.value.is_empty());
        assert!(!value.truncated);
        let value = native_produce_string(
            "é".as_bytes().to_vec(),
            1,
            C::VarChar,
            false,
            false,
            true,
            |_| panic!("complete logical value fits"),
        );
        assert_eq!(value.value, "é".as_bytes());
        assert!(!value.truncated);
        let calls = std::cell::RefCell::new(Vec::new());
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            native_produce_string(
                "éXY".as_bytes().to_vec(),
                1,
                C::VarChar,
                false,
                true,
                true,
                |diagnostic| {
                    calls.borrow_mut().push(diagnostic.message());
                    panic!("original diagnostic effect");
                },
            )
        }));
        assert!(panic.is_err());
        assert_eq!(*calls.borrow(), ["Data Too Long, field len 1, data len 3"]);
    }
}
