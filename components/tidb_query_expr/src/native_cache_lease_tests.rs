// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Compare an owned initialization reservation against the still-active lock
//! owner.
use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use super::NativeContextCache;
#[derive(Debug)]
struct OriginalCacheItem<T> {
    context_id: u64,
    value: Arc<T>,
}

/// A single context-keyed lazy value. Ordinary constructor failures are not
/// retained; changing context replaces an entry only after successful creation.
/// The extra shared-state allocation does not promise allocation/OOM
/// equivalence.
#[derive(Debug)]
struct OriginalCache<T> {
    cached: Arc<RwLock<Option<OriginalCacheItem<T>>>>,
}

impl<T> Default for OriginalCache<T> {
    fn default() -> Self {
        Self {
            cached: Arc::new(RwLock::new(None)),
        }
    }
}

impl<T> Clone for OriginalCache<T> {
    /// Cloning an expression owner starts with an empty cache.
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl<T> OriginalCache<T> {
    /// Construct an empty owner, independently of any existing invocation.
    pub fn new() -> Self {
        Self::default()
    }

    /// Share this live cache owner for one prepared value-body invocation.
    /// This does not inspect or initialize cached values. Ordinary expression
    /// ownership cloning still starts empty through Clone.
    pub fn share_for_invocation(&self) -> Self {
        self.share_state()
    }

    pub(crate) fn share_state(&self) -> Self {
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
        *cached = Some(OriginalCacheItem { context_id, value });
        Ok(Arc::clone(
            &cached.as_ref().expect("cache item installed").value,
        ))
    }
}

trait Backend: Clone + Send + Sync + 'static {
    fn new() -> Self;
    fn shared(&self) -> Self;
    fn get(&self, id: u64) -> Option<Arc<u64>>;
    fn init<E>(&self, id: u64, construct: impl FnOnce() -> Result<u64, E>) -> Result<Arc<u64>, E>;
}
impl Backend for OriginalCache<u64> {
    fn new() -> Self {
        OriginalCache::new()
    }
    fn shared(&self) -> Self {
        self.share_for_invocation()
    }
    fn get(&self, id: u64) -> Option<Arc<u64>> {
        self.get_cache(id)
    }
    fn init<E>(&self, id: u64, construct: impl FnOnce() -> Result<u64, E>) -> Result<Arc<u64>, E> {
        self.get_or_init_cache(id, construct)
    }
}
impl Backend for NativeContextCache<u64> {
    fn new() -> Self {
        NativeContextCache::new()
    }
    fn shared(&self) -> Self {
        self.share_for_invocation()
    }
    fn get(&self, id: u64) -> Option<Arc<u64>> {
        NativeContextCache::get_cache(self, id)
    }
    fn init<E>(&self, id: u64, construct: impl FnOnce() -> Result<u64, E>) -> Result<Arc<u64>, E> {
        NativeContextCache::get_or_init_cache(self, id, construct)
    }
}
fn sequence<C: Backend>(mut code: usize) -> Vec<String> {
    let cache = C::new();
    let mut observations = vec![];
    let mut held = vec![];
    let mut calls = 0u64;
    for step in 0..5 {
        let action = code % 6;
        code /= 6;
        let id = (action / 2) as u64;
        let fail = action % 2 == 1;
        let result = cache.init(id, || {
            calls += 1;
            if fail {
                Err("constructor SQL failure")
            } else {
                Ok(100 * id + 10 * calls + step)
            }
        });
        observations.push(format!("{step}/{id}/{fail}:{result:?}/calls:{calls}"));
        if let Ok(value) = result {
            held.push(value);
        }
        for key in 0..3 {
            let value = cache.get(key);
            let shared = cache.shared().get(key);
            observations.push(format!(
                "{key}:{value:?}/sharedsame:{}/emptyclone:{}",
                match (&value, &shared) {
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    (None, None) => true,
                    _ => false,
                },
                cache.clone().get(key).is_none()
            ));
        }
        observations.push(format!(
            "held:{:?}",
            held.iter().map(|v| **v).collect::<Vec<_>>()
        ));
    }
    observations
}
#[test]
fn cache_initialization_owned_candidate_matches_independent_original_sequences() {
    let (mut count, mut hash) = (0usize, 0xcbf29ce484222325u64);
    for code in 0..6usize.pow(5) {
        let original = sequence::<OriginalCache<u64>>(code);
        let candidate = sequence::<NativeContextCache<u64>>(code);
        assert_eq!(candidate, original, "constructor/context sequence {code}");
        for byte in format!("{code}:{original:?}").bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
        count += 1;
    }
    eprintln!("CACHE LEASE INDEPENDENT ORIGINAL {count}/{hash:016X}");
    assert_eq!((count, hash), (7776, 0xB7F093DF39181199));
}
fn reader_during_construction<C: Backend>(mode: usize) -> Vec<String> {
    let cache = C::new();
    let old = cache.init(7, || Ok::<_, &str>(41)).unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let builder = cache.shared();
    let thread = std::thread::spawn(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            builder.init(8, || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                match mode {
                    0 => Ok(42),
                    1 => Err("failed"),
                    _ => panic!("constructor panic"),
                }
            })
        }))
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let reader = cache.shared();
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let read_thread = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        done_tx.send(reader.get(7)).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(
            done_rx.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "matching old-context reads must wait for the whole constructor"
    );
    release_tx.send(()).unwrap();
    let result = thread.join().unwrap();
    let observed = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    read_thread.join().unwrap();
    let status = match result {
        Ok(Ok(value)) => format!("ok:{value}"),
        Ok(Err(error)) => format!("err:{error}"),
        Err(_) => "panic".into(),
    };
    let same = observed
        .as_ref()
        .is_some_and(|value| Arc::ptr_eq(value, &old));
    let snapshot = format!(
        "{status}/old:{observed:?}/same:{same}/next:{:?}/held:{}",
        cache.get(8),
        *old
    );
    let recovered = cache.init(9, || Ok::<_, &str>(99)).unwrap();
    vec![
        snapshot,
        format!("recovered:{recovered}/previous:{:?}", cache.get(8)),
    ]
}
fn follower_during_construction<C: Backend>(mode: usize) -> Vec<String> {
    let cache = C::new();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let first = cache.shared();
    let thread = std::thread::spawn(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            first.init(8, || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                match mode {
                    0 => Ok(42),
                    1 => Err("failed"),
                    _ => panic!("constructor panic"),
                }
            })
        }))
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let follower = cache.shared();
    let calls = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&calls);
    let (start_tx, start_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let waiting = std::thread::spawn(move || {
        start_tx.send(()).unwrap();
        let value = follower.init(8, || {
            called.fetch_add(1, Ordering::SeqCst);
            Ok::<_, &str>(99)
        });
        done_tx.send(value).unwrap();
    });
    start_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        done_rx.recv_timeout(Duration::from_millis(20)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "constructor ownership must precede its child effects"
    );
    release_tx.send(()).unwrap();
    let result = thread.join().unwrap();
    let next = done_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    waiting.join().unwrap();
    let status = match result {
        Ok(Ok(value)) => format!("ok:{value}"),
        Ok(Err(error)) => format!("err:{error}"),
        Err(_) => "panic".into(),
    };
    vec![format!(
        "{status}/follower:{next}/constructors:{}/same:{}",
        calls.load(Ordering::SeqCst),
        Arc::ptr_eq(&next, &cache.get(8).unwrap())
    )]
}
#[test]
fn cache_initialization_owned_candidate_matches_original_blocking_error_and_panic() {
    let mut count = 0;
    for mode in 0..3 {
        assert_eq!(
            reader_during_construction::<NativeContextCache<u64>>(mode),
            reader_during_construction::<OriginalCache<u64>>(mode)
        );
        assert_eq!(
            follower_during_construction::<NativeContextCache<u64>>(mode),
            follower_during_construction::<OriginalCache<u64>>(mode)
        );
        count += 2;
    }
    eprintln!("CACHE LEASE INDEPENDENT CONCURRENT ORIGINAL {count}");
    assert_eq!(count, 6);
}
#[test]
fn cache_initialization_owned_candidate_abort_and_commit_have_no_replay_or_eviction() {
    let cache = NativeContextCache::new();
    let old = cache.get_or_init_cache(7, || Ok::<_, &str>(41)).unwrap();
    let super::NativeCacheAccess::Initialize(lease) = cache.begin_initialization(8) else {
        panic!("new context must reserve")
    };
    let follower = cache.share_for_invocation();
    let (start_tx, start_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        start_tx.send(()).unwrap();
        done_tx.send(follower.get_cache(7)).unwrap();
    });
    start_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        done_rx.recv_timeout(Duration::from_millis(20)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    drop(lease);
    assert!(Arc::ptr_eq(
        &old,
        &done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()
    ));
    thread.join().unwrap();
    let super::NativeCacheAccess::Initialize(lease) = cache.begin_initialization(8) else {
        panic!("cancelled initialization cannot be cached")
    };
    let committed = lease.commit(42);
    assert!(cache.get_cache(7).is_none());
    assert!(Arc::ptr_eq(&committed, &cache.get_cache(8).unwrap()));
    let super::NativeCacheAccess::Hit(hit) = cache.begin_initialization(8) else {
        panic!("successful commit must publish one shared value")
    };
    assert!(Arc::ptr_eq(&committed, &hit));
}
