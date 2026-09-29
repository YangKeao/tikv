// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

use super::{ChunkRef, ChunkedVec, Evaluable, EvaluableRet, UnsafeRefInto, bit_vec::BitVec};
use crate::impl_chunked_vec_common;

/// A vector storing `Option<T>` with a compact layout.
///
/// `T` owns each stored value and may itself own heap allocations. This
/// includes `Int`, `Real`, `Decimal`, `DateTime` and `Duration` in the copr
/// framework.
///
/// Inside `ChunkedVecSized`, `bitmap` indicates if an element at a given index
/// is null, and `data` stores an initialized value. For a NULL element (or
/// `None`), the corresponding bit is false and `data` stores `T::default()`.
/// That hidden value is still owned, cloned and dropped normally, including any
/// allocations. Encoders must consult the bitmap rather than serialize the
/// hidden payload.
#[derive(Debug, PartialEq, Clone)]
pub struct ChunkedVecSized<T: Sized> {
    data: Vec<T>,
    bitmap: BitVec,
    phantom: std::marker::PhantomData<T>,
}

impl<T: Sized + Clone> ChunkedVecSized<T> {
    #[inline]
    fn get(&self, idx: usize) -> Option<&T> {
        assert!(idx < self.data.len());
        if self.bitmap.get(idx) {
            Some(&self.data[idx])
        } else {
            None
        }
    }
}

impl<T: Sized + Default> ChunkedVecSized<T> {
    /// Replaces the value at `idx` while keeping the data and validity bitmap
    /// layouts in sync.
    ///
    /// # Panics
    ///
    /// Panics if `idx` is out of bounds.
    #[inline]
    pub fn set(&mut self, idx: usize, value: Option<T>) {
        assert!(idx < self.data.len());
        match value {
            Some(value) => {
                self.data[idx] = value;
                self.bitmap.replace(idx, true);
            }
            None => {
                self.data[idx] = T::default();
                self.bitmap.replace(idx, false);
            }
        }
    }
}

impl<T: Clone + Default> ChunkedVec<T> for ChunkedVecSized<T> {
    impl_chunked_vec_common! { T }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            data: Vec::with_capacity(capacity),
            bitmap: BitVec::with_capacity(capacity),
            phantom: std::marker::PhantomData,
        }
    }

    #[inline]
    fn push_data(&mut self, value: T) {
        self.bitmap.push(true);
        self.data.push(value);
    }

    #[inline]
    fn push_null(&mut self) {
        let value = T::default();
        self.bitmap.push(false);
        self.data.push(value);
    }

    fn len(&self) -> usize {
        self.data.len()
    }

    fn truncate(&mut self, len: usize) {
        self.data.truncate(len);
        self.bitmap.truncate(len);
    }

    fn capacity(&self) -> usize {
        self.data.capacity()
    }

    fn append(&mut self, other: &mut Self) {
        self.data.append(&mut other.data);
        self.bitmap.append(&mut other.bitmap);
    }

    fn to_vec(&self) -> Vec<Option<T>> {
        let mut x = Vec::with_capacity(self.len());
        for i in 0..self.len() {
            x.push(self.get(i).cloned());
        }
        x
    }
}

impl<'a, T: Evaluable + EvaluableRet> ChunkRef<'a, &'a T> for &'a ChunkedVecSized<T> {
    #[inline]
    fn get_option_ref(self, idx: usize) -> Option<&'a T> {
        self.get(idx)
    }

    fn get_bit_vec(self) -> &'a BitVec {
        &self.bitmap
    }

    #[inline]
    fn phantom_data(self) -> Option<&'a T> {
        None
    }
}

impl<T: Clone + Default> From<Vec<Option<T>>> for ChunkedVecSized<T> {
    fn from(v: Vec<Option<T>>) -> ChunkedVecSized<T> {
        ChunkedVecSized::from_vec(v)
    }
}

impl<T: Evaluable> UnsafeRefInto<&'static ChunkedVecSized<T>> for &ChunkedVecSized<T> {
    unsafe fn unsafe_into(self) -> &'static ChunkedVecSized<T> {
        std::mem::transmute(self)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::codec::data_type::*;

    #[derive(Clone, Debug)]
    struct OwnedPayload {
        bytes: Vec<u8>,
        boxed: Box<u64>,
        drops: Arc<AtomicUsize>,
    }

    impl Default for OwnedPayload {
        fn default() -> Self {
            Self {
                bytes: vec![7],
                boxed: Box::new(7),
                drops: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl Drop for OwnedPayload {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn test_push_null_uses_default_payload() {
        // A u64-only payload is valid even under the former zeroed initializer,
        // making this regression safe before and after the fix.
        #[derive(Clone, Debug)]
        struct DefaultMarker(u64);

        impl Default for DefaultMarker {
            fn default() -> Self {
                Self(7)
            }
        }

        let mut values = ChunkedVecSized::<DefaultMarker>::with_capacity(1);
        values.push(None);

        assert_eq!(values.len(), 1);
        assert!(!values.bitmap.get(0));
        assert!(values.get(0).is_none());
        assert_eq!(
            values.data[0].0, 7,
            "NULL backing payload must use T::default(), not all-zero bytes"
        );
    }

    #[test]
    fn test_owned_payload_clone_append_truncate_and_drop() {
        let first = OwnedPayload::default();
        let first_drops = first.drops.clone();
        let mut left = ChunkedVecSized::from_vec(vec![Some(first), None]);
        let hidden_drops = left.data[1].drops.clone();
        assert!(!left.bitmap.get(1));
        assert!(left.get(1).is_none());
        assert_eq!(left.data[1].bytes, vec![7]);
        assert_eq!(*left.data[1].boxed, 7);

        let mut cloned = left.clone();
        cloned.data[0].bytes.push(9);
        *cloned.data[0].boxed = 9;
        assert_eq!(left.data[0].bytes, vec![7]);
        assert_eq!(*left.data[0].boxed, 7);
        assert!(cloned.get(1).is_none());
        drop(cloned);
        assert_eq!(first_drops.load(Ordering::SeqCst), 1);
        assert_eq!(hidden_drops.load(Ordering::SeqCst), 1);

        let last = OwnedPayload::default();
        let last_drops = last.drops.clone();
        let mut right = ChunkedVecSized::from_vec(vec![Some(last)]);
        left.append(&mut right);
        assert_eq!(left.len(), 3);
        assert!(right.is_empty());
        assert_eq!(last_drops.load(Ordering::SeqCst), 0);

        left.truncate(1);
        assert_eq!(first_drops.load(Ordering::SeqCst), 1);
        assert_eq!(hidden_drops.load(Ordering::SeqCst), 2);
        assert_eq!(last_drops.load(Ordering::SeqCst), 1);
        drop(right);
        drop(left);
        assert_eq!(first_drops.load(Ordering::SeqCst), 2);
        assert_eq!(hidden_drops.load(Ordering::SeqCst), 2);
        assert_eq!(last_drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_set_none_drops_old_payload_and_retains_default() {
        let old = OwnedPayload::default();
        let old_drops = old.drops.clone();
        let mut values = ChunkedVecSized::from_vec(vec![Some(old)]);

        values.set(0, None);
        assert_eq!(old_drops.load(Ordering::SeqCst), 1);
        assert!(!values.bitmap.get(0));
        assert!(values.get(0).is_none());
        assert_eq!(values.data[0].bytes, vec![7]);
        assert_eq!(*values.data[0].boxed, 7);
        let hidden_drops = values.data[0].drops.clone();
        assert_eq!(hidden_drops.load(Ordering::SeqCst), 0);

        // Repeated NULL replacement drops the previous hidden owned value;
        // the new Default value remains allocated until replacement or drop.
        values.set(0, None);
        assert_eq!(hidden_drops.load(Ordering::SeqCst), 1);
        let next_hidden_drops = values.data[0].drops.clone();
        assert_eq!(next_hidden_drops.load(Ordering::SeqCst), 0);

        let replacement = OwnedPayload::default();
        let replacement_drops = replacement.drops.clone();
        values.set(0, Some(replacement));
        assert_eq!(next_hidden_drops.load(Ordering::SeqCst), 1);
        assert!(values.bitmap.get(0));
        assert!(values.get(0).is_some());
        drop(values);
        assert_eq!(replacement_drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_slice_vec() {
        let test_decimal: &[Option<Decimal>] = &[
            Decimal::from_f64(1.233).ok(),
            Decimal::from_f64(2.233).ok(),
            Decimal::from_f64(3.233).ok(),
            Decimal::from_f64(4.233).ok(),
            Decimal::from_f64(5.233).ok(),
            None,
        ];
        assert_eq!(
            ChunkedVecSized::<Decimal>::from_slice(test_decimal).to_vec(),
            test_decimal
        );
        assert_eq!(
            ChunkedVecSized::<Decimal>::from_vec(test_decimal.to_vec()).to_vec(),
            test_decimal
        );
        let test_real: &[Option<Real>] = &[
            Real::new(1.01001).ok(),
            Real::new(-0.01).ok(),
            Real::new(1.02001).ok(),
            Real::new(f64::MIN).ok(),
            Real::new(f64::MAX).ok(),
            None,
        ];
        assert_eq!(
            ChunkedVecSized::<Real>::from_slice(test_real).to_vec(),
            test_real
        );
        assert_eq!(
            ChunkedVecSized::<Real>::from_vec(test_real.to_vec()).to_vec(),
            test_real
        );
        let mut ctx = EvalContext::default();
        let test_duration: &[Option<Duration>] = &[
            Duration::parse(&mut ctx, "17:51:04.78", 2).ok(),
            Duration::parse(&mut ctx, "-17:51:04.78", 2).ok(),
            Duration::parse(&mut ctx, "17:51:04.78", 0).ok(),
            Duration::parse(&mut ctx, "-17:51:04.78", 0).ok(),
            None,
        ];
        assert_eq!(
            ChunkedVecSized::<Duration>::from_slice(test_duration).to_vec(),
            test_duration
        );
        assert_eq!(
            ChunkedVecSized::<Duration>::from_vec(test_duration.to_vec()).to_vec(),
            test_duration
        );
        let test_datetime: &[Option<DateTime>] = &[
            DateTime::parse_datetime(&mut ctx, "1000-01-01 00:00:00", 0, false).ok(),
            DateTime::parse_datetime(&mut ctx, "1000-01-01 00:00:01", 0, false).ok(),
            DateTime::parse_datetime(&mut ctx, "1000-01-01 00:00:02", 0, false).ok(),
        ];
        assert_eq!(
            ChunkedVecSized::<DateTime>::from_slice(test_datetime).to_vec(),
            test_datetime
        );
        assert_eq!(
            ChunkedVecSized::<DateTime>::from_vec(test_datetime.to_vec()).to_vec(),
            test_datetime
        );
        let test_int: &[Option<Int>] =
            &[Some(1), Some(1), Some(233), Some(2333), Some(23333), None];
        assert_eq!(
            ChunkedVecSized::<Int>::from_slice(test_int).to_vec(),
            test_int
        );
        assert_eq!(
            ChunkedVecSized::<Int>::from_vec(test_int.to_vec()).to_vec(),
            test_int
        );
    }

    #[test]
    fn test_basics() {
        let mut x: ChunkedVecSized<Int> = ChunkedVecSized::with_capacity(0);
        x.push(Some(1));
        x.push(Some(2));
        x.push(Some(3));
        x.push(None);
        assert_eq!(x.get(0), Some(&1));
        assert_eq!(x.get(1), Some(&2));
        assert_eq!(x.get(2), Some(&3));
        assert_eq!(x.get(3), None);
        assert_eq!(x.len(), 4);
        assert!(!x.is_empty());
    }

    #[test]
    fn test_set() {
        let mut x = ChunkedVecSized::<Int>::from_slice(&[Some(1), None, Some(3)]);

        x.set(0, None);
        x.set(1, Some(2));
        x.set(2, Some(4));

        assert_eq!(x.to_vec(), vec![None, Some(2), Some(4)]);
        assert_eq!(
            x,
            ChunkedVecSized::<Int>::from_slice(&[None, Some(2), Some(4)])
        );
    }

    #[test]
    fn test_truncate() {
        let test_real: &[Option<Real>] = &[
            None,
            Real::new(1.01001).ok(),
            Real::new(-0.01).ok(),
            Real::new(1.02001).ok(),
            Real::new(f64::MIN).ok(),
            Real::new(f64::MAX).ok(),
            None,
        ];
        let mut chunked_vec = ChunkedVecSized::<Real>::from_slice(test_real);
        chunked_vec.truncate(100);
        assert_eq!(chunked_vec.len(), 7);
        chunked_vec.truncate(3);
        assert_eq!(chunked_vec.len(), 3);
        assert_eq!(chunked_vec.get(0), None);
        assert_eq!(chunked_vec.get(1), Real::new(1.01001).ok().as_ref());
        assert_eq!(chunked_vec.get(2), Real::new(-0.01).ok().as_ref());
        chunked_vec.truncate(0);
        assert_eq!(chunked_vec.len(), 0);
    }

    #[test]
    fn test_append() {
        let test_real_1: &[Option<Real>] = &[None, Real::new(1.01001).ok(), Real::new(-0.01).ok()];
        let test_real_2: &[Option<Real>] = &[
            Real::new(1.02001).ok(),
            Real::new(f64::MIN).ok(),
            Real::new(f64::MAX).ok(),
            None,
        ];
        let mut chunked_vec_1 = ChunkedVecSized::<Real>::from_slice(test_real_1);
        let mut chunked_vec_2 = ChunkedVecSized::<Real>::from_slice(test_real_2);
        chunked_vec_1.append(&mut chunked_vec_2);
        assert_eq!(chunked_vec_1.len(), 7);
        assert!(chunked_vec_2.is_empty());
        assert_eq!(
            chunked_vec_1.to_vec(),
            &[
                None,
                Real::new(1.01001).ok(),
                Real::new(-0.01).ok(),
                Real::new(1.02001).ok(),
                Real::new(f64::MIN).ok(),
                Real::new(f64::MAX).ok(),
                None,
            ]
        );
    }
}

#[cfg(test)]
mod benches {
    use super::*;

    #[bench]
    fn bench_append(b: &mut test::Bencher) {
        b.iter(|| {
            let mut chunked_vec_int = ChunkedVecSized::with_capacity(10000);
            for _i in 0..5000 {
                chunked_vec_int.push(Some(233));
                chunked_vec_int.push(None);
            }
        });
    }

    #[bench]
    fn bench_iterate(b: &mut test::Bencher) {
        let mut chunked_vec_int = ChunkedVecSized::with_capacity(10000);
        for _i in 0..5000 {
            chunked_vec_int.push(Some(233));
            chunked_vec_int.push(None);
        }
        b.iter(|| {
            let mut sum = 0;
            for i in 0..10000 {
                if let Some(x) = chunked_vec_int.get(i) {
                    sum += *x
                }
            }
            sum
        });
    }
}
