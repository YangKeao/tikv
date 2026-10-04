// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! IF's shared branch decision and lazy native handoff. Boolean conversion and
//! its caller-specific error policy precede this selector; neither branch value
//! is inspected until the caller supplies the demanded operand to finish.

use crate::{NativeIdentityFrameError, native_identity_args_valid};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeIfBranch {
    Then,
    Else,
}

/// The single IF decision shared by native runtime, wire and pure proof sites.
pub fn native_if_choose_branch(condition: Option<bool>) -> NativeIfBranch {
    match condition {
        Some(true) => NativeIfBranch::Then,
        None | Some(false) => NativeIfBranch::Else,
    }
}

pub fn decode_native_if_head_result(bytes: &[u8]) -> Option<NativeIfBranch> {
    match bytes {
        [0] => Some(NativeIfBranch::Then),
        [1] => Some(NativeIfBranch::Else),
        _ => None,
    }
}

pub fn if_head_native_args_valid(condition: Option<i64>) -> bool {
    matches!(condition, None | Some(0 | 1))
}

pub fn if_finish_native_args_valid(report: Option<&[u8]>, value: Option<&[u8]>) -> bool {
    report.and_then(decode_native_if_head_result).is_some() && native_identity_args_valid(value)
}

pub(crate) fn evaluate_if_head_native(
    condition: Option<i64>,
) -> Result<Vec<u8>, NativeIdentityFrameError> {
    if !if_head_native_args_valid(condition) {
        return Err(NativeIdentityFrameError::Invalid);
    }
    let branch = native_if_choose_branch(condition.map(|value| value != 0));
    let mut report = Vec::new();
    report
        .try_reserve_exact(1)
        .map_err(|_| NativeIdentityFrameError::Capacity)?;
    report.push(match branch {
        NativeIfBranch::Then => 0,
        NativeIfBranch::Else => 1,
    });
    Ok(report)
}

pub(crate) fn evaluate_if_finish_native<'a>(
    report: Option<&[u8]>,
    value: Option<&'a [u8]>,
) -> Result<Option<&'a [u8]>, NativeIdentityFrameError> {
    if !if_finish_native_args_valid(report, value) {
        return Err(NativeIdentityFrameError::Invalid);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NativeIdentityRef as View, encode_native_identity};

    #[test]
    fn if_head_and_finish_keep_normalized_demand_and_opaque_selected_values() {
        for (condition, branch, bytes) in [
            (None, NativeIfBranch::Else, [1]),
            (Some(0), NativeIfBranch::Else, [1]),
            (Some(1), NativeIfBranch::Then, [0]),
        ] {
            assert!(if_head_native_args_valid(condition));
            let report = evaluate_if_head_native(condition).unwrap();
            assert_eq!(report, bytes);
            assert_eq!(decode_native_if_head_result(&report), Some(branch));
            assert_eq!(
                native_if_choose_branch(condition.map(|value| value != 0)),
                branch
            );
            assert_eq!(
                evaluate_if_finish_native(Some(&report), None).unwrap(),
                None
            );
        }
        // Wire delegates normalize their full Int domain, unlike native head
        // admission; no nonzero signed condition becomes an ELSE branch.
        for condition in [i64::MIN, -1, 2, i64::MAX] {
            assert!(!if_head_native_args_valid(Some(condition)));
            assert!(evaluate_if_head_native(Some(condition)).is_err());
            assert_eq!(
                native_if_choose_branch(Some(condition != 0)),
                NativeIfBranch::Then
            );
        }
        for value in [
            View::Decimal {
                negative: true,
                scale: u32::MAX,
                storage_scale: 0,
                declared_shape: Some((i64::MIN, i64::MAX)),
                coefficient: b"\xff\0",
            },
            View::String {
                collation: 15,
                bytes: b"\xff\0",
            },
            View::Real(0x7ff8_0000_0000_0123),
            View::Time {
                core: u64::MAX,
                kind: 2,
                fsp: u8::MAX,
            },
            View::Json {
                type_code: 255,
                bytes: b"invalid\xff",
            },
        ] {
            let identity = encode_native_identity(value).unwrap();
            for report in [[0], [1]] {
                assert!(if_finish_native_args_valid(Some(&report), Some(&identity)));
                assert_eq!(
                    evaluate_if_finish_native(Some(&report), Some(&identity)).unwrap(),
                    Some(identity.as_slice())
                );
            }
        }
        for invalid in [b"".as_slice(), &[0, 0], &[1, 0], &[2], &[255]] {
            assert!(decode_native_if_head_result(invalid).is_none());
            assert!(evaluate_if_finish_native(Some(invalid), None).is_err());
        }
        assert!(!if_finish_native_args_valid(None, None));
        assert!(!if_finish_native_args_valid(Some(&[0]), Some(b"")));
        assert!(evaluate_if_finish_native(Some(&[1]), Some(b"\0")).is_err());
    }
}
