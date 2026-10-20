// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Actual byte-unary results with finite demanded work, without transport.
/// Closed original byte-unary SQL policies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeByteUnaryOperation {
    /// First byte as an integer; empty bytes yield zero.
    Ascii,
    /// Remove leading U+0020 bytes.
    LTrim,
    /// Remove trailing U+0020 bytes.
    RTrim,
}
impl NativeByteUnaryOperation {
    /// Original lower-case SQL function name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ascii => "ascii",
            Self::LTrim => "ltrim",
            Self::RTrim => "rtrim",
        }
    }
}
/// Work exhaustion is distinct from an impossible pure-kernel cause.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeByteUnaryError {
    /// Finite actual work allowance exhausted.
    Resource,
    /// Original infallible ASCII owner returned an error.
    Contract,
}
fn check(n: usize, maximum: usize) -> Result<(), NativeByteUnaryError> {
    if maximum == usize::MAX || n > maximum {
        Err(NativeByteUnaryError::Resource)
    } else {
        Ok(())
    }
}
/// Nullable original ASCII over borrowed actual bytes; inspect no unused
/// suffix.
pub fn native_ascii_actual_value(
    input: Option<&[u8]>,
    maximum: usize,
) -> Result<Option<i64>, NativeByteUnaryError> {
    check(usize::from(input.is_some()), maximum)?;
    input
        .map(|v| crate::impl_string::ascii(v).map_err(|_| NativeByteUnaryError::Contract))
        .transpose()
        .map(Option::flatten)
}
/// Borrow only actual bytes; callers account for demanded output ownership.
pub fn native_trim_actual_value(
    input: Option<&[u8]>,
    left: bool,
    maximum: usize,
) -> Result<Option<&[u8]>, NativeByteUnaryError> {
    check(input.map_or(0, <[u8]>::len), maximum)?;
    Ok(input.map(|arg| crate::impl_string::trim_space_bytes(arg, left)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_byte_unary_values_preserve_raw_bytes_space_only_borrowing_and_finite_work() {
        assert_eq!(native_ascii_actual_value(None, 0), Ok(None));
        assert_eq!(native_ascii_actual_value(Some(b""), 1), Ok(Some(0)));
        assert_eq!(native_ascii_actual_value(Some(&[255, 0]), 1), Ok(Some(255)));
        assert_eq!(
            native_ascii_actual_value(Some("中".as_bytes()), 1),
            Ok(Some(228))
        );
        let huge = vec![b'X'; 1 << 20];
        assert_eq!(native_ascii_actual_value(Some(&huge), 1), Ok(Some(88)));
        assert_eq!(
            native_ascii_actual_value(Some(&huge), 0),
            Err(NativeByteUnaryError::Resource)
        );
        assert_eq!(
            native_ascii_actual_value(None, usize::MAX),
            Err(NativeByteUnaryError::Resource)
        );
        for left in [true, false] {
            assert_eq!(native_trim_actual_value(None, left, 0), Ok(None));
            assert_eq!(
                native_trim_actual_value(Some(b""), left, 0),
                Ok(Some(b"".as_slice()))
            );
            assert_eq!(
                native_trim_actual_value(Some(b"   "), left, 3),
                Ok(Some(b"".as_slice()))
            );
            assert_eq!(
                native_trim_actual_value(Some(b"   "), left, 2),
                Err(NativeByteUnaryError::Resource)
            );
            assert_eq!(
                native_trim_actual_value(Some(b"\t\n"), left, 2),
                Ok(Some(b"\t\n".as_slice()))
            );
        }
        let raw = b"  \xff\x00 ";
        let left = native_trim_actual_value(Some(raw), true, raw.len())
            .unwrap()
            .unwrap();
        let right = native_trim_actual_value(Some(raw), false, raw.len())
            .unwrap()
            .unwrap();
        assert_eq!(left, b"\xff\x00 ");
        assert_eq!(right, b"  \xff\x00");
        assert_eq!(left.as_ptr(), raw[2..].as_ptr());
        assert_eq!(right.as_ptr(), raw.as_ptr());
        assert_eq!(
            native_trim_actual_value(Some("\u{a0}x\u{2003}".as_bytes()), true, 128),
            Ok(Some("\u{a0}x\u{2003}".as_bytes()))
        );
    }
}
