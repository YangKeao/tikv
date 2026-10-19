// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native LIKE policies over the shared wildcard matcher. Statement owners keep
//! their context-keyed caches; invocation handles share them without inspecting
//! or compiling a pattern until the actual kernel demands it.

use std::{borrow::Cow, convert::Infallible, mem, sync::Arc};

use tidb_query_datatype::codec::collation::{
    encoding::unicode_to_lower,
    native::NativeCollation,
    pattern::{self, CompiledPattern, MatchOptions, TrailingEscape},
};

use crate::NativeContextCache;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeLikeKind {
    Like,
    Ilike,
    Legacy,
}

#[derive(Clone, Debug)]
pub struct NativeCompiledLikePattern {
    pattern: CompiledPattern,
}

impl NativeCompiledLikePattern {
    pub fn new(pattern: &[u8], escape: u8, collation: NativeCollation) -> Self {
        Self {
            pattern: collation.compile_pattern(pattern, escape),
        }
    }

    pub fn is_match(&self, text: &[u8]) -> bool {
        self.pattern
            .is_match(text)
            .expect("raw supported LIKE comparison cannot fail")
    }

    /// Visible compiled-buffer capacities, not allocator headers or peak usage.
    pub fn retained_heap_bytes(&self) -> Option<usize> {
        self.pattern.retained_heap_bytes()
    }
}

#[derive(Clone, Debug)]
pub struct NativeCompiledIlikePattern {
    pattern: NativeCompiledLikePattern,
}

impl NativeCompiledIlikePattern {
    pub fn new(pattern: &[u8], escape: u8, collation: NativeCollation) -> Self {
        let (pattern, escape) = lower_pattern(pattern, escape);
        Self {
            pattern: NativeCompiledLikePattern::new(&pattern, escape, ilike_collation(collation)),
        }
    }

    pub fn is_match(&self, text: &[u8]) -> bool {
        let mut text = text.to_vec();
        pattern::lower_one_string(&mut text);
        self.pattern.is_match(&text)
    }

    pub fn retained_heap_bytes(&self) -> Option<usize> {
        self.pattern.retained_heap_bytes()
    }
}

// Preserve the current native ILIKE contract, rather than importing wire
// charset selection or broadening ASCII folding to Unicode case folding.
fn ilike_collation(collation: NativeCollation) -> NativeCollation {
    if collation == NativeCollation::Binary {
        NativeCollation::Binary
    } else {
        NativeCollation::Utf8Mb4Bin
    }
}

fn lower_pattern(pattern: &[u8], escape: u8) -> (Vec<u8>, u8) {
    let mut pattern = pattern.to_vec();
    let escape = if escape.is_ascii_alphabetic() {
        pattern::lower_one_string_excluding_escape_char(&mut pattern, escape)
    } else {
        pattern::lower_one_string(&mut pattern);
        escape
    };
    (pattern, escape)
}

/// Pure utility entry. It neither borrows a worker nor claims a worker receipt.
pub fn native_like_match(
    text: &[u8],
    pattern: &[u8],
    escape: u8,
    collation: NativeCollation,
) -> bool {
    collation
        .matches_pattern(text, pattern, escape)
        .expect("raw supported LIKE comparison cannot fail")
}

/// Pure ASCII-only native ILIKE policy; no precomputed host result is accepted.
pub fn native_ilike_match(
    text: &[u8],
    pattern: &[u8],
    escape: u8,
    collation: NativeCollation,
) -> bool {
    let mut text = text.to_vec();
    pattern::lower_one_string(&mut text);
    let (pattern, escape) = lower_pattern(pattern, escape);
    native_like_match(&text, &pattern, escape, ilike_collation(collation))
}

/// Preserve the legacy unistore envelope: invalid UTF-8 is empty, the selected
/// CI policy uses Go's simple Unicode lowercase, and a dangling escape rejects.
/// This is deliberately not modern collation-weighted SQL LIKE or ASCII ILIKE.
pub fn native_legacy_like_match(
    text: &[u8],
    pattern: &[u8],
    escape: u8,
    case_insensitive: bool,
) -> bool {
    let text = legacy_text(text, case_insensitive);
    let pattern = legacy_text(pattern, case_insensitive);
    pattern::matches_runes(
        text.as_bytes(),
        pattern.as_bytes(),
        MatchOptions {
            escape: u32::from(escape),
            trailing_escape: TrailingEscape::Reject,
        },
    )
}

fn legacy_text(bytes: &[u8], case_insensitive: bool) -> Cow<'_, str> {
    let text = std::str::from_utf8(bytes).unwrap_or_default();
    if case_insensitive {
        Cow::Owned(
            text.chars()
                .map(|ch| unicode_to_lower(ch).unwrap_or(ch))
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    }
}

#[derive(Debug)]
enum InvocationState {
    Like {
        collation: NativeCollation,
        cache: Option<(NativeContextCache<NativeCompiledLikePattern>, u64)>,
    },
    Ilike {
        collation: NativeCollation,
        cache: Option<(NativeContextCache<NativeCompiledIlikePattern>, u64)>,
    },
    Legacy {
        case_insensitive: bool,
    },
}

/// Typed live-cache metadata, not SQL operands, a host callback or an answer.
#[derive(Debug)]
pub struct NativeLikeInvocation {
    state: InvocationState,
}

impl Clone for NativeLikeInvocation {
    fn clone(&self) -> Self {
        // Owner Clone intentionally resets a cache; invocation Clone must not.
        match &self.state {
            InvocationState::Like { collation, cache } => Self::like(
                *collation,
                cache.as_ref().map(|(cache, context)| (cache, *context)),
            ),
            InvocationState::Ilike { collation, cache } => Self::ilike(
                *collation,
                cache.as_ref().map(|(cache, context)| (cache, *context)),
            ),
            InvocationState::Legacy { case_insensitive } => Self::legacy(*case_insensitive),
        }
    }
}

/// Failure of finite actual-value LIKE work, distinct from a non-match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeLikeValueError {
    /// Compilation, folding, retained cache or actual matcher steps exceed the
    /// allowance.
    Resource,
}

// Logical work/visible ownership, not a physical allocator or statement limit.
struct LikeWork {
    remaining: usize,
}
impl LikeWork {
    fn new(maximum: usize) -> Result<Self, NativeLikeValueError> {
        if maximum == usize::MAX {
            return Err(NativeLikeValueError::Resource);
        }
        Ok(Self { remaining: maximum })
    }
    fn take(&mut self, bytes: usize, factor: usize) -> Result<(), NativeLikeValueError> {
        self.remaining = bytes
            .checked_mul(factor)
            .and_then(|n| self.remaining.checked_sub(n))
            .ok_or(NativeLikeValueError::Resource)?;
        Ok(())
    }
    fn compiled<T>(&mut self, heap: Option<usize>) -> Result<(), NativeLikeValueError> {
        let bytes = heap
            .and_then(|n| n.checked_add(mem::size_of::<T>()))
            .ok_or(NativeLikeValueError::Resource)?;
        self.take(bytes, 1)
    }
    fn matched(
        &self,
        run: impl FnOnce(&mut usize) -> Option<bool>,
    ) -> Result<bool, NativeLikeValueError> {
        let mut steps = self.remaining / 32;
        run(&mut steps).ok_or(NativeLikeValueError::Resource)
    }
}

// Check only a bounded UTF-8 prefix before calling the original legacy policy.
// A definite first error makes the WHOLE original operand empty; the ignored
// suffix must not be copied or charged as if it were validated/lowercased.
fn legacy_text_bounded<'a>(
    bytes: &'a [u8],
    ci: bool,
    work: &mut LikeWork,
) -> Result<Cow<'a, str>, NativeLikeValueError> {
    let extent = bytes.len().min(work.remaining / 16);
    match std::str::from_utf8(&bytes[..extent]) {
        Err(error) if error.error_len().is_some() => {
            work.take(error.valid_up_to() + error.error_len().unwrap(), 16)?;
            Ok(legacy_text(&[], ci))
        }
        _ if extent < bytes.len() => Err(NativeLikeValueError::Resource),
        _ => {
            // Includes validation, a possible second validation, Unicode
            // lowering and worst-case owned UTF-8 expansion before allocation.
            work.take(bytes.len(), 16)?;
            Ok(legacy_text(bytes, ci))
        }
    }
}

impl NativeLikeInvocation {
    pub fn like(
        collation: NativeCollation,
        cache: Option<(&NativeContextCache<NativeCompiledLikePattern>, u64)>,
    ) -> Self {
        Self {
            state: InvocationState::Like {
                collation,
                cache: cache.map(|(cache, context)| (cache.share_state(), context)),
            },
        }
    }

    pub fn ilike(
        collation: NativeCollation,
        cache: Option<(&NativeContextCache<NativeCompiledIlikePattern>, u64)>,
    ) -> Self {
        Self {
            state: InvocationState::Ilike {
                collation,
                cache: cache.map(|(cache, context)| (cache.share_state(), context)),
            },
        }
    }

    pub fn legacy(case_insensitive: bool) -> Self {
        Self {
            state: InvocationState::Legacy { case_insensitive },
        }
    }

    pub fn kind(&self) -> NativeLikeKind {
        match &self.state {
            InvocationState::Like { .. } => NativeLikeKind::Like,
            InvocationState::Ilike { .. } => NativeLikeKind::Ilike,
            InvocationState::Legacy { .. } => NativeLikeKind::Legacy,
        }
    }

    /// Whether the original constant-pattern policy selected a live owner
    /// cache.
    pub fn cache_enabled(&self) -> bool {
        match &self.state {
            InvocationState::Like { cache, .. } => cache.is_some(),
            InvocationState::Ilike { cache, .. } => cache.is_some(),
            InvocationState::Legacy { .. } => false,
        }
    }

    /// Bind the actual late-demanded context id without cloning/resetting
    /// owners.
    pub fn with_context_id(mut self, context: u64) -> Self {
        match &mut self.state {
            InvocationState::Like {
                cache: Some((_, id)),
                ..
            }
            | InvocationState::Ilike {
                cache: Some((_, id)),
                ..
            } => *id = context,
            _ => {}
        }
        self
    }

    /// Evaluate actual operands with finite demanded work. Cache hits use only
    /// the held compiled Arc, even if newly supplied pattern/escape differ.
    /// A cold constructor is gated inside get_or_init_cache's lock, so a racing
    /// hit does not pay for an ignored pattern. Original comparison panics are
    /// preserved. This method neither evaluates expressions nor encodes inputs.
    pub fn evaluate_bounded(
        &self,
        text: &[u8],
        pattern: &[u8],
        escape: u8,
        maximum: usize,
    ) -> Result<bool, NativeLikeValueError> {
        let mut work = LikeWork::new(maximum)?;
        match &self.state {
            InvocationState::Like { collation, cache } => {
                if let Some((cache, context)) = cache {
                    let compiled = cache.get_or_init_cache(*context, || {
                        work.take(1024, 1)?;
                        work.take(pattern.len(), 128)?;
                        Ok::<_, NativeLikeValueError>(NativeCompiledLikePattern::new(
                            pattern, escape, *collation,
                        ))
                    })?;
                    work.compiled::<NativeCompiledLikePattern>(compiled.retained_heap_bytes())?;
                    work.matched(|steps| {
                        compiled
                            .pattern
                            .is_match_bounded(text, steps)
                            .expect("raw supported LIKE comparison cannot fail")
                    })
                } else {
                    work.matched(|steps| {
                        collation
                            .matches_pattern_bounded(text, pattern, escape, steps)
                            .expect("raw supported LIKE comparison cannot fail")
                    })
                }
            }
            InvocationState::Ilike { collation, cache } => {
                // Cached construction precedes target folding in the original.
                if let Some((cache, context)) = cache {
                    let compiled = cache.get_or_init_cache(*context, || {
                        work.take(1024, 1)?;
                        work.take(pattern.len(), 128)?;
                        Ok::<_, NativeLikeValueError>(NativeCompiledIlikePattern::new(
                            pattern, escape, *collation,
                        ))
                    })?;
                    work.compiled::<NativeCompiledIlikePattern>(compiled.retained_heap_bytes())?;
                    work.take(text.len(), 4)?;
                    let mut text = text.to_vec();
                    pattern::lower_one_string(&mut text);
                    work.matched(|steps| {
                        compiled
                            .pattern
                            .pattern
                            .is_match_bounded(&text, steps)
                            .expect("raw supported LIKE comparison cannot fail")
                    })
                } else {
                    // Uncached ILIKE folds target before pattern; retain that
                    // order and charge both allocations before each is made.
                    work.take(text.len(), 4)?;
                    let mut text = text.to_vec();
                    pattern::lower_one_string(&mut text);
                    work.take(pattern.len(), 4)?;
                    let (pattern, escape) = lower_pattern(pattern, escape);
                    work.matched(|steps| {
                        ilike_collation(*collation)
                            .matches_pattern_bounded(&text, &pattern, escape, steps)
                            .expect("raw supported LIKE comparison cannot fail")
                    })
                }
            }
            InvocationState::Legacy { case_insensitive } => {
                let text = legacy_text_bounded(text, *case_insensitive, &mut work)?;
                let pattern = legacy_text_bounded(pattern, *case_insensitive, &mut work)?;
                work.matched(|steps| {
                    pattern::matches_runes_bounded(
                        text.as_bytes(),
                        pattern.as_bytes(),
                        MatchOptions {
                            escape: u32::from(escape),
                            trailing_escape: TrailingEscape::Reject,
                        },
                        steps,
                    )
                })
            }
        }
    }

    /// Only the kernel calls this after its original NULL/demand gates. Observe
    /// the same resolved Arc that is matched, never a second cache lookup after
    /// another context may have replaced the owner slot. Known bytes cover the
    /// value and compiled buffers, not cache-lock/Arc headers or temporary
    /// folds; this is not a physical-heap or preallocation-limit guarantee.
    pub(crate) fn evaluate(
        &self,
        text: &[u8],
        pattern: &[u8],
        escape: u8,
    ) -> (bool, Option<usize>) {
        match &self.state {
            InvocationState::Like { collation, cache } => match cache {
                Some((cache, context)) => {
                    let compiled = resolve(cache, *context, || {
                        NativeCompiledLikePattern::new(pattern, escape, *collation)
                    });
                    let bytes = compiled.retained_heap_bytes().and_then(|bytes| {
                        mem::size_of::<NativeCompiledLikePattern>().checked_add(bytes)
                    });
                    (compiled.is_match(text), bytes)
                }
                None => (
                    native_like_match(text, pattern, escape, *collation),
                    Some(0),
                ),
            },
            InvocationState::Ilike { collation, cache } => match cache {
                Some((cache, context)) => {
                    let compiled = resolve(cache, *context, || {
                        NativeCompiledIlikePattern::new(pattern, escape, *collation)
                    });
                    let bytes = compiled.retained_heap_bytes().and_then(|bytes| {
                        mem::size_of::<NativeCompiledIlikePattern>().checked_add(bytes)
                    });
                    (compiled.is_match(text), bytes)
                }
                None => (
                    native_ilike_match(text, pattern, escape, *collation),
                    Some(0),
                ),
            },
            InvocationState::Legacy { case_insensitive } => (
                native_legacy_like_match(text, pattern, escape, *case_insensitive),
                Some(0),
            ),
        }
    }
}

fn resolve<T>(
    cache: &NativeContextCache<T>,
    context: u64,
    construct: impl FnOnce() -> T,
) -> Arc<T> {
    match cache.get_or_init_cache(context, || Ok::<_, Infallible>(construct())) {
        Ok(value) => value,
        Err(never) => match never {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finite_like_actual_values_match_original_all_supported_policies() {
        let words: &[&[u8]] = &[
            b"",
            b"abc",
            b"AbC",
            b"%",
            b"_",
            b"a%_",
            b"a\\%",
            b"a\\",
            b"a\0b",
            b"\xff",
            b"\xfe",
            b"\xc3",
            b"a\xffb",
            "é".as_bytes(),
            "e\u{301}".as_bytes(),
            "中".as_bytes(),
            "İ".as_bytes(),
            "ß".as_bytes(),
            b"a_a",
        ];
        let mut count = 0;
        for tag in 0..=15 {
            let collation = NativeCollation::from_tag(tag).unwrap();
            if collation == NativeCollation::Utf8Mb4ZhPinyinTiDbAsCs {
                continue;
            }
            for &text in words {
                for &pattern in words {
                    for escape in [92, 0, 37, 95, 97, 65, 255] {
                        for ilike in [false, true] {
                            let cache_like = NativeContextCache::new();
                            let cache_ilike = NativeContextCache::new();
                            let uncached = if ilike {
                                NativeLikeInvocation::ilike(collation, None)
                            } else {
                                NativeLikeInvocation::like(collation, None)
                            };
                            let cached = if ilike {
                                NativeLikeInvocation::ilike(collation, Some((&cache_ilike, 7)))
                            } else {
                                NativeLikeInvocation::like(collation, Some((&cache_like, 7)))
                            };
                            assert_eq!(
                                uncached.evaluate_bounded(text, pattern, escape, 1 << 20),
                                Ok(uncached.evaluate(text, pattern, escape).0)
                            );
                            // Original and candidate own independent cold cache slots.
                            let owner_like = NativeContextCache::new();
                            let owner_ilike = NativeContextCache::new();
                            let old_cached = if ilike {
                                NativeLikeInvocation::ilike(collation, Some((&owner_ilike, 7)))
                            } else {
                                NativeLikeInvocation::like(collation, Some((&owner_like, 7)))
                            };
                            assert_eq!(
                                cached.evaluate_bounded(text, pattern, escape, 1 << 20),
                                Ok(old_cached.evaluate(text, pattern, escape).0)
                            );
                            count += 2;
                        }
                    }
                }
            }
        }
        for &text in words {
            for &pattern in words {
                for ci in [false, true] {
                    for escape in [92, 0, 37, 95, 97, 65, 255] {
                        let invocation = NativeLikeInvocation::legacy(ci);
                        assert_eq!(
                            invocation.evaluate_bounded(text, pattern, escape, 1 << 20),
                            Ok(invocation.evaluate(text, pattern, escape).0)
                        );
                        count += 1;
                    }
                }
            }
        }
        assert_eq!(count, 156674);
    }

    #[test]
    fn finite_like_demand_cache_compilation_folding_and_backtracking() {
        use NativeLikeValueError::Resource;
        let policy = NativeCollation::Utf8Mb4Bin;
        let large = vec![b'a'; 1 << 20];
        let uncached = NativeLikeInvocation::like(policy, None);
        assert!(!uncached.cache_enabled());
        assert_eq!(uncached.evaluate_bounded(&large, b"%", 92, 32), Ok(true));
        assert_eq!(
            uncached.evaluate_bounded(&large, b"%aaab", 92, 1024),
            Err(Resource)
        );
        assert_eq!(uncached.evaluate_bounded(b"", b"", 92, 0), Ok(true));
        assert_eq!(
            uncached.evaluate_bounded(b"a", b"%", 92, usize::MAX),
            Err(Resource)
        );
        let cache = NativeContextCache::new();
        let invocation = NativeLikeInvocation::like(policy, Some((&cache, 7)));
        assert!(invocation.cache_enabled());
        assert_eq!(
            invocation.evaluate_bounded(b"a", &large, 92, 1024),
            Err(Resource)
        );
        assert!(cache.get_cache(7).is_none());
        assert_eq!(
            invocation.evaluate_bounded(b"abc", b"a%", 92, 8192),
            Ok(true)
        );
        let first = cache.get_cache(7).unwrap();
        let shared = invocation.clone();
        assert_eq!(shared.evaluate_bounded(b"abc", &large, 0, 1024), Ok(true));
        assert!(Arc::ptr_eq(&first, &cache.get_cache(7).unwrap()));
        assert!(cache.clone().get_cache(7).is_none());
        let new_context = shared.with_context_id(8);
        assert_eq!(
            new_context.evaluate_bounded(b"abc", &large, 0, 1024),
            Err(Resource)
        );
        // Failure in a cold constructor leaves the old owner's entry intact.
        assert!(Arc::ptr_eq(&first, &cache.get_cache(7).unwrap()));
        assert!(cache.get_cache(8).is_none());
        assert_eq!(
            new_context.evaluate_bounded(b"zxc", b"z%", 92, 8192),
            Ok(true)
        );
        assert!(cache.get_cache(7).is_none());
        assert!(first.is_match(b"abc"));
        let ilike = NativeLikeInvocation::ilike(policy, None);
        assert_eq!(
            ilike.evaluate_bounded(&large, b"%", 92, 1024),
            Err(Resource)
        );
        assert_eq!(
            ilike.evaluate_bounded(b"a", &large, 92, 1024),
            Err(Resource)
        );
        let legacy = NativeLikeInvocation::legacy(true);
        assert_eq!(
            legacy.evaluate_bounded(&large, b"%", 92, 1024),
            Err(Resource)
        );
        let mut invalid = large;
        invalid[0] = 255;
        // Original invalid UTF-8 becomes empty before lowering. A definite
        // short invalid prefix never demands the large ignored suffix.
        assert_eq!(legacy.evaluate_bounded(&invalid, b"", 92, 64), Ok(true));
        assert_eq!(legacy.evaluate_bounded(b"", &invalid, 92, 64), Ok(true));
        let mut incomplete = vec![b'a'; 128];
        incomplete[0] = 0xc3;
        assert_eq!(
            legacy.evaluate_bounded(&incomplete, b"", 92, 16),
            Err(Resource)
        );
        assert_eq!(legacy.evaluate_bounded(&incomplete, b"", 92, 64), Ok(true));
        assert_eq!(uncached.evaluate_bounded(b"abc", b"a%", 92, 1024), Ok(true));
    }

    #[test]
    fn finite_like_preserves_original_panic_and_recovers_poisoned_cache() {
        let cache = NativeContextCache::new();
        let invocation =
            NativeLikeInvocation::like(NativeCollation::Utf8Mb4ZhPinyinTiDbAsCs, Some((&cache, 7)));
        assert!(
            std::panic::catch_unwind(|| invocation.evaluate_bounded(b"a", b"a", 92, 8192)).is_err()
        );
        assert!(cache.get_cache(7).is_none());
        let ordinary = NativeLikeInvocation::like(NativeCollation::Utf8Mb4Bin, Some((&cache, 7)));
        assert_eq!(ordinary.evaluate_bounded(b"abc", b"a%", 92, 8192), Ok(true));
    }

    #[test]
    fn native_like_invocations_share_live_cache_but_owners_clone_empty() {
        let policy = NativeCollation::Utf8Mb4Bin;
        let cache = NativeContextCache::new();
        let invocation = NativeLikeInvocation::like(policy, Some((&cache, 7)));
        let shared = invocation.clone();
        assert!(cache.get_cache(7).is_none());
        let (matched, known_bytes) = invocation.evaluate(b"abc", b"a%", b'\\');
        assert!(matched);
        let first = cache.get_cache(7).unwrap();
        assert_eq!(
            known_bytes,
            first.retained_heap_bytes().and_then(|bytes| {
                mem::size_of::<NativeCompiledLikePattern>().checked_add(bytes)
            }),
        );
        assert!(shared.evaluate(b"abc", b"b%", b'\\').0);
        assert!(Arc::ptr_eq(&first, &cache.get_cache(7).unwrap()));
        assert!(cache.clone().get_cache(7).is_none());
        let next = NativeLikeInvocation::like(policy, Some((&cache, 8)));
        assert!(next.evaluate(b"bcd", b"b%", b'\\').0);
        assert!(cache.get_cache(7).is_none());
        assert!(first.is_match(b"abc"));
        assert_eq!(
            NativeLikeInvocation::like(policy, None).evaluate(b"abc", b"b%", b'\\'),
            (false, Some(0)),
        );

        let cache = NativeContextCache::new();
        let invocation = NativeLikeInvocation::ilike(policy, Some((&cache, 9)));
        assert!(cache.get_cache(9).is_none());
        assert!(invocation.clone().evaluate(b"ABC", b"a%", 0).0);
        assert!(invocation.evaluate(b"ABC", b"b%", 0).0);
        assert!(cache.clone().get_cache(9).is_none());
        assert!(
            NativeLikeInvocation::ilike(policy, Some((&cache, 10)))
                .evaluate(b"BCD", b"b%", 0)
                .0
        );
    }

    #[test]
    fn native_like_policies_keep_ascii_escape_and_legacy_distinctions() {
        let policy = NativeCollation::Utf8Mb4Bin;
        for (text, pat, escape, expected) in [
            ("abc", "ABC", b'a', true),
            ("abc", "ABC", b'A', false),
            ("a", "AA", b'A', true),
            ("Aa", "AAAA", b'A', true),
            ("ü", "Ü", 0, false),
            ("a_", "A\0_", 0, true),
        ] {
            assert_eq!(
                native_ilike_match(text.as_bytes(), pat.as_bytes(), escape, policy),
                expected,
            );
            assert_eq!(
                NativeCompiledIlikePattern::new(pat.as_bytes(), escape, policy)
                    .is_match(text.as_bytes()),
                expected,
            );
        }
        assert!(native_like_match(b"\\", b"\\", b'\\', policy));
        assert!(!native_legacy_like_match(b"\\", b"\\", b'\\', false));
        assert!(native_legacy_like_match(b"\xff", b"", b'\\', false));
        assert!(native_legacy_like_match(
            "İΣ".as_bytes(),
            "iσ".as_bytes(),
            b'\\',
            true,
        ));
        assert!(!native_ilike_match("İ".as_bytes(), b"i", 0, policy));
        assert!(!native_ilike_match(
            b"\xff",
            b"\xfe",
            0,
            NativeCollation::Binary,
        ));
        assert!(native_ilike_match(b"\xff", b"\xfe", 0, policy));
    }
}
