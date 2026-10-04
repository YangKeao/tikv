// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! IFNULL's lazy, lossless selection handoff. SQL NULL remains physical
//! absence; report tags describe the SDK's choice, never a caller-selected
//! operation.

use crate::{NativeIdentityFrameError, native_identity_args_valid};

/// One nullable decision shared by runtime, wire and pure expression proofs.
/// The actual first value is returned without conversion or cloning, and no
/// right-hand operand is retained or requested by this pure selector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeIfNullChoice<T> {
    NeedSecond,
    Done(T),
}

pub fn native_if_null_choose_first<T>(actual_first: Option<T>) -> NativeIfNullChoice<T> {
    match actual_first {
        None => NativeIfNullChoice::NeedSecond,
        Some(value) => NativeIfNullChoice::Done(value),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeIfNullHeadResult<'a> {
    NeedSecond,
    Done(&'a [u8]),
}

pub fn decode_native_if_null_head_result(bytes: &[u8]) -> Option<NativeIfNullHeadResult<'_>> {
    match bytes.split_first()? {
        (0, []) => Some(NativeIfNullHeadResult::NeedSecond),
        (1, value) if native_identity_args_valid(Some(value)) => {
            Some(NativeIfNullHeadResult::Done(value))
        }
        _ => None,
    }
}

pub fn if_null_head_native_args_valid(value: Option<&[u8]>) -> bool {
    native_identity_args_valid(value)
}

pub fn if_null_finish_native_args_valid(report: Option<&[u8]>, value: Option<&[u8]>) -> bool {
    report == Some(&[0][..]) && native_identity_args_valid(value)
}

pub(crate) fn evaluate_if_null_head_native(
    value: Option<&[u8]>,
) -> Result<Vec<u8>, NativeIdentityFrameError> {
    if !if_null_head_native_args_valid(value) {
        return Err(NativeIdentityFrameError::Invalid);
    }
    let length = 1usize
        .checked_add(value.map_or(0, <[u8]>::len))
        .ok_or(NativeIdentityFrameError::Capacity)?;
    let mut report = Vec::new();
    report
        .try_reserve_exact(length)
        .map_err(|_| NativeIdentityFrameError::Capacity)?;
    match native_if_null_choose_first(value) {
        NativeIfNullChoice::NeedSecond => report.push(0),
        NativeIfNullChoice::Done(value) => {
            report.push(1);
            report.extend_from_slice(value);
        }
    }
    Ok(report)
}

/// Validate the original demand witness and borrow the actual second operand.
/// The RPN wrapper uses the existing nullable byte-selection worker to own it.
pub(crate) fn evaluate_if_null_finish_native<'a>(
    report: Option<&[u8]>,
    value: Option<&'a [u8]>,
) -> Result<Option<&'a [u8]>, NativeIdentityFrameError> {
    if !if_null_finish_native_args_valid(report, value) {
        return Err(NativeIdentityFrameError::Invalid);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NativeIdentityRef as View, encode_native_identity};

    #[test]
    fn if_null_handoff_preserves_null_demand_and_every_opaque_representation() {
        struct NonClone(u8);
        match native_if_null_choose_first(Some(NonClone(7))) {
            NativeIfNullChoice::Done(value) => assert_eq!(value.0, 7),
            NativeIfNullChoice::NeedSecond => panic!("a present first value must win"),
        }
        assert_eq!(
            native_if_null_choose_first::<()>(None),
            NativeIfNullChoice::NeedSecond
        );
        assert_eq!(
            native_if_null_choose_first(Some(None::<u8>)),
            NativeIfNullChoice::Done(None)
        );
        let demand = evaluate_if_null_head_native(None).unwrap();
        assert_eq!(demand, [0]);
        assert_eq!(
            decode_native_if_null_head_result(&demand),
            Some(NativeIfNullHeadResult::NeedSecond)
        );
        assert_eq!(
            evaluate_if_null_finish_native(Some(&demand), None).unwrap(),
            None
        );
        for value in [
            View::MinNotNull,
            View::MaxValue,
            View::Int(0),
            View::UInt(u64::MAX),
            View::Decimal {
                negative: true,
                scale: u32::MAX,
                storage_scale: 0,
                declared_shape: Some((i64::MIN, i64::MAX)),
                coefficient: b"\xff\0",
            },
            View::Real(0x7ff8_0000_0000_0123),
            View::Float32(1.00000051_f64.to_bits()),
            View::String {
                collation: 15,
                bytes: b"\xff\0",
            },
            View::Bytes(b""),
            View::BinaryLiteral(b"\0\xff"),
            View::Duration {
                nanos: i64::MIN,
                fsp: i64::MAX,
            },
            View::Enum {
                collation: 2,
                value: u64::MAX,
                name: b"\xff",
            },
            View::Bit(b"\0\xff"),
            View::Set {
                collation: 3,
                value: 0,
                name: b"\xff\0",
            },
            View::Time {
                core: u64::MAX,
                kind: 2,
                fsp: u8::MAX,
            },
            View::Json {
                type_code: 255,
                bytes: b"invalid\xff",
            },
            View::Raw(b"\xff\0"),
            View::Vector(b"\x34\x12\xc0\x7f"),
        ] {
            let identity = encode_native_identity(value).unwrap();
            assert!(if_null_head_native_args_valid(Some(&identity)));
            let report = evaluate_if_null_head_native(Some(&identity)).unwrap();
            assert_eq!(&report[1..], &identity);
            assert_eq!(
                decode_native_if_null_head_result(&report),
                Some(NativeIfNullHeadResult::Done(identity.as_slice()))
            );
            assert_eq!(
                evaluate_if_null_finish_native(Some(&demand), Some(&identity)).unwrap(),
                Some(identity.as_slice())
            );
            assert!(!if_null_finish_native_args_valid(
                Some(&report),
                Some(&identity)
            ));
            assert!(evaluate_if_null_finish_native(Some(&report), None).is_err());
        }
        for invalid in [b"".as_slice(), &[0, 0], &[1], &[1, 0], &[2], &[1, 3, 0]] {
            assert!(decode_native_if_null_head_result(invalid).is_none());
        }
        assert!(if_null_head_native_args_valid(None));
        assert!(!if_null_head_native_args_valid(Some(b"")));
        assert!(evaluate_if_null_head_native(Some(b"\0")).is_err());
        assert!(!if_null_finish_native_args_valid(None, None));
        assert!(!if_null_finish_native_args_valid(Some(&demand), Some(b"")));
        assert!(evaluate_if_null_finish_native(Some(&[0, 0]), None).is_err());
    }
}
