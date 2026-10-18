// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Actual GREATEST/LEAST sequence values, without identity frames or
//! evaluators.
use std::cmp::Ordering;

use tidb_query_datatype::codec::{
    collation::native::NativeCollation,
    mysql::{
        NativeVectorFloat32, Time, TimeType,
        time::{
            NativeSessionTimeZone, NativeTemporalValue, native_get_time_fsp, native_parse_time,
        },
    },
};

use crate::NativeExtremumDomain;

/// Wrong typed progress is separate from a finite logical resource refusal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExtremumValueError {
    /// Inconsistent domain, completed reducer or wrong actual value kind.
    Invalid,
    /// Retained value capacity or demanded work exceeds its finite allowance.
    ResourceLimit(&'static str),
}
/// Actual cast/coercion results. No host-computed ordering or winner is
/// accepted.
#[derive(Debug)]
pub enum NativeExtremumSequenceValue {
    /// Original datetime cast result, including its raw core and precision.
    Time(NativeTemporalValue),
    /// Original native vector cast result.
    Vector(NativeVectorFloat32),
    /// Original direct bytes or SDK-normalized temporal text.
    Bytes(Vec<u8>),
}
impl NativeExtremumSequenceValue {
    fn retained_bytes(&self) -> Result<usize, NativeExtremumValueError> {
        match self {
            Self::Time(_) => Ok(0),
            Self::Bytes(value) => Ok(value.capacity()),
            Self::Vector(value) => value.elements_capacity().checked_mul(4).ok_or(
                NativeExtremumValueError::ResourceLimit("extremum vector capacity overflow"),
            ),
        }
    }
}
/// An actual typed reducer. Numeric comparison/promotion uses the separate
/// NativeExtremumNumericCursor, which consumes actual comparison datums.
#[derive(Debug)]
pub struct NativeExtremumSequence {
    domain: NativeExtremumDomain,
    want: Ordering,
    collation: NativeCollation,
    count: usize,
    next: usize,
    best: Option<NativeExtremumSequenceValue>,
    null: bool,
    maximum: usize,
}
impl NativeExtremumSequence {
    /// Bind the actual SDK head's domain and finite owned-value allowance.
    pub fn new(
        domain: NativeExtremumDomain,
        want: Ordering,
        collation: NativeCollation,
        count: usize,
        maximum: usize,
    ) -> Result<Self, NativeExtremumValueError> {
        if count == 0 || domain == NativeExtremumDomain::Numeric {
            return Err(NativeExtremumValueError::Invalid);
        }
        finite(maximum)?;
        Ok(Self {
            domain,
            want,
            collation,
            count,
            next: 0,
            best: None,
            null: false,
            maximum,
        })
    }
    /// Only the next actual cast/coercion argument; None means complete.
    pub fn next_index(&self) -> Option<usize> {
        (self.next < self.count).then_some(self.next)
    }
    /// Actual owned winner capacity, excluding fixed stack state.
    pub fn retained_bytes(&self) -> Result<usize, NativeExtremumValueError> {
        self.best
            .as_ref()
            .map_or(Ok(0), NativeExtremumSequenceValue::retained_bytes)
    }
    /// Accept one actual result. Only temporal casts/text coercion may become
    /// NULL here; the head already scanned original NULLs before domain choice.
    pub fn accept(
        &mut self,
        value: Option<NativeExtremumSequenceValue>,
    ) -> Result<(), NativeExtremumValueError> {
        use NativeExtremumDomain as D;
        use NativeExtremumSequenceValue as V;
        if self.next >= self.count {
            return Err(NativeExtremumValueError::Invalid);
        }
        let Some(value) = value else {
            if !matches!(self.domain, D::Time { .. } | D::StringAsTime { .. }) {
                return Err(NativeExtremumValueError::Invalid);
            }
            self.null = true;
            self.best = None;
            self.next = self.count;
            return Ok(());
        };
        if !matches!(
            (self.domain, &value),
            (D::Time { .. }, V::Time(_))
                | (D::Vector, V::Vector(_))
                | (D::DirectString | D::StringAsTime { .. }, V::Bytes(_))
        ) {
            return Err(NativeExtremumValueError::Invalid);
        }
        let live = self
            .retained_bytes()?
            .checked_add(value.retained_bytes()?)
            .ok_or(NativeExtremumValueError::ResourceLimit(
                "extremum live capacity overflow",
            ))?;
        check(live, self.maximum)?;
        let wins = match (&value, &self.best) {
            (_, None) => true,
            (V::Time(candidate), Some(V::Time(best))) => time_wins(self.want, *candidate, *best),
            (V::Vector(candidate), Some(V::Vector(best))) => {
                vector_wins(self.want, candidate, best)
            }
            (V::Bytes(candidate), Some(V::Bytes(best))) => match self.domain {
                D::DirectString => string_wins(self.want, self.collation, candidate, best),
                D::StringAsTime { .. } => time_text_wins(self.want, candidate, best),
                _ => return Err(NativeExtremumValueError::Invalid),
            },
            _ => return Err(NativeExtremumValueError::Invalid),
        };
        if wins {
            self.best = Some(value);
        }
        self.next += 1;
        Ok(())
    }
    /// Move the actual final value, retaining original time precision while
    /// retagging Date/Datetime exactly as the old native sequence did.
    pub fn finish(self) -> Result<Option<NativeExtremumSequenceValue>, NativeExtremumValueError> {
        if self.next != self.count {
            return Err(NativeExtremumValueError::Invalid);
        }
        if self.null {
            return Ok(None);
        }
        let mut value = self.best.ok_or(NativeExtremumValueError::Invalid)?;
        if let (NativeExtremumDomain::Time { ret_date }, NativeExtremumSequenceValue::Time(value)) =
            (self.domain, &mut value)
        {
            value.set_kind(if ret_date {
                TimeType::Date
            } else {
                TimeType::DateTime
            });
        }
        Ok(Some(value))
    }
}
fn finite(maximum: usize) -> Result<(), NativeExtremumValueError> {
    if maximum == usize::MAX {
        Err(NativeExtremumValueError::ResourceLimit(
            "extremum requires a finite value allowance",
        ))
    } else {
        Ok(())
    }
}
fn check(bytes: usize, maximum: usize) -> Result<(), NativeExtremumValueError> {
    finite(maximum)?;
    if bytes > maximum {
        Err(NativeExtremumValueError::ResourceLimit(
            "extremum actual value/work allowance exceeded",
        ))
    } else {
        Ok(())
    }
}
pub(crate) fn time_wins(
    want: Ordering,
    candidate: NativeTemporalValue,
    best: NativeTemporalValue,
) -> bool {
    Time::native_core_compare(candidate.raw, best.raw) == want
}
pub(crate) fn vector_wins(
    want: Ordering,
    candidate: &NativeVectorFloat32,
    best: &NativeVectorFloat32,
) -> bool {
    candidate.compare(best) == want
}
pub(crate) fn string_wins(
    want: Ordering,
    collation: NativeCollation,
    candidate: &[u8],
    best: &[u8],
) -> bool {
    collation
        .compare(candidate, best)
        .expect("raw supported collation comparison cannot fail")
        == want
}
pub(crate) fn time_text_wins(want: Ordering, candidate: &[u8], best: &[u8]) -> bool {
    if want == Ordering::Greater {
        candidate > best
    } else {
        candidate < best
    }
}
pub(crate) fn time_text_value(
    input: &str,
    as_date: bool,
    allow_invalid_dates: bool,
    zone: &NativeSessionTimeZone,
) -> Vec<u8> {
    let kind = if as_date {
        TimeType::Date
    } else {
        TimeType::DateTime
    };
    let fsp = if as_date {
        0
    } else {
        i64::from(native_get_time_fsp(input))
    };
    match native_parse_time(
        input,
        kind,
        fsp,
        false,
        true,
        allow_invalid_dates,
        true,
        zone,
    ) {
        Ok(parsed) => {
            let mut out = String::new();
            Time::write_native_core_display(parsed.time.raw, as_date, parsed.time.fsp, &mut out)
                .expect("formatting Time into String cannot fail");
            out.into_bytes()
        }
        Err(_) => input.as_bytes().to_vec(),
    }
}
/// Parse the actual text using original per-item context. Parse failure keeps
/// original text without warnings. Bound work before parsing/formatting.
pub fn native_extremum_time_text_value(
    input: &str,
    as_date: bool,
    allow_invalid_dates: bool,
    zone: &NativeSessionTimeZone,
    maximum: usize,
) -> Result<Vec<u8>, NativeExtremumValueError> {
    let work = input
        .len()
        .checked_mul(16)
        .and_then(|n| n.checked_add(512))
        .ok_or(NativeExtremumValueError::ResourceLimit(
            "extremum temporal text extent overflow",
        ))?;
    check(work, maximum)?;
    let value = time_text_value(input, as_date, allow_invalid_dates, zone);
    check(value.capacity(), maximum)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use NativeExtremumSequenceValue as V;

    use super::*;
    #[test]
    fn sequence_owns_actual_time_vector_and_collation_winners() {
        let time = NativeTemporalValue {
            raw: 0,
            kind: TimeType::Timestamp,
            fsp: 6,
        };
        let mut reducer = NativeExtremumSequence::new(
            NativeExtremumDomain::Time { ret_date: false },
            Ordering::Equal,
            NativeCollation::Binary,
            2,
            0,
        )
        .unwrap();
        assert_eq!(reducer.next_index(), Some(0));
        reducer
            .accept(Some(V::Time(NativeTemporalValue { fsp: 1, ..time })))
            .unwrap();
        reducer.accept(Some(V::Time(time))).unwrap();
        assert_eq!(reducer.next_index(), None);
        assert!(matches!(
            reducer.accept(Some(V::Time(time))),
            Err(NativeExtremumValueError::Invalid)
        ));
        let Some(V::Time(value)) = reducer.finish().unwrap() else {
            panic!("time")
        };
        assert_eq!(value.kind, TimeType::DateTime);
        assert_eq!(value.fsp, 6);
        for want in [Ordering::Less, Ordering::Greater] {
            let mut reducer = NativeExtremumSequence::new(
                NativeExtremumDomain::Vector,
                want,
                NativeCollation::Binary,
                2,
                1024,
            )
            .unwrap();
            reducer
                .accept(Some(V::Vector(NativeVectorFloat32::must_create(vec![1.0]))))
                .unwrap();
            reducer
                .accept(Some(V::Vector(NativeVectorFloat32::must_create(vec![2.0]))))
                .unwrap();
            let Some(V::Vector(value)) = reducer.finish().unwrap() else {
                panic!("vector")
            };
            assert_eq!(
                value.elements(),
                if want == Ordering::Less {
                    &[1.0][..]
                } else {
                    &[2.0][..]
                }
            );
        }
        for domain in [
            NativeExtremumDomain::DirectString,
            NativeExtremumDomain::StringAsTime { as_date: true },
        ] {
            let mut reducer = NativeExtremumSequence::new(
                domain,
                Ordering::Equal,
                NativeCollation::Utf8Mb4GeneralCi,
                2,
                1024,
            )
            .unwrap();
            reducer.accept(Some(V::Bytes(b"A".to_vec()))).unwrap();
            reducer.accept(Some(V::Bytes(b"a".to_vec()))).unwrap();
            let Some(V::Bytes(value)) = reducer.finish().unwrap() else {
                panic!("bytes")
            };
            // Direct collation uses actual equality; time text's non-Greater arm uses <.
            assert_eq!(
                value,
                if domain == NativeExtremumDomain::DirectString {
                    b"a"
                } else {
                    b"A"
                }
            );
        }
    }
    #[test]
    fn sequence_checks_contracts_actual_capacity_and_null_before_reuse() {
        assert!(
            NativeExtremumSequence::new(
                NativeExtremumDomain::Numeric,
                Ordering::Less,
                NativeCollation::Binary,
                1,
                1024
            )
            .is_err()
        );
        assert!(
            NativeExtremumSequence::new(
                NativeExtremumDomain::Vector,
                Ordering::Less,
                NativeCollation::Binary,
                1,
                usize::MAX
            )
            .is_err()
        );
        let mut reducer = NativeExtremumSequence::new(
            NativeExtremumDomain::DirectString,
            Ordering::Less,
            NativeCollation::Binary,
            1,
            0,
        )
        .unwrap();
        assert!(matches!(
            reducer.accept(None),
            Err(NativeExtremumValueError::Invalid)
        ));
        assert!(matches!(
            reducer.accept(Some(V::Bytes(Vec::with_capacity(1024)))),
            Err(NativeExtremumValueError::ResourceLimit(_))
        ));
        assert_eq!(reducer.next_index(), Some(0));
        reducer.accept(Some(V::Bytes(Vec::new()))).unwrap();
        assert!(matches!(reducer.finish().unwrap(),Some(V::Bytes(value)) if value.is_empty()));
        let mut reducer = NativeExtremumSequence::new(
            NativeExtremumDomain::Time { ret_date: true },
            Ordering::Greater,
            NativeCollation::Binary,
            2,
            0,
        )
        .unwrap();
        reducer.accept(None).unwrap();
        assert_eq!(reducer.next_index(), None);
        assert!(reducer.finish().unwrap().is_none());
    }
    #[test]
    fn temporal_text_keeps_original_fallback_and_bounds_work_before_parse() {
        let zone = NativeSessionTimeZone::utc();
        assert_eq!(
            native_extremum_time_text_value("99-1-1", true, false, &zone, 4096).unwrap(),
            b"1999-01-01"
        );
        assert_eq!(
            native_extremum_time_text_value("zzz", false, false, &zone, 4096).unwrap(),
            b"zzz"
        );
        assert!(matches!(
            native_extremum_time_text_value("99-1-1", true, false, &zone, 0),
            Err(NativeExtremumValueError::ResourceLimit(_))
        ));
    }
}
