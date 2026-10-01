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
