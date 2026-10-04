// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Partial GREATEST/LEAST policy foundation, not an RPN/runtime admission.
//! Owns the original head and numeric winner/promotion decisions. Existing
//! callers still actuate casts and comparisons; the other four reducers are
//! deliberately outside this module until the complete staged runtime lands.

use std::cmp::Ordering;

use tidb_query_datatype::codec::mysql::TimeType;

/// Observations of an actual datum, not a preselected domain or winner.
/// `kind` is the native Rust DatumKind discriminant: Null=0, Decimal=5,
/// Real=6, Float32=7, String=8, Bytes=9, Duration=11, Time=15, Vector=18.
/// Temporal kind and visible decimal scale are present ONLY for their kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeExtremumValueMeta {
    pub kind: u8,
    pub time_kind: Option<TimeType>,
    pub decimal_scale: Option<u32>,
}

impl NativeExtremumValueMeta {
    fn valid(self) -> bool {
        self.kind <= 18
            && (self.kind == 15) == self.time_kind.is_some()
            && (self.kind == 5) == self.decimal_scale.is_some()
    }
}

/// Lossless adapter domain for native EvalType, not the wire EvalType codec.
/// In particular, Datetime and Timestamp remain separate metadata values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExtremumEvalType {
    Int,
    Real,
    Decimal,
    String,
    Datetime,
    Timestamp,
    Duration,
    Json,
    VectorFloat32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExtremumStringMode {
    Directly,
    AsDate,
    AsDatetime,
}

/// The existing planner's actual optional GlSignature. This module does not
/// replace aggregate field-type inference or normalize combinations of fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeExtremumSignature {
    pub arg_type: NativeExtremumEvalType,
    pub cmp_string_mode: NativeExtremumStringMode,
    pub ret_date: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExtremumDomain {
    Vector,
    Time { ret_date: bool },
    StringAsTime { as_date: bool },
    DirectString,
    Numeric,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExtremumHead {
    Null,
    Domain(NativeExtremumDomain),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExtremumPolicyError {
    EmptyArguments,
    InvalidMetadata,
    IncompleteCursor,
    ExhaustedCursor,
}

/// Arity precedes the full actual-NULL scan, which precedes signature/domain
/// choice. Child evaluation is still eager and belongs to the caller before
/// this head runs. No global sentinel/raw rejection belongs here: a numeric
/// singleton can return its original value without ever comparing it.
pub fn native_extremum_head(
    values: &[NativeExtremumValueMeta],
    signature: Option<NativeExtremumSignature>,
) -> Result<NativeExtremumHead, NativeExtremumPolicyError> {
    if values.is_empty() {
        return Err(NativeExtremumPolicyError::EmptyArguments);
    }
    if values.iter().any(|value| value.kind == 0) {
        return Ok(NativeExtremumHead::Null);
    }
    if !values.iter().all(|value| value.valid()) {
        return Err(NativeExtremumPolicyError::InvalidMetadata);
    }
    let signature = signature.unwrap_or_else(|| {
        let arg_type = if values.iter().any(|value| value.kind == 18) {
            NativeExtremumEvalType::VectorFloat32
        } else if values.iter().all(|value| value.kind == 15) {
            NativeExtremumEvalType::Datetime
        } else if values.iter().any(|value| matches!(value.kind, 8 | 9)) {
            NativeExtremumEvalType::String
        } else {
            NativeExtremumEvalType::Real
        };
        NativeExtremumSignature {
            arg_type,
            cmp_string_mode: NativeExtremumStringMode::Directly,
            ret_date: values
                .iter()
                .all(|value| value.kind == 15 && value.time_kind == Some(TimeType::Date)),
        }
    });
    let domain = match signature.arg_type {
        NativeExtremumEvalType::VectorFloat32 => NativeExtremumDomain::Vector,
        NativeExtremumEvalType::Datetime | NativeExtremumEvalType::Timestamp => {
            NativeExtremumDomain::Time {
                ret_date: signature.ret_date,
            }
        }
        NativeExtremumEvalType::String => match signature.cmp_string_mode {
            NativeExtremumStringMode::Directly => NativeExtremumDomain::DirectString,
            NativeExtremumStringMode::AsDate => {
                NativeExtremumDomain::StringAsTime { as_date: true }
            }
            NativeExtremumStringMode::AsDatetime => {
                NativeExtremumDomain::StringAsTime { as_date: false }
            }
        },
        // This includes ETDuration and an explicitly supplied ETJson signature.
        // Only the planner, not this runtime head, folds aggregate JSON to String.
        _ => NativeExtremumDomain::Numeric,
    };
    Ok(NativeExtremumHead::Domain(domain))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExtremumComparisonOp {
    Lt,
    Gt,
}

/// The requested original candidate-versus-best comparison, not reversed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeExtremumComparisonRequest {
    pub candidate_index: usize,
    pub best_index: usize,
    pub op: NativeExtremumComparisonOp,
}

/// Projection of the ACTUAL computed comparison datum. The caller must not
/// turn it into a wins/tie flag. Only Datum::Int(1) replaces the incumbent;
/// SQL NULL and other datum kinds belong to OtherValue, not a fabricated zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExtremumComparisonValue {
    Int(i64),
    OtherValue,
}

/// The SDK-selected winner and exact post-comparison conversion. Precision
/// None is the mixed-signed/unsigned path's plain to_decimal; Some is the
/// original cast_to_precision(0, scale as u32), without extra normalization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExtremumPromotion {
    KeepWinner {
        index: usize,
    },
    ToReal {
        index: usize,
    },
    ToDecimal {
        index: usize,
        precision_scale: Option<u32>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeExtremumNumericCursor {
    count: usize,
    next: usize,
    best: usize,
    op: NativeExtremumComparisonOp,
}

impl NativeExtremumNumericCursor {
    pub fn new(count: usize, want: Ordering) -> Result<Self, NativeExtremumPolicyError> {
        if count == 0 {
            return Err(NativeExtremumPolicyError::EmptyArguments);
        }
        Ok(Self {
            count,
            next: 1,
            best: 0,
            // Preserve the original helper's non-Greater arm, including Equal.
            op: if want == Ordering::Greater {
                NativeExtremumComparisonOp::Gt
            } else {
                NativeExtremumComparisonOp::Lt
            },
        })
    }

    pub fn request(&self) -> Option<NativeExtremumComparisonRequest> {
        (self.next < self.count).then_some(NativeExtremumComparisonRequest {
            candidate_index: self.next,
            best_index: self.best,
            op: self.op,
        })
    }

    pub fn observe(
        &mut self,
        value: NativeExtremumComparisonValue,
    ) -> Result<(), NativeExtremumPolicyError> {
        if self.next >= self.count {
            return Err(NativeExtremumPolicyError::ExhaustedCursor);
        }
        if matches!(value, NativeExtremumComparisonValue::Int(1)) {
            self.best = self.next;
        }
        self.next += 1;
        Ok(())
    }

    /// Promotion is evaluated only after ALL original comparisons have
    /// completed. Metadata decimals may be empty, short, or longer than the
    /// argument list: the old helper's exact slice/get/maximum policy remains.
    pub fn finish(
        &self,
        values: &[NativeExtremumValueMeta],
        arg_decimals: &[i64],
        all_constant: bool,
    ) -> Result<NativeExtremumPromotion, NativeExtremumPolicyError> {
        if self.next < self.count {
            return Err(NativeExtremumPolicyError::IncompleteCursor);
        }
        if values.len() != self.count || !values.iter().all(|value| value.valid()) {
            return Err(NativeExtremumPolicyError::InvalidMetadata);
        }
        let index = self.best;
        if values.iter().any(|value| matches!(value.kind, 6 | 7)) {
            return Ok(NativeExtremumPromotion::ToReal { index });
        }
        if values.iter().any(|value| value.kind == 5) {
            let max_datum_scale = values
                .iter()
                .filter_map(|value| value.decimal_scale)
                .max()
                .unwrap_or(0);
            let scale = if all_constant && arg_decimals.iter().any(|decimal| *decimal >= 0) {
                arg_decimals
                    .iter()
                    .copied()
                    .filter(|decimal| *decimal >= 0)
                    .max()
                    .unwrap_or(0)
            } else if arg_decimals.get(index).is_some_and(|decimal| *decimal >= 0) {
                arg_decimals[index]
            } else {
                i64::from(max_datum_scale)
            };
            return Ok(NativeExtremumPromotion::ToDecimal {
                index,
                precision_scale: Some(scale as u32),
            });
        }
        if values.iter().any(|value| value.kind == 3) && values.iter().any(|value| value.kind == 4)
        {
            return Ok(NativeExtremumPromotion::ToDecimal {
                index,
                precision_scale: None,
            });
        }
        Ok(NativeExtremumPromotion::KeepWinner { index })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extremum_policy_preserves_head_order_cursor_results_and_scale_metadata() {
        let plain = |kind| NativeExtremumValueMeta {
            kind,
            time_kind: None,
            decimal_scale: None,
        };
        let date = NativeExtremumValueMeta {
            kind: 15,
            time_kind: Some(TimeType::Date),
            decimal_scale: None,
        };
        let datetime = NativeExtremumValueMeta {
            time_kind: Some(TimeType::DateTime),
            ..date
        };
        let decimal = NativeExtremumValueMeta {
            kind: 5,
            time_kind: None,
            decimal_scale: Some(3),
        };
        assert_eq!(
            native_extremum_head(&[], None),
            Err(NativeExtremumPolicyError::EmptyArguments)
        );
        assert_eq!(
            native_extremum_head(&[plain(17), plain(0), plain(18)], None),
            Ok(NativeExtremumHead::Null)
        );
        for (values, domain) in [
            (vec![plain(8), plain(18)], NativeExtremumDomain::Vector),
            (
                vec![date, date],
                NativeExtremumDomain::Time { ret_date: true },
            ),
            (
                vec![date, datetime],
                NativeExtremumDomain::Time { ret_date: false },
            ),
            (vec![date, plain(9)], NativeExtremumDomain::DirectString),
            (vec![plain(17)], NativeExtremumDomain::Numeric),
            (vec![plain(1)], NativeExtremumDomain::Numeric),
        ] {
            assert_eq!(
                native_extremum_head(&values, None),
                Ok(NativeExtremumHead::Domain(domain))
            );
        }
        let signature = NativeExtremumSignature {
            arg_type: NativeExtremumEvalType::String,
            cmp_string_mode: NativeExtremumStringMode::AsDate,
            ret_date: false,
        };
        assert_eq!(
            native_extremum_head(&[plain(3)], Some(signature)),
            Ok(NativeExtremumHead::Domain(
                NativeExtremumDomain::StringAsTime { as_date: true }
            ))
        );
        assert_eq!(
            native_extremum_head(
                &[plain(8)],
                Some(NativeExtremumSignature {
                    arg_type: NativeExtremumEvalType::Duration,
                    ..signature
                })
            ),
            Ok(NativeExtremumHead::Domain(NativeExtremumDomain::Numeric))
        );
        assert_eq!(
            native_extremum_head(&[plain(5)], None),
            Err(NativeExtremumPolicyError::InvalidMetadata)
        );
        for kind in [1, 2, 17] {
            let cursor = NativeExtremumNumericCursor::new(1, Ordering::Less).unwrap();
            assert!(cursor.request().is_none());
            assert_eq!(
                cursor.finish(&[plain(kind)], &[], false),
                Ok(NativeExtremumPromotion::KeepWinner { index: 0 })
            );
        }
        let mut cursor = NativeExtremumNumericCursor::new(5, Ordering::Greater).unwrap();
        assert_eq!(
            cursor.finish(&[plain(3); 5], &[], false),
            Err(NativeExtremumPolicyError::IncompleteCursor)
        );
        // Actual IEEE comparison zero (including unordered NaN), arbitrary Int,
        // and actual NULL/other results all retain the incumbent, never win.
        for actual in [
            NativeExtremumComparisonValue::Int(0),
            NativeExtremumComparisonValue::Int(2),
            NativeExtremumComparisonValue::OtherValue,
        ] {
            assert_eq!(cursor.request().unwrap().best_index, 0);
            cursor.observe(actual).unwrap();
        }
        assert_eq!(
            cursor.request(),
            Some(NativeExtremumComparisonRequest {
                candidate_index: 4,
                best_index: 0,
                op: NativeExtremumComparisonOp::Gt
            })
        );
        cursor
            .observe(NativeExtremumComparisonValue::Int(1))
            .unwrap();
        assert_eq!(
            cursor.finish(
                &[plain(3), plain(3), plain(3), plain(7), plain(3)],
                &[],
                false
            ),
            Ok(NativeExtremumPromotion::ToReal { index: 4 })
        );
        assert_eq!(
            cursor.observe(NativeExtremumComparisonValue::Int(1)),
            Err(NativeExtremumPolicyError::ExhaustedCursor)
        );
        let mut cursor = NativeExtremumNumericCursor::new(2, Ordering::Equal).unwrap();
        assert_eq!(cursor.request().unwrap().op, NativeExtremumComparisonOp::Lt);
        cursor
            .observe(NativeExtremumComparisonValue::Int(0))
            .unwrap();
        assert_eq!(
            cursor.finish(&[plain(3), plain(4)], &[], false),
            Ok(NativeExtremumPromotion::ToDecimal {
                index: 0,
                precision_scale: None
            })
        );
        for (metadata, constant, expected) in [
            (vec![], false, 3),
            (vec![0, 3], false, 0),
            (vec![0, 3], true, 3),
            (vec![-2], false, 3),
            (vec![-2, -1], true, 3),
            (vec![0, 3, 7], true, 7),
            (vec![0, 3, 7], false, 0),
            (vec![4_294_967_296], false, 0),
            (vec![i64::MAX], true, u32::MAX),
        ] {
            assert_eq!(
                cursor.finish(&[plain(3), decimal], &metadata, constant),
                Ok(NativeExtremumPromotion::ToDecimal {
                    index: 0,
                    precision_scale: Some(expected)
                })
            );
        }
        let mut second = NativeExtremumNumericCursor::new(2, Ordering::Less).unwrap();
        second
            .observe(NativeExtremumComparisonValue::Int(1))
            .unwrap();
        assert_eq!(
            second.finish(&[plain(3), decimal], &[0], false),
            Ok(NativeExtremumPromotion::ToDecimal {
                index: 1,
                precision_scale: Some(3)
            })
        );
    }
}
