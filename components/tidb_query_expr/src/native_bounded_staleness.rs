// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Bounded-staleness endpoint classification and SafeTS clamping over exact
//! native calendar cores. The caller alone delivers warnings and obtains SafeTS
//! after NeedSafe; neither stage formats or validates calendar/FSP semantics.

use tidb_query_datatype::codec::mysql::{Time, TimeType, time::NativeTemporalValue};

use crate::{NativeIdentityFrameError, NativeIdentityRef, decode_native_identity};

type FrameResult<T> = Result<T, NativeIdentityFrameError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NativeBoundedStalenessHeadResult {
    InvalidLeft = 0,
    InvalidRight = 1,
    RangeNull = 2,
    NeedSafe = 3,
}

pub fn decode_native_bounded_staleness_head(
    bytes: &[u8],
) -> Option<NativeBoundedStalenessHeadResult> {
    use NativeBoundedStalenessHeadResult::*;
    match bytes {
        [0] => Some(InvalidLeft),
        [1] => Some(InvalidRight),
        [2] => Some(RangeNull),
        [3] => Some(NeedSafe),
        _ => None,
    }
}

fn actual_time(value: Option<&[u8]>) -> FrameResult<NativeTemporalValue> {
    let NativeIdentityRef::Time { core, kind, fsp } =
        decode_native_identity(value.ok_or(NativeIdentityFrameError::Invalid)?)?
    else {
        return Err(NativeIdentityFrameError::Invalid);
    };
    let kind = match kind {
        0 => TimeType::Date,
        1 => TimeType::DateTime,
        2 => TimeType::Timestamp,
        _ => return Err(NativeIdentityFrameError::Invalid),
    };
    // Do not use new(): raw DATE FSP and even FSP 255 are valid operand
    // representations. Only the final chosen value receives new metadata.
    Ok(NativeTemporalValue {
        raw: core,
        kind,
        fsp,
    })
}

pub fn bounded_staleness_head_native_args_valid(left: Option<&[u8]>, right: Option<&[u8]>) -> bool {
    actual_time(left).is_ok() && actual_time(right).is_ok()
}

pub(crate) fn evaluate_bounded_staleness_head_native(
    left: Option<&[u8]>,
    right: Option<&[u8]>,
) -> FrameResult<NativeBoundedStalenessHeadResult> {
    let left = actual_time(left)?;
    let right = actual_time(right)?;
    use NativeBoundedStalenessHeadResult::*;
    // Both existing zero flags together are exactly month==0 || day==0:
    // raw==0 already has both fields zero. Year and other fields stay unchecked.
    Ok(if Time::native_date_rejects_zero(left.raw, true, true) {
        InvalidLeft
    } else if Time::native_date_rejects_zero(right.raw, true, true) {
        InvalidRight
    } else if Time::native_core_compare(left.raw, right.raw).is_gt() {
        RangeNull
    } else {
        NeedSafe
    })
}

pub fn bounded_staleness_finish_native_args_valid(
    left: Option<&[u8]>,
    right: Option<&[u8]>,
    safe: Option<&[u8]>,
) -> bool {
    matches!(
        evaluate_bounded_staleness_head_native(left, right),
        Ok(NativeBoundedStalenessHeadResult::NeedSafe)
    ) && safe.is_none_or(|safe| actual_time(Some(safe)).is_ok())
}

/// Also used by reply-size planning; there is no alternate host clamp policy.
pub(crate) fn evaluate_bounded_staleness_finish_native(
    left: Option<&[u8]>,
    right: Option<&[u8]>,
    safe: Option<&[u8]>,
) -> FrameResult<NativeIdentityRef<'static>> {
    if !bounded_staleness_finish_native_args_valid(left, right, safe) {
        return Err(NativeIdentityFrameError::Invalid);
    }
    let left = actual_time(left)?;
    let right = actual_time(right)?;
    let mut selected = match safe.map(|safe| actual_time(Some(safe))).transpose()? {
        Some(safe) if Time::native_core_compare(safe.raw, left.raw).is_lt() => left,
        Some(safe) if Time::native_core_compare(safe.raw, right.raw).is_gt() => right,
        Some(safe) => safe,
        None => left,
    };
    selected.set_kind(TimeType::DateTime);
    selected
        .set_fsp(3)
        .expect("bounded-staleness precision three is valid");
    Ok(NativeIdentityRef::Time {
        core: selected.raw,
        kind: 1,
        fsp: selected.fsp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode_native_identity;

    #[test]
    fn bounded_staleness_keeps_priority_raw_comparison_and_metadata_only_precision() {
        use NativeBoundedStalenessHeadResult::*;
        let frame = |core, kind, fsp| {
            encode_native_identity(NativeIdentityRef::Time { core, kind, fsp }).unwrap()
        };
        let raw = Time::native_core_from_fields(2020, 1, 1, 2, 3, 4, 123_456);
        let later = Time::native_core_from_fields(2022, 1, 1, 2, 3, 4, 654_321);
        let left = frame(raw | 1, 0, 255);
        let right = frame(later | 2, 2, 255);
        let invalid = frame(
            Time::native_core_from_fields(2021, 0, 1, 0, 0, 0, 0),
            1,
            255,
        );
        assert_eq!(
            evaluate_bounded_staleness_head_native(Some(&invalid), Some(&invalid)).unwrap(),
            InvalidLeft
        );
        assert_eq!(
            evaluate_bounded_staleness_head_native(Some(&left), Some(&invalid)).unwrap(),
            InvalidRight
        );
        assert_eq!(
            evaluate_bounded_staleness_head_native(Some(&right), Some(&left)).unwrap(),
            RangeNull
        );
        assert!(!bounded_staleness_finish_native_args_valid(
            Some(&right),
            Some(&left),
            None
        ));
        assert!(!bounded_staleness_finish_native_args_valid(
            Some(&invalid),
            Some(&right),
            None
        ));
        assert_eq!(
            evaluate_bounded_staleness_head_native(Some(&left), Some(&right)).unwrap(),
            NeedSafe
        );
        for (safe_raw, expected) in [
            (raw - 16, raw | 1),
            (later + 16, later | 2),
            (raw | 13, raw | 13),
            (later | 14, later | 14),
            // SafeTS itself is not checked for invalid-zero/calendar values.
            (
                Time::native_core_from_fields(2021, 0, 0, 31, 63, 63, 1_048_575) | 15,
                Time::native_core_from_fields(2021, 0, 0, 31, 63, 63, 1_048_575) | 15,
            ),
        ] {
            let safe = frame(safe_raw, 0, 255);
            assert!(bounded_staleness_finish_native_args_valid(
                Some(&left),
                Some(&right),
                Some(&safe)
            ));
            assert_eq!(
                evaluate_bounded_staleness_finish_native(Some(&left), Some(&right), Some(&safe))
                    .unwrap(),
                NativeIdentityRef::Time {
                    core: expected,
                    kind: 1,
                    fsp: 3
                }
            );
        }
        assert_eq!(
            evaluate_bounded_staleness_finish_native(Some(&left), Some(&right), None).unwrap(),
            NativeIdentityRef::Time {
                core: raw | 1,
                kind: 1,
                fsp: 3
            }
        );
        let year_zero = frame(
            Time::native_core_from_fields(0, 13, 31, 31, 63, 63, 1_048_575),
            1,
            255,
        );
        assert_eq!(
            evaluate_bounded_staleness_head_native(Some(&year_zero), Some(&left)).unwrap(),
            NeedSafe
        );
        assert!(!bounded_staleness_head_native_args_valid(
            None,
            Some(&right)
        ));
        assert!(!bounded_staleness_head_native_args_valid(
            Some(b""),
            Some(&right)
        ));
        assert!(!bounded_staleness_finish_native_args_valid(
            Some(&left),
            Some(&right),
            Some(b"")
        ));
        let non_time = encode_native_identity(NativeIdentityRef::Int(0)).unwrap();
        assert!(!bounded_staleness_head_native_args_valid(
            Some(&non_time),
            Some(&right)
        ));
        for (tag, expected) in [
            (0, InvalidLeft),
            (1, InvalidRight),
            (2, RangeNull),
            (3, NeedSafe),
        ] {
            assert_eq!(decode_native_bounded_staleness_head(&[tag]), Some(expected));
        }
        for invalid in [b"".as_slice(), &[0, 0], &[3, 0], &[4], &[255]] {
            assert!(decode_native_bounded_staleness_head(invalid).is_none());
        }
    }
}
