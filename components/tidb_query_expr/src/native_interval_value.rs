// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Actual borrowed INTERVAL classification and typed search state, not a
//! transport report, source tree or another evaluator.
use super::{
    CmpOpLe, CmpOpLt, NativeIntervalCast as Cast, NativeIntervalEvalType as Type,
    NativeIntervalFieldType, NativeIntervalNumber as Number, integer_predicate, less,
};

/// Classification of an actual already-provided eager value. Other values
/// retain their original real-coercion callback; no host computes membership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeIntervalEagerInput {
    /// Actual SQL NULL.
    Null,
    /// Actual signed native integer.
    Int(i64),
    /// Actual unsigned native integer.
    UInt(u64),
    /// Actual range sentinel, checked before target NULL.
    Sentinel,
    /// An actual non-NULL value outside the integer/sentinel domains.
    Other,
}
impl NativeIntervalEagerInput {
    fn integer(self) -> Option<Number> {
        match self {
            Self::Int(value) => Some(Number::Signed(value)),
            Self::UInt(value) => Some(Number::Unsigned(value)),
            _ => None,
        }
    }
}
/// Invalid value/state contracts stay separate from explicit logical limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeIntervalValueError {
    /// Original eager/lazy argument shape refusal before any value request.
    Arity {
        /// Actual number of supplied arguments or metadata records.
        actual: usize,
    },
    /// Wrong number kind or use of an already completed search.
    Invalid,
    /// A finite retained-state allowance failed before its allocation/work.
    ResourceLimit(&'static str),
}
/// A real SDK demand or actual final index, without serialized state/report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeIntervalValueStep {
    /// The actual next argument ordinal and original coercion domain.
    Probe { index: usize, cast: Cast },
    /// Actual index, including -1 for target NULL.
    Index(i64),
}
/// Copyable fresh lazy search state. Immutable metadata chooses its domain and
/// nullable linear/binary policy; actual typed values advance it afterward.
#[derive(Clone, Copy, Debug)]
pub struct NativeIntervalSearch {
    pub(super) count: usize,
    pub(super) nullable: bool,
    pub(super) cast: Cast,
    pub(super) low: usize,
    pub(super) high: usize,
    pub(super) target: Option<Number>,
    result: Option<i64>,
}
impl NativeIntervalSearch {
    /// Consume complete actual metadata observations, never source evaluation
    /// or a host-precomputed all-integer/nullable answer. No buffer is
    /// allocated.
    pub fn new(
        types: impl ExactSizeIterator<Item = Option<NativeIntervalFieldType>>,
    ) -> Result<Self, NativeIntervalValueError> {
        let count = types.len();
        if count < 2 {
            return Err(NativeIntervalValueError::Arity { actual: count });
        }
        if count > isize::MAX as usize {
            return Err(NativeIntervalValueError::Invalid);
        }
        let mut all_int = true;
        let mut nullable = false;
        for field in types {
            all_int &= field.is_some_and(|field| field.eval_type == Type::Int);
            nullable |= field.is_none_or(|field| field.flags & 1 == 0);
        }
        Ok(Self {
            count,
            nullable,
            cast: if all_int { Cast::Int } else { Cast::Real },
            low: 1,
            high: count,
            target: None,
            result: None,
        })
    }
    /// Number of original argument metadata records bound to this search.
    pub fn argument_count(&self) -> usize {
        self.count
    }
    /// Return only the current real demand or completed index.
    pub fn step(&self) -> NativeIntervalValueStep {
        if let Some(index) = self.result {
            return NativeIntervalValueStep::Index(index);
        }
        let index = if self.target.is_none() {
            0
        } else if self.nullable {
            self.low
        } else {
            self.low + (self.high - self.low) / 2
        };
        NativeIntervalValueStep::Probe {
            index,
            cast: self.cast,
        }
    }
    /// Accept one actual already-cast value. Real NaN uses target<boundary;
    /// a NULL target terminates even under an original NOT_NULL declaration.
    pub fn accept(
        &mut self,
        value: Option<Number>,
    ) -> Result<NativeIntervalValueStep, NativeIntervalValueError> {
        if self.result.is_some()
            || value.is_some_and(|value| {
                !matches!(
                    (self.cast, value),
                    (Cast::Int, Number::Signed(_) | Number::Unsigned(_))
                        | (Cast::Real, Number::Real(_))
                )
            })
        {
            return Err(NativeIntervalValueError::Invalid);
        }
        let Some(target) = self.target else {
            if let Some(value) = value {
                self.target = Some(value);
            } else {
                self.result = Some(-1);
            }
            return Ok(self.step());
        };
        let NativeIntervalValueStep::Probe { index, .. } = self.step() else {
            unreachable!()
        };
        let before = value.is_some_and(|boundary| less(target, boundary));
        if self.nullable {
            if before {
                self.result = Some((index - 1) as i64);
                return Ok(self.step());
            }
            self.low += 1;
        } else if before {
            self.high = index;
        } else {
            self.low = index + 1;
        }
        if self.low >= self.high {
            self.result = Some((self.low - 1) as i64);
        }
        Ok(self.step())
    }
    pub(super) fn resumed(
        count: usize,
        nullable: bool,
        cast: Cast,
        low: usize,
        high: usize,
        target: Option<Number>,
    ) -> Self {
        Self {
            count,
            nullable,
            cast,
            low,
            high,
            target,
            result: None,
        }
    }
}

pub(super) enum EagerClassification {
    Index(i64),
    Sentinels,
    Real { nullable: bool },
}
pub(super) fn eager_classify<T>(
    values: &[T],
    view: impl Fn(&T) -> NativeIntervalEagerInput,
) -> Result<EagerClassification, NativeIntervalValueError> {
    use NativeIntervalEagerInput as V;
    if values.len() < 2 {
        return Err(NativeIntervalValueError::Arity {
            actual: values.len(),
        });
    }
    if values.len() > isize::MAX as usize {
        return Err(NativeIntervalValueError::Invalid);
    }
    if values.iter().any(|value| view(value) == V::Sentinel) {
        return Ok(EagerClassification::Sentinels);
    }
    if view(&values[0]) == V::Null {
        return Ok(EagerClassification::Index(-1));
    }
    let nullable = values.iter().any(|value| view(value) == V::Null);
    if values
        .iter()
        .all(|value| matches!(view(value), V::Null | V::Int(_) | V::UInt(_)))
    {
        let target = view(&values[0]).integer().expect("non-NULL integer target");
        let index = if nullable {
            values[1..]
                .iter()
                .position(|value| {
                    view(value)
                        .integer()
                        .is_some_and(|boundary| integer_predicate::<CmpOpLt>(target, boundary))
                })
                .unwrap_or(values.len() - 1)
        } else {
            values[1..].partition_point(|value| {
                view(value)
                    .integer()
                    .is_some_and(|boundary| integer_predicate::<CmpOpLe>(boundary, target))
            })
        };
        return Ok(EagerClassification::Index(index as i64));
    }
    Ok(EagerClassification::Real { nullable })
}
/// An actual final answer, sentinel refusal or genuine all-real cast state.
#[derive(Debug)]
pub enum NativeIntervalEagerStart {
    /// Actual result without any real casts.
    Index(i64),
    /// Preserve the eager range-sentinel SQL refusal before target NULL.
    Sentinels,
    /// Actual eager real conversions, in original non-NULL argument order.
    Real(NativeIntervalEagerReal),
}
/// Eager real state retains only NULL markers and actual converted f64 bits.
/// It reserves its complete finite state once, before any coercion effect.
#[derive(Debug)]
pub struct NativeIntervalEagerReal {
    nulls: Vec<bool>,
    reals: Vec<u64>,
    low: usize,
    nullable: bool,
    result: Option<i64>,
}
impl NativeIntervalEagerReal {
    /// Actual owned state capacity. The caller separately bounds its borrowed
    /// input views and original conversion work; no source payload is copied.
    pub fn retained_bytes(&self) -> usize {
        self.nulls.capacity() + self.reals.capacity() * 8
    }
    /// Return the next actual non-NULL argument or the completed index.
    pub fn step(&self) -> NativeIntervalValueStep {
        self.result.map_or(
            NativeIntervalValueStep::Probe {
                index: self.low,
                cast: Cast::Real,
            },
            NativeIntervalValueStep::Index,
        )
    }
    /// Store a genuine conversion result. All non-NULL conversions complete
    /// before partition; a late error must therefore remain reachable.
    pub fn accept_real(
        &mut self,
        value: f64,
    ) -> Result<NativeIntervalValueStep, NativeIntervalValueError> {
        if self.result.is_some() || self.low >= self.nulls.len() || self.nulls[self.low] {
            return Err(NativeIntervalValueError::Invalid);
        }
        self.reals.push(value.to_bits());
        self.low += 1;
        while self.low < self.nulls.len() && self.nulls[self.low] {
            self.low += 1;
        }
        if self.low == self.nulls.len() {
            self.result = Some(eager_real_index(&self.nulls, &self.reals, self.nullable));
        }
        Ok(self.step())
    }
}
pub(super) fn eager_real_index(nulls: &[bool], reals: &[u64], nullable: bool) -> i64 {
    let target = f64::from_bits(reals[0]);
    if nullable {
        let mut real = reals[1..].iter();
        nulls[1..]
            .iter()
            .position(|null| {
                !*null
                    && target
                        < f64::from_bits(
                            *real.next().expect("one actual real per non-NULL boundary"),
                        )
            })
            .unwrap_or(nulls.len() - 1) as i64
    } else {
        // This is <=, not !(target<boundary). Raw NaN is deliberately distinct
        // from the lazy search and no source callback is involved here.
        reals[1..].partition_point(|bits| f64::from_bits(*bits) <= target) as i64
    }
}
/// Classify actual already-evaluated values before any cast. The caller builds
/// these views without evaluating/coercing an expression. Real state owns no
/// original input payload, identity frame, report or execution program.
pub fn native_interval_eager_values(
    values: &[NativeIntervalEagerInput],
    maximum: usize,
) -> Result<NativeIntervalEagerStart, NativeIntervalValueError> {
    let class = eager_classify(values, |value| *value)?;

    let nullable = match class {
        EagerClassification::Index(index) => return Ok(NativeIntervalEagerStart::Index(index)),
        EagerClassification::Sentinels => return Ok(NativeIntervalEagerStart::Sentinels),
        EagerClassification::Real { nullable } => nullable,
    };
    let reals = values
        .iter()
        .filter(|value| **value != NativeIntervalEagerInput::Null)
        .count();
    let bound = values
        .len()
        .checked_add(
            reals
                .checked_mul(8)
                .ok_or(NativeIntervalValueError::ResourceLimit(
                    "INTERVAL state extent overflow",
                ))?,
        )
        .ok_or(NativeIntervalValueError::ResourceLimit(
            "INTERVAL state extent overflow",
        ))?;
    if bound > maximum {
        return Err(NativeIntervalValueError::ResourceLimit(
            "INTERVAL eager state exceeds allowance",
        ));
    }
    let mut nulls = Vec::new();
    let mut numbers = Vec::new();
    nulls.try_reserve_exact(values.len()).map_err(|_| {
        NativeIntervalValueError::ResourceLimit("INTERVAL NULL state allocation failed")
    })?;
    numbers.try_reserve_exact(reals).map_err(|_| {
        NativeIntervalValueError::ResourceLimit("INTERVAL real state allocation failed")
    })?;
    let bytes = nulls
        .capacity()
        .checked_add(numbers.capacity().checked_mul(8).ok_or(
            NativeIntervalValueError::ResourceLimit("INTERVAL state capacity overflow"),
        )?)
        .ok_or(NativeIntervalValueError::ResourceLimit(
            "INTERVAL state capacity overflow",
        ))?;
    if bytes > maximum {
        return Err(NativeIntervalValueError::ResourceLimit(
            "INTERVAL state capacity exceeds allowance",
        ));
    }
    nulls.extend(
        values
            .iter()
            .map(|value| *value == NativeIntervalEagerInput::Null),
    );
    Ok(NativeIntervalEagerStart::Real(NativeIntervalEagerReal {
        nulls,
        reals: numbers,
        low: 0,
        nullable,
        result: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fields(
        count: usize,
        kind: Type,
        flags: u32,
    ) -> impl ExactSizeIterator<Item = Option<NativeIntervalFieldType>> {
        vec![
            Some(NativeIntervalFieldType {
                eval_type: kind,
                flags
            });
            count
        ]
        .into_iter()
    }
    #[test]
    fn typed_search_preserves_actual_domains_probe_order_nan_null_and_contracts() {
        for count in [0, 1] {
            assert!(
                matches!(NativeIntervalSearch::new(fields(count, Type::Int, 1)), Err(NativeIntervalValueError::Arity { actual }) if actual == count)
            );
        }
        let mut search = NativeIntervalSearch::new(fields(3, Type::Real, 1)).unwrap();
        assert_eq!(
            search.step(),
            NativeIntervalValueStep::Probe {
                index: 0,
                cast: Cast::Real
            }
        );
        assert_eq!(
            search
                .accept(Some(Number::Real(f64::NAN.to_bits())))
                .unwrap(),
            NativeIntervalValueStep::Probe {
                index: 2,
                cast: Cast::Real
            }
        );
        assert_eq!(
            search.accept(Some(Number::Real(2.0f64.to_bits()))).unwrap(),
            NativeIntervalValueStep::Index(2)
        );
        assert_eq!(search.accept(None), Err(NativeIntervalValueError::Invalid));
        let mut search = NativeIntervalSearch::new(fields(4, Type::Int, 0)).unwrap();
        assert_eq!(
            search.accept(Some(Number::Real(1.0f64.to_bits()))),
            Err(NativeIntervalValueError::Invalid)
        );
        assert_eq!(
            search.accept(Some(Number::Signed(1))).unwrap(),
            NativeIntervalValueStep::Probe {
                index: 1,
                cast: Cast::Int
            }
        );
        assert_eq!(
            search.accept(None).unwrap(),
            NativeIntervalValueStep::Probe {
                index: 2,
                cast: Cast::Int
            }
        );
        assert_eq!(
            search.accept(Some(Number::Unsigned(u64::MAX))).unwrap(),
            NativeIntervalValueStep::Index(1)
        );
        let mut search = NativeIntervalSearch::new(fields(3, Type::Int, 1)).unwrap();
        search.accept(Some(Number::Unsigned(u64::MAX))).unwrap();
        assert_eq!(
            search.accept(Some(Number::Signed(-1))).unwrap(),
            NativeIntervalValueStep::Index(2)
        );
        let mut null = NativeIntervalSearch::new(fields(3, Type::Int, 1)).unwrap();
        assert_eq!(
            null.accept(None).unwrap(),
            NativeIntervalValueStep::Index(-1)
        );
    }
    #[test]
    fn eager_borrowed_classification_and_real_state_keep_precedence_nan_and_finite_capacity() {
        use NativeIntervalEagerInput as V;
        assert!(matches!(
            native_interval_eager_values(&[V::Null, V::Sentinel], 0),
            Ok(NativeIntervalEagerStart::Sentinels)
        ));
        assert!(matches!(
            native_interval_eager_values(&[V::Null, V::Other], 0),
            Ok(NativeIntervalEagerStart::Index(-1))
        ));
        assert!(matches!(
            native_interval_eager_values(&[V::Int(-1), V::UInt(0), V::UInt(u64::MAX)], 0),
            Ok(NativeIntervalEagerStart::Index(0))
        ));
        assert!(matches!(
            native_interval_eager_values(&[V::Other, V::Other], 0),
            Err(NativeIntervalValueError::ResourceLimit(_))
        ));
        assert!(native_interval_eager_values(&[V::Int(1), V::Int(2)], usize::MAX).is_ok());
        for nullable in [false, true] {
            let views = if nullable {
                vec![V::Other, V::Other, V::Null, V::Other]
            } else {
                vec![V::Other; 3]
            };
            let NativeIntervalEagerStart::Real(mut state) =
                native_interval_eager_values(&views, 1024).unwrap()
            else {
                panic!("real state")
            };
            let bytes = state.retained_bytes();
            assert_eq!(
                state.accept_real(f64::NAN).unwrap(),
                NativeIntervalValueStep::Probe {
                    index: 1,
                    cast: Cast::Real
                }
            );
            assert_eq!(
                state.accept_real(1.0).unwrap(),
                NativeIntervalValueStep::Probe {
                    index: if nullable { 3 } else { 2 },
                    cast: Cast::Real
                }
            );
            assert_eq!(
                state.accept_real(2.0).unwrap(),
                NativeIntervalValueStep::Index(if nullable { 3 } else { 0 })
            );
            assert_eq!(
                state.retained_bytes(),
                bytes,
                "reserved state must never grow during effects"
            );
            assert_eq!(
                state.accept_real(3.0),
                Err(NativeIntervalValueError::Invalid)
            );
        }
    }
    #[test]
    fn ordinary_wire_validation_still_precedes_null_and_typed_progress() {
        let identity = crate::encode_native_identity(crate::NativeIdentityRef::Int(1)).unwrap();
        let mut packet =
            crate::encode_native_interval_eager_head(&[None, Some(&identity)]).unwrap();
        packet.pop();
        assert!(!crate::interval_eager_head_native_args_valid(Some(&packet)));
        assert!(matches!(
            super::super::evaluate_interval_eager_head_native(Some(&packet)),
            Err(crate::NativeIdentityFrameError::Invalid)
        ));
        let mut packet = crate::encode_native_interval_lazy_head(
            &[Some(NativeIntervalFieldType {
                eval_type: Type::Int,
                flags: u32::MAX,
            }); 3],
        )
        .unwrap();
        packet.pop();
        assert!(!crate::interval_lazy_head_native_args_valid(Some(&packet)));
        assert!(matches!(
            super::super::evaluate_interval_lazy_head_native(Some(&packet)),
            Err(crate::NativeIdentityFrameError::Invalid)
        ));
    }
}
