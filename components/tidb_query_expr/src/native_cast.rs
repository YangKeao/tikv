// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native Real/Float32-to-UNSIGNED policy. This differs deliberately from the
//! wire cast's rounding, clip-to-zero, boundary-warning and NaN behavior.
//! Only presentation/delivery of a computed overflow event remains with
//! callers.

use crate::{NativeIdentityFrameError, NativeIdentityRef, decode_native_identity};

/// Source category in the closed native-cast AST admission policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCastAdmissionSource {
    RangeSentinel,
    Vector,
    Other,
}

/// Target category in the closed native-cast AST admission policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCastAdmissionTarget {
    String,
    Vector,
    Other,
}

/// Decision produced by the closed native-cast AST admission policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCastAdmission {
    Allow,
    RejectRangeSentinel,
    RejectVectorTarget,
}

/// Applies the closed native-cast AST admission policy.
pub const fn native_cast_admission(
    source: NativeCastAdmissionSource,
    target: NativeCastAdmissionTarget,
) -> NativeCastAdmission {
    match source {
        NativeCastAdmissionSource::RangeSentinel => NativeCastAdmission::RejectRangeSentinel,
        NativeCastAdmissionSource::Vector => match target {
            NativeCastAdmissionTarget::String | NativeCastAdmissionTarget::Vector => {
                NativeCastAdmission::Allow
            }
            NativeCastAdmissionTarget::Other => NativeCastAdmission::RejectVectorTarget,
        },
        NativeCastAdmissionSource::Other => NativeCastAdmission::Allow,
    }
}

/// Whether a source-specific UNION DECIMAL cast yields zero or converts its
/// value.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NativeUnionDecimalRoute {
    Zero,
    Convert,
}

/// Chooses the UNION DECIMAL negative-source policy.
pub const fn native_union_decimal_route(is_negative: bool) -> NativeUnionDecimalRoute {
    if is_negative {
        NativeUnionDecimalRoute::Zero
    } else {
        NativeUnionDecimalRoute::Convert
    }
}

/// Chooses the UNION DECIMAL route from trimmed valid UTF-8 text.
pub fn native_union_text_decimal_route(bytes: &[u8]) -> NativeUnionDecimalRoute {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return NativeUnionDecimalRoute::Convert;
    };
    let trimmed = text.trim();
    native_union_decimal_route(trimmed.len() > 1 && trimmed.starts_with('-'))
}

/// Clamps a signed UNION value before unsigned projection.
pub const fn native_union_signed_to_unsigned(value: i64) -> u64 {
    if value < 0 { 0 } else { value as u64 }
}

/// Clamps a real UNION value before signed integer projection.
pub fn native_union_real_to_signed(value: f64) -> i64 {
    if value < 0.0 { 0 } else { value as i64 }
}

/// Clamps a real UNION value before real projection.
pub fn native_union_real(value: f64) -> f64 {
    if value < 0.0 { 0.0 } else { value }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeCastRealUnsignedResult {
    pub value: u64,
    pub overflow_bits: Option<u64>,
}

fn actual_real_bits(value: Option<&[u8]>) -> Result<u64, NativeIdentityFrameError> {
    match decode_native_identity(value.ok_or(NativeIdentityFrameError::Invalid)?)? {
        NativeIdentityRef::Real(bits) | NativeIdentityRef::Float32(bits) => Ok(bits),
        _ => Err(NativeIdentityFrameError::Invalid),
    }
}

pub fn cast_real_unsigned_native_args_valid(value: Option<&[u8]>) -> bool {
    actual_real_bits(value).is_ok()
}

/// The single computation used by execution and exact reply-length planning.
/// Float32 carries its original f64 bits; there is no narrowing or finite gate.
pub(crate) fn evaluate_cast_real_unsigned_native(
    value: Option<&[u8]>,
) -> Result<NativeCastRealUnsignedResult, NativeIdentityFrameError> {
    let rounded = f64::from_bits(actual_real_bits(value)?).round_ties_even();
    Ok(if rounded < 0.0 {
        NativeCastRealUnsignedResult {
            value: (rounded as i64) as u64,
            overflow_bits: Some(rounded.to_bits()),
        }
    } else if !rounded.is_finite() || rounded >= (u64::MAX as f64) {
        NativeCastRealUnsignedResult {
            value: u64::MAX,
            overflow_bits: Some(rounded.to_bits()),
        }
    } else {
        NativeCastRealUnsignedResult {
            value: rounded as u64,
            overflow_bits: None,
        }
    })
}

pub(crate) fn encode_native_cast_real_unsigned_result(
    result: NativeCastRealUnsignedResult,
) -> Result<Vec<u8>, NativeIdentityFrameError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(if result.overflow_bits.is_some() {
            17
        } else {
            9
        })
        .map_err(|_| NativeIdentityFrameError::Capacity)?;
    bytes.push(u8::from(result.overflow_bits.is_some()));
    bytes.extend_from_slice(&result.value.to_le_bytes());
    if let Some(bits) = result.overflow_bits {
        bytes.extend_from_slice(&bits.to_le_bytes());
    }
    Ok(bytes)
}

/// Structural decoding only: event bits are the actual rounded float, including
/// nonfinite values, and are not reclassified or numerically normalized here.
pub fn decode_native_cast_real_unsigned_result(
    bytes: &[u8],
) -> Option<NativeCastRealUnsignedResult> {
    let overflow_bits = match (bytes.first()?, bytes.len()) {
        (0, 9) => None,
        (1, 17) => Some(u64::from_le_bytes(bytes[9..17].try_into().ok()?)),
        _ => return None,
    };
    Some(NativeCastRealUnsignedResult {
        value: u64::from_le_bytes(bytes[1..9].try_into().ok()?),
        overflow_bits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode_native_identity;

    #[test]
    fn native_real_unsigned_keeps_even_ties_wrap_events_and_canonical_reports() {
        let upper = u64::MAX as f64;
        for (input, value, overflow) in [
            (0.0, 0, None),
            (-0.0, 0, None),
            (0.5, 0, None),
            (-0.5, 0, None),
            (1.5, 2, None),
            (2.5, 2, None),
            (0.50000001, 1, None),
            (-1.5, u64::MAX - 1, Some(-2.0_f64)),
            (-2.5, u64::MAX - 1, Some(-2.0)),
            (-1e300, 1_u64 << 63, Some(-1e300)),
            ((1_u64 << 63) as f64, 1_u64 << 63, None),
            (f64::from_bits(upper.to_bits() - 1), u64::MAX - 2047, None),
            (upper, u64::MAX, Some(upper)),
            (f64::INFINITY, u64::MAX, Some(f64::INFINITY)),
            (f64::NEG_INFINITY, 1_u64 << 63, Some(f64::NEG_INFINITY)),
        ] {
            for original in [
                NativeIdentityRef::Real(input.to_bits()),
                NativeIdentityRef::Float32(input.to_bits()),
            ] {
                let input = encode_native_identity(original).unwrap();
                assert!(cast_real_unsigned_native_args_valid(Some(&input)));
                let result = evaluate_cast_real_unsigned_native(Some(&input)).unwrap();
                assert_eq!(
                    result,
                    NativeCastRealUnsignedResult {
                        value,
                        overflow_bits: overflow.map(f64::to_bits)
                    }
                );
                let encoded = encode_native_cast_real_unsigned_result(result).unwrap();
                assert_eq!(encoded.len(), if overflow.is_some() { 17 } else { 9 });
                assert_eq!(
                    decode_native_cast_real_unsigned_result(&encoded),
                    Some(result)
                );
            }
        }
        for bits in [
            0x7ff8_0000_0000_0123,
            0xfff8_0000_0000_0456,
            0x7ff0_0000_0000_0001,
        ] {
            let input = encode_native_identity(NativeIdentityRef::Real(bits)).unwrap();
            let result = evaluate_cast_real_unsigned_native(Some(&input)).unwrap();
            assert_eq!(result.value, u64::MAX);
            assert_eq!(
                result.overflow_bits,
                Some(f64::from_bits(bits).round_ties_even().to_bits())
            );
            assert!(f64::from_bits(result.overflow_bits.unwrap()).is_nan());
            assert_eq!(
                decode_native_cast_real_unsigned_result(
                    &encode_native_cast_real_unsigned_result(result).unwrap()
                ),
                Some(result)
            );
        }
        assert!(!cast_real_unsigned_native_args_valid(None));
        assert!(evaluate_cast_real_unsigned_native(None).is_err());
        for invalid in [b"".as_slice(), &[6], &[7, 0], &[255]] {
            assert!(!cast_real_unsigned_native_args_valid(Some(invalid)));
            assert!(evaluate_cast_real_unsigned_native(Some(invalid)).is_err());
        }
        for value in [
            NativeIdentityRef::Int(1),
            NativeIdentityRef::UInt(1),
            NativeIdentityRef::Bytes(b"1.5"),
        ] {
            let input = encode_native_identity(value).unwrap();
            assert!(!cast_real_unsigned_native_args_valid(Some(&input)));
            assert!(evaluate_cast_real_unsigned_native(Some(&input)).is_err());
        }
        let mut canonical = encode_native_cast_real_unsigned_result(NativeCastRealUnsignedResult {
            value: 7,
            overflow_bits: Some(0),
        })
        .unwrap();
        // Decoder checks the frame, not whether event bits would overflow.
        assert_eq!(
            decode_native_cast_real_unsigned_result(&canonical)
                .unwrap()
                .overflow_bits,
            Some(0)
        );
        for length in 0..17 {
            assert!(decode_native_cast_real_unsigned_result(&canonical[..length]).is_none());
        }
        canonical.push(0);
        assert!(decode_native_cast_real_unsigned_result(&canonical).is_none());
        canonical.truncate(9);
        canonical[0] = 2;
        assert!(decode_native_cast_real_unsigned_result(&canonical).is_none());
        canonical[0] = 0;
        assert_eq!(
            decode_native_cast_real_unsigned_result(&canonical),
            Some(NativeCastRealUnsignedResult {
                value: 7,
                overflow_bits: None
            })
        );
        canonical.push(0);
        assert!(decode_native_cast_real_unsigned_result(&canonical).is_none());
    }
}

#[cfg(test)]
#[test]
fn union_cast_policy_preserves_seven_source_specific_clamps_and_routes() {
    use NativeUnionDecimalRoute::*;
    assert_eq!(native_union_decimal_route(true), Zero);
    assert_eq!(native_union_decimal_route(false), Convert);
    for text in [b"-1".as_slice(), b"  -1.5  ", b"-x"] {
        assert_eq!(native_union_text_decimal_route(text), Zero);
    }
    for text in [&b"-"[..], &b"+1"[..], &b"1"[..], &b"  -  "[..], &[0xff]] {
        assert_eq!(native_union_text_decimal_route(text), Convert);
    }
    assert_eq!(native_union_signed_to_unsigned(-1), 0);
    assert_eq!(native_union_signed_to_unsigned(7), 7);
    assert_eq!(native_union_real_to_signed(-0.5), 0);
    assert_eq!(native_union_real_to_signed(2.9), 2);
    assert_eq!(native_union_real(-0.5).to_bits(), 0.0_f64.to_bits());
    assert_eq!(native_union_real(2.5), 2.5);
    assert!(native_union_real(f64::NAN).is_nan());
}

#[cfg(test)]
#[test]
fn cast_admission_matrix_rejects_sentinels_and_non_string_vector_targets() {
    use NativeCastAdmission::{Allow, RejectRangeSentinel, RejectVectorTarget};
    use NativeCastAdmissionSource::{Other, RangeSentinel, Vector};
    use NativeCastAdmissionTarget::{Other as OtherTarget, String, Vector as VectorTarget};
    for target in [String, VectorTarget, OtherTarget] {
        assert_eq!(
            native_cast_admission(RangeSentinel, target),
            RejectRangeSentinel
        );
    }
    assert_eq!(native_cast_admission(Vector, String), Allow);
    assert_eq!(native_cast_admission(Vector, VectorTarget), Allow);
    assert_eq!(
        native_cast_admission(Vector, OtherTarget),
        RejectVectorTarget
    );
    assert_eq!(native_cast_admission(Other, OtherTarget), Allow);
}
