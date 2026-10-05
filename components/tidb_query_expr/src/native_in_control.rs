// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native IN's pure control plane. Callbacks execute the original child reads,
//! coercions and comparison gateways; this module never invokes a comparison
//! producer behind those gateways and does not add a facade entry of its own.
//! In particular, exhaustive AST comparisons and the existing UnaryNot worker
//! retain their original observable invocation counts and statement callbacks.
use std::{borrow::Cow, collections::HashSet};

use tidb_query_datatype::codec::collation::{KeyOptions, native::NativeCollation};

pub use crate::NativeIntervalEvalType as NativeInControlEvalType;

/// An actual native datum classification, not a host-computed match predicate.
/// Unsigned integers and all other non-NULL datum kinds belong to `Other`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeInControlValue {
    Null,
    Int(i64),
    Other,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeInControlResult {
    Null,
    Bool(bool),
}
#[derive(Clone, Copy, Default)]
struct Membership {
    found_null: bool,
    found_match: bool,
}
impl Membership {
    fn observe(&mut self, value: NativeInControlValue) -> bool {
        match value {
            NativeInControlValue::Null => {
                self.found_null = true;
                false
            }
            NativeInControlValue::Int(0) => false,
            _ => {
                self.found_match = true;
                true
            }
        }
    }
    fn finish(self) -> NativeInControlResult {
        if self.found_match {
            NativeInControlResult::Bool(true)
        } else if self.found_null {
            NativeInControlResult::Null
        } else {
            NativeInControlResult::Bool(false)
        }
    }
}
fn scalar<E>(
    mut state: Membership,
    candidate_count: usize,
    early_match: bool,
    mut compare: impl FnMut(usize) -> Result<NativeInControlValue, E>,
) -> Result<NativeInControlResult, E> {
    for index in 0..candidate_count {
        let matched = state.observe(compare(index)?);
        if early_match && matched {
            return Ok(state.finish());
        }
    }
    Ok(state.finish())
}
/// AST list indices are zero-based candidate ordinals. The mandatory left
/// evaluation has already happened, but its NULL does not seed the accumulator.
/// Even a match must not suppress a later candidate evaluation/comparison.
pub fn native_in_ast_scalar<E>(
    candidate_count: usize,
    compare: impl FnMut(usize) -> Result<NativeInControlValue, E>,
) -> Result<NativeInControlResult, E> {
    scalar(Membership::default(), candidate_count, false, compare)
}
/// Ready-value IN returns on its first match. `compare(i)` denotes the original
/// comparison with values[i + 1], after the mandatory left value is available.
pub fn native_in_ready_values<E>(
    left: NativeInControlValue,
    candidate_count: usize,
    compare: impl FnMut(usize) -> Result<NativeInControlValue, E>,
) -> Result<NativeInControlResult, E> {
    scalar(
        Membership {
            found_null: left == NativeInControlValue::Null,
            found_match: false,
        },
        candidate_count,
        true,
        compare,
    )
}
/// Generic typed IN compares every candidate. The stored cache NULL flag is an
/// independent source field, even when no prepared key set is currently
/// present. Candidate indices are zero-based ordinals (original argument index
/// minus 1).
pub fn native_in_generic<E>(
    left: NativeInControlValue,
    candidate_count: usize,
    cache_has_null: bool,
    compare: impl FnMut(usize) -> Result<NativeInControlValue, E>,
) -> Result<NativeInControlResult, E> {
    scalar(
        Membership {
            found_null: left == NativeInControlValue::Null || cache_has_null,
            found_match: false,
        },
        candidate_count,
        false,
        compare,
    )
}

/// The actual syntactic candidate shape. A `Values` callback has evaluated the
/// entire right row, including any later field errors, before width is checked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeInRowCandidate<T> {
    NotRow,
    Values(Vec<T>),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeInRowError<E> {
    Child(E),
    ItemMismatch,
    WidthMismatch,
}
/// Shared row equality, also used by non-IN native row comparison adapters.
/// A definite mismatch short-circuits remaining comparisons, not the already
/// completed right-row evaluation. Empty equal-width rows have the true
/// identity.
pub fn native_row_equality<T, E>(
    left: &[T],
    right: &[T],
    mut compare: impl FnMut(&T, &T) -> Result<NativeInControlValue, E>,
) -> Result<NativeInControlResult, NativeInRowError<E>> {
    if left.len() != right.len() {
        return Err(NativeInRowError::WidthMismatch);
    }
    if left.is_empty() {
        return Ok(NativeInControlResult::Bool(true));
    }
    let mut saw_null = false;
    for (left, right) in left.iter().zip(right) {
        match compare(left, right).map_err(NativeInRowError::Child)? {
            NativeInControlValue::Int(0) => return Ok(NativeInControlResult::Bool(false)),
            NativeInControlValue::Null => saw_null = true,
            NativeInControlValue::Int(1) => {}
            _ => unreachable!("comparison worker returns only Int(0/1) or NULL"),
        }
    }
    Ok(if saw_null {
        NativeInControlResult::Null
    } else {
        NativeInControlResult::Bool(true)
    })
}
/// AST row IN owns both candidate and field traversal. Shape errors remain
/// late: a non-row is rejected only when visited; width is checked only after
/// `read` has evaluated the complete right row. The outer list is exhaustive.
pub fn native_in_ast_rows<T, E>(
    left: &[T],
    candidate_count: usize,
    mut read: impl FnMut(usize) -> Result<NativeInRowCandidate<T>, E>,
    mut compare: impl FnMut(&T, &T) -> Result<NativeInControlValue, E>,
) -> Result<NativeInControlResult, NativeInRowError<E>> {
    let mut state = Membership::default();
    for index in 0..candidate_count {
        let right = match read(index).map_err(NativeInRowError::Child)? {
            NativeInRowCandidate::NotRow => return Err(NativeInRowError::ItemMismatch),
            NativeInRowCandidate::Values(values) => values,
        };
        let compared = native_row_equality(left, &right, &mut compare)?;
        state.observe(match compared {
            NativeInControlResult::Null => NativeInControlValue::Null,
            NativeInControlResult::Bool(value) => NativeInControlValue::Int(i64::from(value)),
        });
    }
    Ok(state.finish())
}

/// Only these strict literal values belong to the prepared string-key table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeInCacheValue<'a> {
    Null,
    String(&'a [u8]),
    Bytes(&'a [u8]),
    Other,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeInCacheArg<'a> {
    Dynamic,
    Constant {
        level: u32,
        value: NativeInCacheValue<'a>,
    },
}
/// Real standard-library storage, returned as parts so existing expression
/// fields and their observable HashSet len/capacity remain unchanged.
#[derive(Clone, Debug)]
pub struct NativeInStringCache {
    pub keys: HashSet<Vec<u8>>,
    pub non_const_args: Vec<usize>,
    pub has_null: bool,
}
fn key(collation: NativeCollation, value: &[u8]) -> Vec<u8> {
    collation
        .key(value, KeyOptions::Default)
        .expect("raw supported collation key into Vec cannot fail")
}
/// Build-time metadata is read lazily in the source order. Ineligible names or
/// arities do not demand the first type; ineligible types do not resolve a
/// collator. The exact ConstLevel::STRICT integer is 2, not any nonzero level.
pub fn native_in_build_string_cache<'a>(
    name_lowercase: &str,
    arg_count: usize,
    first_type: impl FnOnce() -> Option<NativeInControlEvalType>,
    mut argument: impl FnMut(usize) -> NativeInCacheArg<'a>,
    collation: impl FnOnce() -> NativeCollation,
) -> Option<NativeInStringCache> {
    if name_lowercase != "in" || arg_count < 2 {
        return None;
    }
    if first_type() != Some(NativeInControlEvalType::String) {
        return None;
    }
    let collation = collation();
    let mut keys = HashSet::with_capacity(arg_count.saturating_sub(1));
    let mut non_const_args = Vec::new();
    let mut has_null = false;
    for index in 1..arg_count {
        match argument(index) {
            NativeInCacheArg::Constant {
                level: 2,
                value: NativeInCacheValue::String(value) | NativeInCacheValue::Bytes(value),
            } => {
                keys.insert(key(collation, value));
            }
            NativeInCacheArg::Constant {
                level: 2,
                value: NativeInCacheValue::Null,
            } => has_null = true,
            _ => non_const_args.push(index),
        }
    }
    Some(NativeInStringCache {
        keys,
        non_const_args,
        has_null,
    })
}
/// Probe the actual stored keys, never regenerated literals. Coercion always
/// runs, but collator resolution follows only a non-NULL coercion. Dynamic
/// callbacks receive the actual stored argument indices (not list ordinals).
/// A prepared hit skips work only if that original index list is empty.
pub fn native_in_prepared<'a, E>(
    left: NativeInControlValue,
    keys: &HashSet<Vec<u8>>,
    dynamic_indices: &[usize],
    has_null: bool,
    coerce: impl FnOnce() -> Result<Option<Cow<'a, [u8]>>, E>,
    collation: impl FnOnce() -> NativeCollation,
    mut compare: impl FnMut(usize) -> Result<NativeInControlValue, E>,
) -> Result<NativeInControlResult, E> {
    let mut state = Membership {
        found_null: left == NativeInControlValue::Null || has_null,
        found_match: false,
    };
    if let Some(value) = coerce()? {
        let collation = collation();
        state.found_match = if collation.can_use_raw_mem_as_key() {
            keys.contains(value.as_ref())
        } else {
            keys.contains(&key(collation, value.as_ref()))
        };
    }
    if state.found_match && dynamic_indices.is_empty() {
        return Ok(NativeInControlResult::Bool(true));
    }
    for &index in dynamic_indices {
        state.observe(compare(index)?);
    }
    Ok(state.finish())
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;
    #[test]
    fn controls_preserve_demands_late_errors_real_cache_parts_and_row_identities() {
        type E = &'static str;
        use NativeInCacheArg as A;
        use NativeInCacheValue as C;
        use NativeInControlResult as R;
        use NativeInControlValue as V;
        assert_eq!(
            native_in_ast_scalar::<E>(0, |_| unreachable!()),
            Ok(R::Bool(false))
        );
        assert_eq!(
            native_in_ready_values::<E>(V::Null, 0, |_| unreachable!()),
            Ok(R::Null)
        );
        assert_eq!(
            native_in_generic::<E>(V::Null, 0, false, |_| unreachable!()),
            Ok(R::Null)
        );
        assert_eq!(
            native_in_generic::<E>(V::Other, 0, true, |_| unreachable!()),
            Ok(R::Null)
        );
        let calls = RefCell::new(Vec::new());
        let result = native_in_ast_scalar(3, |index| {
            calls.borrow_mut().push(index);
            if index == 2 {
                Err("suffix")
            } else {
                Ok(V::Int(1))
            }
        });
        assert_eq!(result, Err("suffix"));
        assert_eq!(*calls.borrow(), [0, 1, 2]);
        calls.borrow_mut().clear();
        let result = native_in_ready_values(V::Null, 3, |index| {
            calls.borrow_mut().push(index);
            if index == 0 {
                Ok(V::Other)
            } else {
                Err("undemanded")
            }
        });
        assert_eq!(result, Ok(R::Bool(true)));
        assert_eq!(*calls.borrow(), [0]);
        calls.borrow_mut().clear();
        assert_eq!(
            native_in_generic::<E>(V::Other, 3, false, |index| {
                calls.borrow_mut().push(index);
                Ok([V::Int(1), V::Null, V::Int(0)][index])
            }),
            Ok(R::Bool(true))
        );
        assert_eq!(*calls.borrow(), [0, 1, 2]);
        assert_eq!(
            native_in_ast_scalar::<E>(1, |_| Ok(V::Int(-7))),
            Ok(R::Bool(true))
        );

        assert_eq!(
            native_row_equality::<i32, E>(&[], &[], |_, _| unreachable!()),
            Ok(R::Bool(true))
        );
        assert_eq!(
            native_row_equality::<i32, E>(&[1], &[], |_, _| unreachable!()),
            Err(NativeInRowError::WidthMismatch)
        );
        assert_eq!(
            native_row_equality::<_, E>(&[0, 1], &[0, 2], |left, _| Ok(if *left == 0 {
                V::Null
            } else {
                V::Int(0)
            })),
            Ok(R::Bool(false))
        );
        assert!(
            std::panic::catch_unwind(|| native_row_equality::<_, E>(&[1], &[1], |_, _| Ok(
                V::Other
            )))
            .is_err()
        );
        assert!(
            std::panic::catch_unwind(|| native_row_equality::<_, E>(&[1], &[1], |_, _| Ok(
                V::Int(2)
            )))
            .is_err()
        );
        let events = RefCell::new(Vec::new());
        let result = native_in_ast_rows::<_, E>(
            &[1, 2],
            2,
            |index| {
                events.borrow_mut().push(format!("read {index}"));
                Ok(NativeInRowCandidate::Values(if index == 0 {
                    vec![8, 9]
                } else {
                    vec![1, 2]
                }))
            },
            |left, right| {
                events.borrow_mut().push(format!("eq {left} {right}"));
                Ok(V::Int(i64::from(left == right)))
            },
        );
        assert_eq!(result, Ok(R::Bool(true)));
        assert_eq!(
            *events.borrow(),
            ["read 0", "eq 1 8", "read 1", "eq 1 1", "eq 2 2"]
        );
        let visited = RefCell::new(Vec::new());
        let result = native_in_ast_rows::<_, E>(
            &[1],
            2,
            |index| {
                visited.borrow_mut().push(index);
                Ok(if index == 0 {
                    NativeInRowCandidate::Values(vec![1])
                } else {
                    NativeInRowCandidate::NotRow
                })
            },
            |_, _| Ok(V::Int(1)),
        );
        assert_eq!(result, Err(NativeInRowError::ItemMismatch));
        assert_eq!(*visited.borrow(), [0, 1]);
        let fields = RefCell::new(Vec::new());
        let result = native_in_ast_rows::<_, E>(
            &[1],
            1,
            |_| {
                let values = (0..3)
                    .map(|i| {
                        fields.borrow_mut().push(i);
                        i
                    })
                    .collect();
                Ok(NativeInRowCandidate::Values(values))
            },
            |_, _| unreachable!(),
        );
        assert_eq!(result, Err(NativeInRowError::WidthMismatch));
        assert_eq!(*fields.borrow(), [0, 1, 2]);
        let result = native_in_ast_rows(
            &[1],
            2,
            |index| {
                if index == 0 {
                    Ok(NativeInRowCandidate::Values(vec![1]))
                } else {
                    Err("right-row evaluation")
                }
            },
            |_, _| Ok(V::Int(1)),
        );
        assert_eq!(result, Err(NativeInRowError::Child("right-row evaluation")));
        assert_eq!(
            native_in_ast_rows::<i32, E>(
                &[],
                1,
                |_| Ok(NativeInRowCandidate::Values(vec![])),
                |_, _| unreachable!()
            ),
            Ok(R::Bool(true))
        );
        assert_eq!(
            native_in_ast_rows::<i32, E>(&[], 0, |_| unreachable!(), |_, _| unreachable!()),
            Ok(R::Bool(false))
        );

        assert!(
            native_in_build_string_cache(
                "other",
                3,
                || panic!("type"),
                |_| panic!("argument"),
                || panic!("collator")
            )
            .is_none()
        );
        assert!(
            native_in_build_string_cache(
                "in",
                1,
                || panic!("type"),
                |_| panic!("argument"),
                || panic!("collator")
            )
            .is_none()
        );
        assert!(
            native_in_build_string_cache(
                "in",
                2,
                || None,
                |_| panic!("argument"),
                || panic!("collator")
            )
            .is_none()
        );
        assert!(
            native_in_build_string_cache(
                "in",
                2,
                || Some(NativeInControlEvalType::Int),
                |_| panic!("argument"),
                || panic!("collator")
            )
            .is_none()
        );
        events.borrow_mut().clear();
        let cache = native_in_build_string_cache(
            "in",
            8,
            || {
                events.borrow_mut().push("type".into());
                Some(NativeInControlEvalType::String)
            },
            |index| {
                events.borrow_mut().push(format!("arg {index}"));
                match index {
                    1 => A::Constant {
                        level: 2,
                        value: C::String(b"A"),
                    },
                    2 => A::Constant {
                        level: 2,
                        value: C::Bytes(b"a"),
                    },
                    3 => A::Constant {
                        level: 2,
                        value: C::Null,
                    },
                    4 => A::Constant {
                        level: 1,
                        value: C::String(b"b"),
                    },
                    5 => A::Constant {
                        level: 2,
                        value: C::Other,
                    },
                    6 => A::Dynamic,
                    7 => A::Constant {
                        level: u32::MAX,
                        value: C::Null,
                    },
                    _ => unreachable!(),
                }
            },
            || {
                events.borrow_mut().push("collator".into());
                NativeCollation::Utf8Mb4GeneralCi
            },
        )
        .unwrap();
        assert_eq!(cache.keys.len(), 1);
        assert!(cache.keys.capacity() >= 7);
        assert!(cache.has_null);
        assert_eq!(cache.non_const_args, [4, 5, 6, 7]);
        assert_eq!(
            *events.borrow(),
            [
                "type", "collator", "arg 1", "arg 2", "arg 3", "arg 4", "arg 5", "arg 6", "arg 7"
            ]
        );
        let large = native_in_build_string_cache(
            "in",
            1001,
            || Some(NativeInControlEvalType::String),
            |_| A::Constant {
                level: 2,
                value: C::String(b"same"),
            },
            || NativeCollation::Binary,
        )
        .unwrap();
        assert_eq!(large.keys.len(), 1);
        assert!(large.keys.capacity() >= 1000);

        let keys = HashSet::from([b"a".to_vec()]);
        calls.borrow_mut().clear();
        let result = native_in_prepared::<E>(
            V::Other,
            &keys,
            &[9, 2, 9],
            true,
            || Ok(Some(Cow::Borrowed(b"a"))),
            || NativeCollation::Binary,
            |index| {
                calls.borrow_mut().push(index);
                Ok(V::Null)
            },
        );
        assert_eq!(result, Ok(R::Bool(true)));
        assert_eq!(*calls.borrow(), [9, 2, 9]);
        assert_eq!(
            native_in_prepared::<E>(
                V::Other,
                &keys,
                &[],
                true,
                || Ok(Some(Cow::Borrowed(b"a"))),
                || NativeCollation::Binary,
                |_| unreachable!()
            ),
            Ok(R::Bool(true))
        );
        assert_eq!(
            native_in_prepared(
                V::Other,
                &keys,
                &[1],
                false,
                || Ok(Some(Cow::Borrowed(b"a"))),
                || NativeCollation::Binary,
                |_| Err("dynamic suffix")
            ),
            Err("dynamic suffix")
        );
        assert_eq!(
            native_in_prepared::<E>(
                V::Null,
                &keys,
                &[],
                false,
                || Ok(None),
                || panic!("NULL collation"),
                |_| unreachable!()
            ),
            Ok(R::Null)
        );
        assert_eq!(
            native_in_prepared::<E>(
                V::Other,
                &keys,
                &[],
                false,
                || Ok(None),
                || panic!("NULL collation"),
                |_| unreachable!()
            ),
            Ok(R::Bool(false))
        );
        assert_eq!(
            native_in_prepared::<E>(
                V::Other,
                &keys,
                &[],
                false,
                || Err("coercion"),
                || panic!("early collation"),
                |_| unreachable!()
            ),
            Err("coercion")
        );
        let coerced = Cell::new(false);
        assert_eq!(
            native_in_prepared::<E>(
                V::Other,
                &keys,
                &[],
                false,
                || {
                    coerced.set(true);
                    Ok(Some(Cow::Borrowed(b"a")))
                },
                || {
                    assert!(coerced.get());
                    NativeCollation::Binary
                },
                |_| unreachable!()
            ),
            Ok(R::Bool(true))
        );
        // Probe a real previously-built key with a different runtime policy;
        // no literal re-keying or replacement table is allowed at evaluation.
        let cached_key = cache.keys.iter().next().unwrap();
        assert_eq!(
            native_in_prepared::<E>(
                V::Other,
                &cache.keys,
                &[],
                false,
                || Ok(Some(Cow::Borrowed(cached_key.as_slice()))),
                || NativeCollation::Binary,
                |_| unreachable!()
            ),
            Ok(R::Bool(true))
        );
        assert_eq!(
            native_in_prepared::<E>(
                V::Other,
                &cache.keys,
                &[],
                false,
                || Ok(Some(Cow::Borrowed(b"a"))),
                || NativeCollation::Utf8Mb4GeneralCi,
                |_| unreachable!()
            ),
            Ok(R::Bool(true))
        );
    }
}
