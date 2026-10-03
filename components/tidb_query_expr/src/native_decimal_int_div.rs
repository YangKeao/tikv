// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native Decimal DIV phase demand and computation over the shared decimal
//! core. Inputs retain actual raw representations and separately demanded
//! precision reads. Reports contain only computed warnings/outcomes, never host
//! answers. Limits bound each materialized buffer and the decimal core's
//! documented scratch/output domain, not a combined physical allocator peak.

use std::fmt::{self, Write};

use tidb_query_common::{Result, error::EvaluateError};
use tidb_query_datatype::codec::mysql::{
    Decimal,
    decimal::{NativeDecimalError, NativeDecimalOp, Res},
};

use crate::{NativeIdentityRef, decode_native_identity, impl_math::native_decimal_failure};

#[derive(Clone, Copy)]
struct RawDecimal<'a> {
    negative: bool,
    scale: u32,
    storage_scale: u32,
    coefficient: &'a [u8],
}

fn raw_decimal(value: NativeIdentityRef<'_>) -> Option<RawDecimal<'_>> {
    match value {
        NativeIdentityRef::Decimal {
            negative,
            scale,
            storage_scale,
            coefficient,
            ..
        } => Some(RawDecimal {
            negative,
            scale,
            storage_scale,
            coefficient,
        }),
        _ => None,
    }
}

fn raw_zero(value: RawDecimal<'_>) -> bool {
    // DecimalDigits::Deref/as_str precedes the original is_zero byte iterator.
    // Empty and all-zero strings are zero; do not normalize or inspect lhs.
    std::str::from_utf8(value.coefficient)
        .expect("decimal coefficients are ASCII digits")
        .bytes()
        .all(|byte| byte == b'0')
}

fn projection(value: RawDecimal<'_>) -> Option<(i128, u32)> {
    Decimal::native_raw_coefficient_i128(value.negative, value.coefficient, value.storage_scale)
}

fn increment(value: i64) -> Option<u32> {
    u32::try_from(value)
        .ok()
        .map(|value| if value == 0 { 4 } else { value })
}

fn probe_gate(left: RawDecimal<'_>, right: RawDecimal<'_>) -> bool {
    left.storage_scale <= 3 && left.storage_scale == right.storage_scale
}

/// Determines whether the original first precision getter is demanded, without
/// allocating a frame or computing a quotient. The RHS zero test comes first.
pub fn native_intdiv_needs_probe(
    left: NativeIdentityRef<'_>,
    right: NativeIdentityRef<'_>,
) -> Option<bool> {
    let (left, right) = (raw_decimal(left)?, raw_decimal(right)?);
    if raw_zero(right) {
        return Some(false);
    }
    Some(probe_gate(left, right))
}

enum Phase {
    Zero,
    Fast(i128, i128),
    Fallback,
}

fn phase(left: RawDecimal<'_>, right: RawDecimal<'_>, probe: Option<i64>) -> Option<Phase> {
    if raw_zero(right) {
        return probe.is_none().then_some(Phase::Zero);
    }
    if !probe_gate(left, right) {
        return probe.is_none().then_some(Phase::Fallback);
    }
    let increment = increment(probe?)?;
    if increment > 30 {
        return Some(Phase::Fallback);
    }
    // Tuple evaluation is deliberately eager: a failed lhs projection does not
    // suppress the original rhs projection (including its raw UTF-8 panic).
    let pair = (projection(left), projection(right));
    Some(match pair {
        (Some((left, _)), Some((right, _))) => Phase::Fast(left, right),
        _ => Phase::Fallback,
    })
}

/// Determines the independent fallback getter's demand, preserving the actual
/// first getter's value and both eager raw coefficient projections.
pub fn native_intdiv_needs_fallback(
    left: NativeIdentityRef<'_>,
    right: NativeIdentityRef<'_>,
    probe: Option<i64>,
) -> Option<bool> {
    Some(matches!(
        phase(raw_decimal(left)?, raw_decimal(right)?, probe)?,
        Phase::Fallback
    ))
}

fn framed_decimal(frame: Option<&[u8]>) -> Option<RawDecimal<'_>> {
    raw_decimal(decode_native_identity(frame?).ok()?)
}

/// Only genuine Decimal frames and the exact demanded u32 precision reads are
/// admitted. No fallback read is fabricated from the first read's value.
pub fn native_intdiv_args_valid(
    left: Option<&[u8]>,
    probe: Option<i64>,
    fallback: Option<i64>,
    right: Option<&[u8]>,
) -> bool {
    let (Some(left), Some(right)) = (
        left.and_then(|bytes| decode_native_identity(bytes).ok()),
        right.and_then(|bytes| decode_native_identity(bytes).ok()),
    ) else {
        return false;
    };
    let Some(needs_probe) = native_intdiv_needs_probe(left, right) else {
        return false;
    };
    if needs_probe != probe.is_some() {
        return false;
    }
    match native_intdiv_needs_fallback(left, right, probe) {
        Some(true) => fallback.and_then(increment).is_some(),
        Some(false) => fallback.is_none(),
        None => false,
    }
}

/// Legacy exact DIV has no precision metadata and retains opaque coefficient
/// bytes through admission; the actual worker owns zero and arithmetic policy.
pub fn native_intdiv_legacy_args_valid(left: Option<&[u8]>, right: Option<&[u8]>) -> bool {
    framed_decimal(left).is_some() && framed_decimal(right).is_some()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeIntDivOutcome {
    ZeroDivisor,
    Value(i64),
    IntOverflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeIntDivReport<'a> {
    pub warning: Option<&'a str>,
    pub outcome: NativeIntDivOutcome,
}

/// Packet: outcome byte, warning-presence byte, i64 LE bits only for Value,
/// then u64 LE byte length and UTF-8 text only when a warning is present.
/// ZeroDivisor cannot have a truncation warning; all extents consume exactly.
pub fn decode_native_intdiv_report(bytes: &[u8]) -> Option<NativeIntDivReport<'_>> {
    let (&kind, bytes) = bytes.split_first()?;
    let (&warning, mut bytes) = bytes.split_first()?;
    let outcome = match kind {
        0 => NativeIntDivOutcome::ZeroDivisor,
        1 => {
            let (bits, tail) = bytes.split_at_checked(8)?;
            bytes = tail;
            NativeIntDivOutcome::Value(i64::from_le_bytes(bits.try_into().ok()?))
        }
        2 => NativeIntDivOutcome::IntOverflow,
        _ => return None,
    };
    let warning = match warning {
        0 if bytes.is_empty() => None,
        1 if kind != 0 => {
            let (length, text) = bytes.split_at_checked(8)?;
            let length = usize::try_from(u64::from_le_bytes(length.try_into().ok()?)).ok()?;
            if text.len() != length || length == 0 {
                return None;
            }
            Some(std::str::from_utf8(text).ok()?)
        }
        _ => return None,
    };
    Some(NativeIntDivReport { warning, outcome })
}

pub fn native_intdiv_result_valid(bytes: &[u8]) -> bool {
    decode_native_intdiv_report(bytes).is_some()
}

fn invalid(message: &'static str) -> tidb_query_common::Error {
    native_decimal_failure(NativeDecimalError::InvalidInput(message))
}

fn resource(message: &'static str) -> tidb_query_common::Error {
    native_decimal_failure(NativeDecimalError::Resource(message))
}

fn actual_error(error: impl std::error::Error + Send + Sync + 'static) -> tidb_query_common::Error {
    EvaluateError::Caused(Box::new(error)).into()
}

fn finite_budget(budget: usize) -> Result<()> {
    if budget == usize::MAX {
        return Err(resource("native Decimal DIV requires a finite budget"));
    }
    Ok(())
}

fn shared(value: RawDecimal<'_>, budget: usize) -> Result<Decimal> {
    // The old native bridge reaches DecimalDigits::as_bytes through its str
    // dereference before the shared digit constructor. Keep that panic/order.
    std::str::from_utf8(value.coefficient).expect("decimal coefficients are ASCII digits");
    Decimal::try_from_native_digits(
        value.negative,
        value.coefficient,
        value.storage_scale,
        value.scale,
        budget,
    )
    .map_err(native_decimal_failure)
}

struct WarningWriter {
    text: String,
    budget: usize,
    error: Option<tidb_query_common::Error>,
}

impl fmt::Write for WarningWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self
            .text
            .len()
            .checked_add(text.len())
            .is_none_or(|length| length > self.budget)
        {
            self.error = Some(resource("native Decimal DIV warning exceeds budget"));
            return Err(fmt::Error);
        }
        if let Err(error) = self.text.try_reserve_exact(text.len()) {
            self.error = Some(actual_error(error));
            return Err(fmt::Error);
        }
        self.text.push_str(text);
        Ok(())
    }
}

fn warning_text(quotient: &Decimal, budget: usize) -> Result<String> {
    let mut writer = WarningWriter {
        text: String::new(),
        budget,
        error: None,
    };
    if let Err(error) = write!(
        &mut writer,
        "Truncated incorrect DECIMAL value: '{quotient}'"
    ) {
        return Err(writer.error.unwrap_or_else(|| actual_error(error)));
    }
    Ok(writer.text)
}

fn report(outcome: NativeIntDivOutcome, warning: Option<&str>, budget: usize) -> Result<Vec<u8>> {
    let base: usize = 2 + if matches!(outcome, NativeIntDivOutcome::Value(_)) {
        8
    } else {
        0
    };
    let length = warning
        .map_or(Some(base), |text| {
            base.checked_add(8)?.checked_add(text.len())
        })
        .ok_or_else(|| resource("native Decimal DIV report extent overflow"))?;
    if length > budget {
        return Err(resource("native Decimal DIV report exceeds budget"));
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).map_err(actual_error)?;
    bytes.push(match outcome {
        NativeIntDivOutcome::ZeroDivisor => 0,
        NativeIntDivOutcome::Value(_) => 1,
        NativeIntDivOutcome::IntOverflow => 2,
    });
    bytes.push(u8::from(warning.is_some()));
    if let NativeIntDivOutcome::Value(bits) = outcome {
        bytes.extend_from_slice(&bits.to_le_bytes());
    }
    if let Some(warning) = warning {
        let length = u64::try_from(warning.len()).map_err(actual_error)?;
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(warning.as_bytes());
    }
    Ok(bytes)
}

pub(crate) fn evaluate_native_intdiv(
    left: Option<&[u8]>,
    probe: Option<i64>,
    fallback: Option<i64>,
    right: Option<&[u8]>,
    unsigned: bool,
    budget: usize,
) -> Result<Vec<u8>> {
    finite_budget(budget)?;
    let (Some(left), Some(right)) = (framed_decimal(left), framed_decimal(right)) else {
        return Err(invalid(
            "native Decimal DIV requires two actual Decimal frames",
        ));
    };
    let phase = phase(left, right, probe)
        .ok_or_else(|| invalid("native Decimal DIV probe presence or range mismatch"))?;
    match phase {
        Phase::Zero | Phase::Fast(..) if fallback.is_some() => {
            return Err(invalid(
                "native Decimal DIV contains an undemanded fallback read",
            ));
        }
        _ => {}
    }
    match phase {
        Phase::Zero => report(NativeIntDivOutcome::ZeroDivisor, None, budget),
        Phase::Fast(left, right) => {
            let quotient = left.checked_div(right);
            let bits = if unsigned {
                quotient
                    .and_then(|value| u64::try_from(value).ok())
                    .map(|value| value as i64)
            } else {
                quotient.and_then(|value| i64::try_from(value).ok())
            };
            report(
                bits.map_or(NativeIntDivOutcome::IntOverflow, NativeIntDivOutcome::Value),
                None,
                budget,
            )
        }
        Phase::Fallback => {
            let increment = fallback.and_then(increment).ok_or_else(|| {
                invalid("native Decimal DIV requires its actual fallback precision")
            })?;
            let left = shared(left, budget)?;
            let right = shared(right, budget)?;
            // Unlike the separate '/' profile, DIV passes the effective raw
            // increment directly: no extra scale + increment expression here.
            let divided = left
                .try_native_mysql_div(&right, increment, budget)
                .map_err(native_decimal_failure)?
                .ok_or_else(|| invalid("nonzero raw Decimal DIV divisor projected to zero"))?;
            let (mut quotient, truncated, warned) = match divided {
                Res::Ok(value) => (value, false, false),
                Res::Truncated(value) => (value, true, true),
                Res::Overflow(value) => (value, false, true),
            };
            if truncated {
                let (precision, fraction) = quotient.natural_storage_shape();
                let integer_words = (precision - fraction as usize).div_ceil(9);
                let scale = (9usize.saturating_sub(integer_words) * 9)
                    .min(quotient.result_scale() as usize) as i32;
                quotient = quotient
                    .try_native_math(NativeDecimalOp::Truncate(scale), budget)
                    .map_err(native_decimal_failure)?;
            }
            let warning = if warned {
                Some(warning_text(&quotient, budget)?)
            } else {
                None
            };
            let outcome = if unsigned {
                match quotient.as_u64() {
                    Res::Ok(value) | Res::Truncated(value) => {
                        NativeIntDivOutcome::Value(value as i64)
                    }
                    Res::Overflow(_) if matches!(quotient.as_i64(), Res::Truncated(0)) => {
                        NativeIntDivOutcome::Value(0)
                    }
                    Res::Overflow(_) => NativeIntDivOutcome::IntOverflow,
                }
            } else {
                match quotient.as_i64() {
                    Res::Ok(value) | Res::Truncated(value) => NativeIntDivOutcome::Value(value),
                    Res::Overflow(_) => NativeIntDivOutcome::IntOverflow,
                }
            };
            report(outcome, warning.as_deref(), budget)
        }
    }
}

// Empty raw lhs with a nonzero rhs is outside shared math's coefficient
// admission (an infrastructure error, unlike the old legacy empty-digit zero).
// This documented representation gap does not revive a second division core.
pub(crate) fn evaluate_legacy_decimal_intdiv(
    left: &[u8],
    right: &[u8],
    budget: usize,
) -> Result<Option<i64>> {
    finite_budget(budget)?;
    let (Some(left), Some(right)) = (framed_decimal(Some(left)), framed_decimal(Some(right)))
    else {
        return Err(invalid(
            "legacy Decimal DIV requires two actual Decimal frames",
        ));
    };
    if raw_zero(right) {
        return Ok(None);
    }
    let left = shared(left, budget)?;
    let right = shared(right, budget)?;
    let quotient = left
        .try_native_integer_quotient(&right, budget)
        .map_err(native_decimal_failure)?;
    Ok(quotient.and_then(|value| match value.as_i64() {
        Res::Ok(value) | Res::Truncated(value) => Some(value),
        Res::Overflow(_) => None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode_native_identity;

    fn decimal(
        digits: &[u8],
        negative: bool,
        scale: u32,
        storage_scale: u32,
    ) -> NativeIdentityRef<'_> {
        NativeIdentityRef::Decimal {
            negative,
            scale,
            storage_scale,
            declared_shape: Some((-1, 7)),
            coefficient: digits,
        }
    }

    #[test]
    fn native_intdiv_phase_demands_raw_zero_fast_bits_and_transport_bounds() {
        let one = decimal(b"1", false, 0, 0);
        let invalid_left = decimal(&[255], true, u32::MAX, u32::MAX);
        let invalid_frame = encode_native_identity(invalid_left).unwrap();
        for digits in [b"".as_slice(), b"0", b"000"] {
            let zero = decimal(digits, true, 0, 0);
            let zero_frame = encode_native_identity(zero).unwrap();
            assert_eq!(native_intdiv_needs_probe(invalid_left, zero), Some(false));
            assert_eq!(
                native_intdiv_needs_fallback(invalid_left, zero, None),
                Some(false)
            );
            assert!(native_intdiv_args_valid(
                Some(&invalid_frame),
                None,
                None,
                Some(&zero_frame)
            ));
            assert_eq!(
                evaluate_native_intdiv(
                    Some(&invalid_frame),
                    None,
                    None,
                    Some(&zero_frame),
                    false,
                    2
                )
                .unwrap(),
                vec![0, 0]
            );
            assert!(!native_intdiv_args_valid(
                Some(&invalid_frame),
                Some(0),
                None,
                Some(&zero_frame)
            ));
            assert!(!native_intdiv_args_valid(
                Some(&invalid_frame),
                None,
                Some(0),
                Some(&zero_frame)
            ));
        }
        let wide_visible = decimal(b"18446744073709551615", false, u32::MAX, 0);
        let left = encode_native_identity(wide_visible).unwrap();
        let right = encode_native_identity(one).unwrap();
        assert_eq!(native_intdiv_needs_probe(wide_visible, one), Some(true));
        assert_eq!(
            native_intdiv_needs_fallback(wide_visible, one, Some(0)),
            Some(false)
        );
        for (unsigned, expected) in [
            (true, NativeIntDivOutcome::Value(-1)),
            (false, NativeIntDivOutcome::IntOverflow),
        ] {
            let output =
                evaluate_native_intdiv(Some(&left), Some(0), None, Some(&right), unsigned, 1024)
                    .unwrap();
            assert_eq!(
                decode_native_intdiv_report(&output),
                Some(NativeIntDivReport {
                    warning: None,
                    outcome: expected
                })
            );
        }
        let minimum = encode_native_identity(decimal(
            b"-170141183460469231731687303715884105728",
            false,
            0,
            0,
        ))
        .unwrap();
        let negative_one = encode_native_identity(decimal(b"1", true, 0, 0)).unwrap();
        let output = evaluate_native_intdiv(
            Some(&minimum),
            Some(4),
            None,
            Some(&negative_one),
            false,
            1024,
        )
        .unwrap();
        assert_eq!(
            decode_native_intdiv_report(&output).unwrap().outcome,
            NativeIntDivOutcome::IntOverflow
        );
        let negative_fraction = encode_native_identity(decimal(b"1", true, 0, 0)).unwrap();
        let two = encode_native_identity(decimal(b"2", false, 0, 0)).unwrap();
        let output = evaluate_native_intdiv(
            Some(&negative_fraction),
            Some(4),
            None,
            Some(&two),
            true,
            1024,
        )
        .unwrap();
        assert_eq!(
            decode_native_intdiv_report(&output).unwrap().outcome,
            NativeIntDivOutcome::Value(0)
        );
        assert!(!native_intdiv_args_valid(None, Some(4), None, Some(&right)));
        assert!(!native_intdiv_args_valid(
            Some(&left),
            None,
            None,
            Some(&right)
        ));
        assert!(!native_intdiv_args_valid(
            Some(&left),
            Some(-1),
            None,
            Some(&right)
        ));
        assert!(!native_intdiv_args_valid(
            Some(&left),
            Some(i64::from(u32::MAX) + 1),
            None,
            Some(&right)
        ));
        assert!(!native_intdiv_args_valid(
            Some(&left),
            Some(4),
            Some(4),
            Some(&right)
        ));
        assert_eq!(
            native_intdiv_needs_probe(NativeIdentityRef::Int(1), one),
            None
        );
        assert!(evaluate_native_intdiv(Some(&left), Some(4), None, Some(&right), true, 9).is_err());
        assert!(
            evaluate_native_intdiv(Some(&left), Some(4), None, Some(&right), true, usize::MAX)
                .is_err()
        );
        assert!(
            std::panic::catch_unwind(|| native_intdiv_needs_probe(
                one,
                decimal(&[255], false, 0, 0)
            ))
            .is_err()
        );
        // A large ASCII lhs fails its raw i128 projection, but the eager tuple
        // still visits the RHS parser. Native raw signed MIN is not normalized.
        let wide = decimal(b"170141183460469231731687303715884105728", false, 0, 0);
        assert_eq!(native_intdiv_needs_fallback(wide, one, Some(4)), Some(true));
        let bad_left = decimal(&[255], false, 0, 0);
        assert!(
            std::panic::catch_unwind(|| native_intdiv_needs_fallback(bad_left, one, Some(4)))
                .is_err()
        );
    }

    #[test]
    fn native_intdiv_bounded_fallback_warning_reports_and_legacy_exact_core() {
        let one_view = decimal(b"1", false, 0, 0);
        let three_view = decimal(b"3", false, 0, 0);
        let one = encode_native_identity(one_view).unwrap();
        let three = encode_native_identity(three_view).unwrap();
        assert_eq!(
            native_intdiv_needs_fallback(one_view, three_view, Some(31)),
            Some(true)
        );
        assert!(native_intdiv_args_valid(
            Some(&one),
            Some(31),
            Some(0),
            Some(&three)
        ));
        assert!(!native_intdiv_args_valid(
            Some(&one),
            Some(31),
            None,
            Some(&three)
        ));
        assert!(!native_intdiv_args_valid(
            Some(&one),
            Some(31),
            Some(-1),
            Some(&three)
        ));
        let output =
            evaluate_native_intdiv(Some(&one), Some(31), Some(0), Some(&three), false, 4096)
                .unwrap();
        assert_eq!(
            decode_native_intdiv_report(&output),
            Some(NativeIntDivReport {
                warning: None,
                outcome: NativeIntDivOutcome::Value(0)
            })
        );
        let output =
            evaluate_native_intdiv(Some(&one), Some(31), Some(90), Some(&three), false, 4096)
                .unwrap();
        let expected = format!("Truncated incorrect DECIMAL value: '0.{}'", "3".repeat(30));
        assert_eq!(
            decode_native_intdiv_report(&output),
            Some(NativeIntDivReport {
                warning: Some(&expected),
                outcome: NativeIntDivOutcome::Value(0)
            })
        );
        for width in [73, 82] {
            let digits = "9".repeat(width);
            let left = encode_native_identity(decimal(digits.as_bytes(), false, 0, 0)).unwrap();
            let output =
                evaluate_native_intdiv(Some(&left), Some(4), Some(4), Some(&one), false, 4096)
                    .unwrap();
            // 73 integer digits leave no fraction word after Truncated;
            // 82 digits overflow into the signed 81-digit maximum.
            let expected = format!(
                "Truncated incorrect DECIMAL value: '{}'",
                "9".repeat(width.min(81))
            );
            assert_eq!(
                decode_native_intdiv_report(&output),
                Some(NativeIntDivReport {
                    warning: Some(&expected),
                    outcome: NativeIntDivOutcome::IntOverflow
                })
            );
        }
        let negative_tenth = encode_native_identity(decimal(b"01", true, 1, 1)).unwrap();
        assert!(native_intdiv_args_valid(
            Some(&negative_tenth),
            None,
            Some(4),
            Some(&one)
        ));
        let output =
            evaluate_native_intdiv(Some(&negative_tenth), None, Some(4), Some(&one), true, 4096)
                .unwrap();
        assert_eq!(
            decode_native_intdiv_report(&output).unwrap().outcome,
            NativeIntDivOutcome::Value(0)
        );
        assert!(
            evaluate_native_intdiv(Some(&one), Some(31), Some(4), Some(&three), false, 0).is_err()
        );
        let valid = report(NativeIntDivOutcome::Value(-1), Some("actual warning"), 128).unwrap();
        assert!(native_intdiv_result_valid(&valid));
        let mut trailing = valid.clone();
        trailing.push(0);
        let mut invalid_utf8 = valid.clone();
        *invalid_utf8.last_mut().unwrap() = 255;
        let mut bad_extent = valid;
        bad_extent[10..18].copy_from_slice(&u64::MAX.to_le_bytes());
        for invalid in [
            vec![],
            vec![0],
            vec![3, 0],
            vec![0, 1],
            vec![2, 2],
            vec![1, 0],
            vec![2, 0, 0],
            trailing,
            invalid_utf8,
            bad_extent,
        ] {
            assert!(!native_intdiv_result_valid(&invalid));
        }
        let legacy_zero = encode_native_identity(decimal(b"", false, 0, 0)).unwrap();
        let invalid_left = encode_native_identity(decimal(&[255], false, 0, 0)).unwrap();
        assert!(native_intdiv_legacy_args_valid(
            Some(&invalid_left),
            Some(&legacy_zero)
        ));
        assert_eq!(
            evaluate_legacy_decimal_intdiv(&invalid_left, &legacy_zero, 0).unwrap(),
            None
        );
        let seven = encode_native_identity(decimal(b"7", true, 0, 0)).unwrap();
        let two = encode_native_identity(decimal(b"2", false, 0, 0)).unwrap();
        assert_eq!(
            evaluate_legacy_decimal_intdiv(&seven, &two, 4096).unwrap(),
            Some(-3)
        );
        let maximum =
            encode_native_identity(decimal(b"18446744073709551615", false, 0, 0)).unwrap();
        assert_eq!(
            evaluate_legacy_decimal_intdiv(&maximum, &one, 4096).unwrap(),
            None
        );
        assert!(!native_intdiv_legacy_args_valid(None, Some(&one)));
        assert!(evaluate_legacy_decimal_intdiv(&one, &three, 0).is_err());
        assert!(evaluate_legacy_decimal_intdiv(&legacy_zero, &one, 4096).is_err());
    }
}
