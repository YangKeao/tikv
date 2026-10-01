// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Context-keyed native regexp state shared with the actual kernel invocation.
//! Owners clone to empty caches; invocation handles explicitly share live
//! state. Binding a handle neither reads nor initializes the cache. Compiler
//! invocation remains lazy, after the caller's original
//! NULL/position/return-option gates.

use std::{
    fmt,
    sync::{Arc, RwLock},
};

use regex::{Regex, RegexBuilder};

use crate::{
    NativeReplacementPart, RegexpPolicyError, regexp_match_flags, regexp_replacement_parts,
};

#[derive(Debug)]
struct CacheItem<T> {
    context_id: u64,
    value: Arc<T>,
}

/// A single context-keyed lazy value. Ordinary constructor failures are not
/// retained; changing context replaces an entry only after successful creation.
/// The extra shared-state allocation does not promise allocation/OOM
/// equivalence.
#[derive(Debug)]
pub struct NativeContextCache<T> {
    cached: Arc<RwLock<Option<CacheItem<T>>>>,
}

impl<T> Default for NativeContextCache<T> {
    fn default() -> Self {
        Self {
            cached: Arc::new(RwLock::new(None)),
        }
    }
}

impl<T> Clone for NativeContextCache<T> {
    /// Cloning an expression owner starts with an empty cache.
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl<T> NativeContextCache<T> {
    /// Construct an empty owner, independently of any existing invocation.
    pub fn new() -> Self {
        Self::default()
    }

    fn share_state(&self) -> Self {
        Self {
            cached: Arc::clone(&self.cached),
        }
    }

    /// Observe a matching context without initializing or replacing its value.
    pub fn get_cache(&self, context_id: u64) -> Option<Arc<T>> {
        self.cached
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|item| item.context_id == context_id)
            .map(|item| Arc::clone(&item.value))
    }

    /// Preserve the original generic cache constructor contract. This closure
    /// is not carried in regexp metadata: kernels construct their own typed
    /// values, without a native callback or a precomputed-result snapshot.
    pub fn get_or_init_cache<E>(
        &self,
        context_id: u64,
        construct: impl FnOnce() -> Result<T, E>,
    ) -> Result<Arc<T>, E> {
        if let Some(value) = self.get_cache(context_id) {
            return Ok(value);
        }
        let mut cached = self
            .cached
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(item) = cached.as_ref().filter(|item| item.context_id == context_id) {
            return Ok(Arc::clone(&item.value));
        }
        let value = Arc::new(construct()?);
        *cached = Some(CacheItem { context_id, value });
        Ok(Arc::clone(
            &cached.as_ref().expect("cache item installed").value,
        ))
    }
}

/// Native compiler failures retain their actual cause; SQL frontends map these
/// variants to their original static Unsupported messages, not by parsing text.
#[derive(Clone, Debug)]
pub enum NativeRegexpCompileError {
    EmptyPattern,
    InvalidMatchType(char),
    InvalidPattern(regex::Error),
}

impl fmt::Display for NativeRegexpCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPattern => formatter.write_str("empty regular expression pattern"),
            Self::InvalidMatchType(flag) => write!(formatter, "Invalid match type: {flag}"),
            Self::InvalidPattern(error) => {
                write!(formatter, "invalid regular expression pattern: {error}")
            }
        }
    }
}

impl std::error::Error for NativeRegexpCompileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidPattern(error) => Some(error),
            _ => None,
        }
    }
}

/// Complete typed kernel error domain; no SQL classification depends on
/// Display.
#[derive(Clone, Debug)]
pub enum NativeRegexpError {
    Compile(NativeRegexpCompileError),
    Policy(RegexpPolicyError),
    InvalidReturnOption(i64),
}

impl fmt::Display for NativeRegexpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compile(error) => fmt::Display::fmt(error, formatter),
            Self::Policy(RegexpPolicyError::InvalidMatchType(flag)) => {
                write!(formatter, "Invalid match type: {flag}")
            }
            Self::Policy(RegexpPolicyError::InvalidPosition { pos, count }) => {
                write!(formatter, "Invalid pos: {pos} in regexp, count: {count}")
            }
            Self::Policy(RegexpPolicyError::InvalidSubstitution(group)) => {
                write!(formatter, "Substitution number is out of range: {group}")
            }
            Self::Policy(RegexpPolicyError::InvalidReplacementUtf8(error)) => {
                write!(formatter, "invalid UTF-8 regexp replacement: {error}")
            }
            Self::InvalidReturnOption(option) => {
                write!(formatter, "Invalid regexp return option: {option}")
            }
        }
    }
}

impl std::error::Error for NativeRegexpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Compile(error) => Some(error),
            Self::Policy(RegexpPolicyError::InvalidReplacementUtf8(error)) => Some(error),
            _ => None,
        }
    }
}

/// A successful cache construction retains either a compiled regex or its
/// compilation failure. The latter is intentionally a warm cache hit.
#[derive(Clone, Debug)]
pub struct NativeCachedRegexp {
    pub result: Result<Regex, NativeRegexpCompileError>,
}

/// Compile using the native builder representation, not wire inline flags.
/// Empty-pattern checking precedes flag scanning and syntax compilation.
pub fn compile_native_regexp(
    pattern: &str,
    match_type: &str,
) -> Result<Regex, NativeRegexpCompileError> {
    if pattern.is_empty() {
        return Err(NativeRegexpCompileError::EmptyPattern);
    }
    let flags = regexp_match_flags(match_type, false).map_err(|error| match error {
        RegexpPolicyError::InvalidMatchType(flag) => {
            NativeRegexpCompileError::InvalidMatchType(flag)
        }
        _ => unreachable!("flag scanning reports only an invalid match type"),
    })?;
    RegexBuilder::new(pattern)
        .case_insensitive(flags.contains(&'i'))
        .multi_line(flags.contains(&'m'))
        .dot_matches_new_line(flags.contains(&'s'))
        .build()
        .map_err(NativeRegexpCompileError::InvalidPattern)
}

/// Pure binary-collation statistics helper, not a pooled SQL worker. Invalid
/// UTF-8, empty patterns and compilation failures decline estimation with None.
/// This API does not make a worker-scope or resource-budget guarantee.
pub fn regexp_match_bin_collation_native(text: &[u8], pattern: &[u8]) -> Option<bool> {
    let text = std::str::from_utf8(text).ok()?;
    let pattern = std::str::from_utf8(pattern).ok()?;
    Some(compile_native_regexp(pattern, "").ok()?.is_match(text))
}

/// A typed invocation handle to the caller's actual cache owners. It contains
/// context/constness semantics, not argument values, host callbacks or answers.
#[derive(Debug)]
pub struct NativeRegexpInvocation {
    pattern_cache: NativeContextCache<NativeCachedRegexp>,
    replacement_cache: NativeContextCache<Vec<NativeReplacementPart>>,
    context_id: u64,
    cache_pattern: bool,
    cache_replacement: bool,
}

impl Clone for NativeRegexpInvocation {
    /// Unlike owner Clone, cloning an invocation explicitly shares live state.
    fn clone(&self) -> Self {
        Self {
            pattern_cache: self.pattern_cache.share_state(),
            replacement_cache: self.replacement_cache.share_state(),
            context_id: self.context_id,
            cache_pattern: self.cache_pattern,
            cache_replacement: self.cache_replacement,
        }
    }
}

impl NativeRegexpInvocation {
    /// Capture live owner handles without any lookup, context eviction or work.
    pub fn new(
        pattern_cache: &NativeContextCache<NativeCachedRegexp>,
        replacement_cache: &NativeContextCache<Vec<NativeReplacementPart>>,
        context_id: u64,
        cache_pattern: bool,
        cache_replacement: bool,
    ) -> Self {
        Self {
            pattern_cache: pattern_cache.share_state(),
            replacement_cache: replacement_cache.share_state(),
            context_id,
            cache_pattern,
            cache_replacement,
        }
    }

    /// Resolve only at the kernel's original compilation demand point.
    pub(crate) fn resolve_pattern(
        &self,
        pattern: &str,
        match_type: &str,
    ) -> Arc<NativeCachedRegexp> {
        if !self.cache_pattern {
            return Arc::new(NativeCachedRegexp {
                result: compile_native_regexp(pattern, match_type),
            });
        }
        // Return the actual borrowed cache value, including a warm error. The
        // kernel measures and uses this Arc, even if another context replaces
        // the owner's slot meanwhile; no Regex clone or second cache lookup.
        match self.pattern_cache.get_or_init_cache(self.context_id, || {
            Ok::<_, std::convert::Infallible>(NativeCachedRegexp {
                result: compile_native_regexp(pattern, match_type),
            })
        }) {
            Ok(cached) => cached,
            Err(never) => match never {},
        }
    }

    /// Resolve replacement tokens only after successful pattern resolution.
    pub(crate) fn resolve_replacement(&self, replacement: &str) -> Arc<Vec<NativeReplacementPart>> {
        if !self.cache_replacement {
            return Arc::new(regexp_replacement_parts(replacement.as_bytes()));
        }
        match self
            .replacement_cache
            .get_or_init_cache(self.context_id, || {
                Ok::<_, std::convert::Infallible>(regexp_replacement_parts(replacement.as_bytes()))
            }) {
            Ok(cached) => cached,
            Err(never) => match never {},
        }
    }
}

/// Known logical bytes for the pattern cell and the actual resolved value.
/// Owner/handle pointers are accounted with metadata, not counted again here.
/// This counts the concrete lock/slot layout and retained value, plus visible
/// syntax-error String capacity. It never estimates Regex pattern capacity,
/// compiled-engine storage, engine TLS, Arc/allocator headers or peak memory.
/// Opaque engine costs are outside this observer: cached engines are owned by
/// the statement, while uncached engines are transient. Full accounting remains
/// a deferred M6 task, not implied by a successful observation here.
pub(crate) fn known_pattern_cache_bytes(value: &Arc<NativeCachedRegexp>) -> Option<usize> {
    let base = std::mem::size_of::<RwLock<Option<CacheItem<NativeCachedRegexp>>>>()
        .checked_add(std::mem::size_of::<NativeCachedRegexp>())?;
    let syntax_capacity = match &value.result {
        Err(NativeRegexpCompileError::InvalidPattern(regex::Error::Syntax(text))) => {
            text.capacity()
        }
        _ => 0,
    };
    base.checked_add(syntax_capacity)
}

/// Known logical bytes for the replacement cell and this actual resolved Arc.
/// Capacity, including unused Vec slots, is charged; only initialized Literal
/// parts own additional buffers. No later owner/cache lookup is performed.
pub(crate) fn known_replacement_cache_bytes(
    value: &Arc<Vec<NativeReplacementPart>>,
) -> Option<usize> {
    let mut bytes = std::mem::size_of::<RwLock<Option<CacheItem<Vec<NativeReplacementPart>>>>>()
        .checked_add(std::mem::size_of::<Vec<NativeReplacementPart>>())?
        .checked_add(
            value
                .capacity()
                .checked_mul(std::mem::size_of::<NativeReplacementPart>())?,
        )?;
    for part in value.iter() {
        if let NativeReplacementPart::Literal(literal) = part {
            bytes = bytes.checked_add(literal.capacity())?;
        }
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-derived lifecycle invariants; no compiled output is used as an oracle.
    #[test]
    fn native_regexp_generic_cache_retains_old_value_on_constructor_failure() {
        let cache = NativeContextCache::<u32>::new();
        let first = cache.get_or_init_cache(7, || Ok::<_, &str>(41)).unwrap();
        let hit = cache
            .get_or_init_cache::<&str>(7, || panic!("hit must not construct"))
            .unwrap();
        assert!(Arc::ptr_eq(&first, &hit));
        assert_eq!(
            cache.get_or_init_cache(8, || Err::<u32, _>("failed")),
            Err("failed")
        );
        assert!(Arc::ptr_eq(&first, &cache.get_cache(7).unwrap()));
        assert!(cache.get_cache(8).is_none());
        assert!(cache.clone().get_cache(7).is_none());
        // Original lock-poison recovery also retains the old entry.
        assert!(
            std::panic::catch_unwind(|| {
                let _ = cache.get_or_init_cache::<&str>(8, || panic!("constructor panic"));
            })
            .is_err()
        );
        assert!(Arc::ptr_eq(&first, &cache.get_cache(7).unwrap()));
        let next = cache.get_or_init_cache(8, || Ok::<_, &str>(42)).unwrap();
        assert!(!Arc::ptr_eq(&first, &next));
        assert!(cache.get_cache(7).is_none());
        assert_eq!(*next, 42);
    }

    #[test]
    fn native_regexp_invocation_shares_lazy_warm_results_and_independent_tokens() {
        let patterns = NativeContextCache::default();
        let replacements = NativeContextCache::default();
        let invocation = NativeRegexpInvocation::new(&patterns, &replacements, 7, true, true);
        let shared = invocation.clone();
        assert!(patterns.get_cache(7).is_none());
        let error = invocation.resolve_pattern("(", "");
        assert!(matches!(
            &error.result,
            Err(NativeRegexpCompileError::InvalidPattern(_))
        ));
        assert!(Arc::ptr_eq(&error, &patterns.get_cache(7).unwrap()));
        let error_bytes = known_pattern_cache_bytes(&error).unwrap();
        // Same context deliberately ignores changed pattern and flags.
        let hit = shared.resolve_pattern("valid", "x");
        assert!(matches!(
            &hit.result,
            Err(NativeRegexpCompileError::InvalidPattern(_))
        ));
        assert!(Arc::ptr_eq(&error, &hit));
        let first = invocation.resolve_replacement("X");
        assert_eq!(
            first.as_ref(),
            &[NativeReplacementPart::Literal(b"X".to_vec())]
        );
        let NativeReplacementPart::Literal(literal) = &first[0] else {
            unreachable!()
        };
        let token_bytes =
            std::mem::size_of::<RwLock<Option<CacheItem<Vec<NativeReplacementPart>>>>>()
                + std::mem::size_of::<Vec<NativeReplacementPart>>()
                + first.capacity() * std::mem::size_of::<NativeReplacementPart>()
                + literal.capacity();
        assert_eq!(known_replacement_cache_bytes(&first), Some(token_bytes));
        assert!(Arc::ptr_eq(&first, &shared.resolve_replacement("Y")));
        let bypass = NativeRegexpInvocation::new(&patterns, &replacements, 8, false, false);
        assert_eq!(
            bypass
                .resolve_pattern("valid", "")
                .result
                .as_ref()
                .unwrap()
                .as_str(),
            "valid"
        );
        assert_eq!(
            bypass.resolve_replacement("Y").as_ref(),
            &[NativeReplacementPart::Literal(b"Y".to_vec())]
        );
        assert!(Arc::ptr_eq(&error, &patterns.get_cache(7).unwrap()));
        assert!(Arc::ptr_eq(&first, &replacements.get_cache(7).unwrap()));
        let new_context = NativeRegexpInvocation::new(&patterns, &replacements, 8, true, true);
        assert!(patterns.get_cache(7).is_some()); // construction does not evict
        assert!(patterns.clone().get_cache(7).is_none());
        assert_eq!(
            new_context
                .resolve_pattern("next", "")
                .result
                .as_ref()
                .unwrap()
                .as_str(),
            "next"
        );
        assert!(patterns.get_cache(7).is_none());
        // Measurement follows the actual old Arc, not the now-replaced slot.
        assert_eq!(known_pattern_cache_bytes(&error), Some(error_bytes));
        assert!(replacements.get_cache(7).is_some()); // independently demanded
        assert_eq!(
            new_context.resolve_replacement("Y").as_ref(),
            &[NativeReplacementPart::Literal(b"Y".to_vec())]
        );
        assert!(replacements.get_cache(7).is_none());
        assert_eq!(known_replacement_cache_bytes(&first), Some(token_bytes));
        assert_eq!(regexp_match_bin_collation_native(b"A", b"a"), Some(false));
        assert_eq!(regexp_match_bin_collation_native(b"a", b""), None);
        assert_eq!(regexp_match_bin_collation_native(b"a", b"("), None);
        assert_eq!(regexp_match_bin_collation_native(b"\xff", b"a"), None);
    }

    #[test]
    fn native_regexp_generic_cache_constructs_once_for_concurrent_context() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache = NativeContextCache::<usize>::default();
        let calls = AtomicUsize::new(0);
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        cache
                            .get_or_init_cache(9, || {
                                calls.fetch_add(1, Ordering::SeqCst);
                                Ok::<_, ()>(17)
                            })
                            .unwrap()
                    })
                })
                .collect();
            for handle in handles {
                assert!(Arc::ptr_eq(
                    &handle.join().unwrap(),
                    &cache.get_cache(9).unwrap()
                ));
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
