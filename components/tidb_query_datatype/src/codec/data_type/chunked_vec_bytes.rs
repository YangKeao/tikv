// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

use std::{alloc::Layout, mem};

use super::{Bytes, BytesRef, ChunkRef, ChunkedVec, UnsafeRefInto, bit_vec::BitVec};
use crate::{
    codec::{Error, Result},
    impl_chunked_vec_common,
};

#[cfg(test)]
std::thread_local! {
    static RESERVE_FAIL_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn check_reserve_step() -> Result<()> {
    RESERVE_FAIL_AFTER.with(|remaining| match remaining.get() {
        None => Ok(()),
        Some(0) => {
            remaining.set(None);
            Err(Error::Other(
                "injected byte-vector reservation failure".into(),
            ))
        }
        Some(steps) => {
            remaining.set(Some(steps - 1));
            Ok(())
        }
    })
}

#[derive(Debug, PartialEq, Clone)]
pub struct ChunkedVecBytes {
    data: Vec<u8>,
    bitmap: BitVec,
    length: usize,
    var_offset: Vec<usize>,
}

/// A vector storing `Option<Bytes>` with a compact layout.
///
/// Inside `ChunkedVecBytes`, `bitmap` indicates if an element at given index is
/// null, and `data` stores actual data. Bytes data are stored adjacent to each
/// other in `data`. If element at a given index is null, then it takes no space
/// in `data`. Otherwise, contents of the `Bytes` are stored, and `var_offset`
/// indicates the starting position of each element.
impl ChunkedVecBytes {
    /// Returns the retained data, offset and bitmap element-buffer bytes.
    ///
    /// Unused Vec capacity is included, even after truncation. Inline fields
    /// and allocator bookkeeping are excluded. `None` means the checked
    /// byte count cannot be represented by `usize`; this is not a total
    /// process-heap size.
    pub fn retained_heap_bytes(&self) -> Option<usize> {
        self.data
            .capacity()
            .checked_add(
                self.var_offset
                    .capacity()
                    .checked_mul(mem::size_of::<usize>())?,
            )?
            .checked_add(self.bitmap.retained_heap_bytes()?)
    }

    /// Creates an empty vector with room for `rows` values and `data_bytes`
    /// total payload bytes. NULL and empty values still need an offset and a
    /// validity bit; even an empty vector retains its initial zero offset.
    ///
    /// All counts and layouts are checked before reserving. Resource failures
    /// are `Error::Other`, not SQL numeric errors. Capacity may exceed the
    /// request: inspect `retained_heap_bytes()` before the next budgeted
    /// effect.
    pub fn try_with_capacities(rows: usize, data_bytes: usize) -> Result<Self> {
        let mut result = Self {
            data: Vec::new(),
            bitmap: BitVec::with_capacity(0),
            length: 0,
            var_offset: Vec::new(),
        };
        result.reserve_lengths(rows, data_bytes)?;
        result.var_offset.push(0);
        Ok(result)
    }

    /// Reserves room for additional `push_ref` calls without changing values or
    /// logical lengths. With no intervening capacity-consuming mutation, pushes
    /// of at most `additional_rows` values and `additional_data_bytes` total
    /// payload bytes need no buffer growth.
    ///
    /// The whole count/layout plan is checked before the first reserve. An
    /// allocation error may leave capacity from earlier successful reserves;
    /// capacity is neither exact nor rolled back. A budgeting caller must save
    /// the old charge and remeasure actual capacity before its next effect,
    /// accounting separately for live sources and old/new allocation overlap.
    /// This helper does not enforce a budget or a transient heap peak.
    pub fn try_reserve_append(
        &mut self,
        additional_rows: usize,
        additional_data_bytes: usize,
    ) -> Result<()> {
        let rows = self
            .length
            .checked_add(additional_rows)
            .ok_or_else(|| Error::Other("byte-vector reservation row count overflow".into()))?;
        let data_bytes = self
            .data
            .len()
            .checked_add(additional_data_bytes)
            .ok_or_else(|| Error::Other("byte-vector reservation data length overflow".into()))?;
        self.reserve_lengths(rows, data_bytes)
    }

    fn reserve_lengths(&mut self, rows: usize, data_bytes: usize) -> Result<()> {
        let offsets = rows
            .checked_add(1)
            .ok_or_else(|| Error::Other("byte-vector reservation offset count overflow".into()))?;
        let offset_bytes = offsets
            .max(self.var_offset.capacity())
            .checked_mul(mem::size_of::<usize>())
            .ok_or_else(|| Error::Other("byte-vector reservation offset size overflow".into()))?;
        let words = BitVec::checked_word_len(rows)?;
        let bitmap_bytes = words
            .checked_mul(mem::size_of::<u64>())
            .and_then(|bytes| Some(bytes.max(self.bitmap.retained_heap_bytes()?)))
            .ok_or_else(|| Error::Other("byte-vector reservation bitmap size overflow".into()))?;
        Layout::array::<u8>(data_bytes)
            .map_err(|_| Error::Other("byte-vector reservation data layout overflow".into()))?;
        Layout::array::<usize>(offsets)
            .map_err(|_| Error::Other("byte-vector reservation offset layout overflow".into()))?;
        // Existing capacity may already exceed a requested extent. This checks
        // representability, not an upper bound on what the allocator will grant.
        data_bytes
            .max(self.data.capacity())
            .checked_add(offset_bytes)
            .and_then(|bytes| bytes.checked_add(bitmap_bytes))
            .ok_or_else(|| Error::Other("byte-vector reservation total size overflow".into()))?;

        #[cfg(test)]
        check_reserve_step()?;
        self.data
            .try_reserve_exact(data_bytes.saturating_sub(self.data.len()))
            .map_err(|error| Error::Other(Box::new(error)))?;
        #[cfg(test)]
        check_reserve_step()?;
        self.var_offset
            .try_reserve_exact(offsets.saturating_sub(self.var_offset.len()))
            .map_err(|error| Error::Other(Box::new(error)))?;
        #[cfg(test)]
        check_reserve_step()?;
        self.bitmap.try_reserve_len(rows)?;
        self.retained_heap_bytes()
            .ok_or_else(|| Error::Other("byte-vector retained size overflow".into()))?;
        Ok(())
    }

    #[inline]
    pub fn push_data_ref(&mut self, value: BytesRef<'_>) {
        self.bitmap.push(true);
        self.data.extend_from_slice(value);
        self.finish_append();
    }

    #[inline]
    fn finish_append(&mut self) {
        self.var_offset.push(self.data.len());
        self.length += 1;
    }

    #[inline]
    pub fn push_ref(&mut self, value: Option<BytesRef<'_>>) {
        if let Some(x) = value {
            self.push_data_ref(x);
        } else {
            self.push_null();
        }
    }
    #[inline]
    pub fn get(&self, idx: usize) -> Option<BytesRef<'_>> {
        assert!(idx < self.len());
        if self.bitmap.get(idx) {
            Some(&self.data[self.var_offset[idx]..self.var_offset[idx + 1]])
        } else {
            None
        }
    }

    pub fn into_writer(self) -> BytesWriter {
        BytesWriter { chunked_vec: self }
    }
}

impl ChunkedVec<Bytes> for ChunkedVecBytes {
    impl_chunked_vec_common! { Bytes }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            data: Vec::with_capacity(capacity),
            bitmap: BitVec::with_capacity(capacity),
            var_offset: vec![0],
            length: 0,
        }
    }

    #[inline]
    fn push_data(&mut self, mut value: Bytes) {
        self.bitmap.push(true);
        self.data.append(&mut value);
        self.finish_append();
    }

    #[inline]
    fn push_null(&mut self) {
        self.bitmap.push(false);
        self.finish_append();
    }

    fn len(&self) -> usize {
        self.length
    }

    fn truncate(&mut self, len: usize) {
        if len < self.len() {
            self.data.truncate(self.var_offset[len]);
            self.bitmap.truncate(len);
            self.var_offset.truncate(len + 1);
            self.length = len;
        }
    }

    fn capacity(&self) -> usize {
        self.data.capacity().max(self.length)
    }

    fn append(&mut self, other: &mut Self) {
        self.data.append(&mut other.data);
        self.bitmap.append(&mut other.bitmap);
        let var_offset_last = *self.var_offset.last().unwrap();
        for i in 1..other.var_offset.len() {
            self.var_offset.push(other.var_offset[i] + var_offset_last);
        }
        self.length += other.length;
        other.var_offset = vec![0];
        other.length = 0;
    }

    fn to_vec(&self) -> Vec<Option<Bytes>> {
        let mut x = Vec::with_capacity(self.len());
        for i in 0..self.len() {
            x.push(self.get(i).map(|x| x.to_owned()));
        }
        x
    }
}

pub struct BytesWriter {
    chunked_vec: ChunkedVecBytes,
}

pub struct PartialBytesWriter {
    chunked_vec: ChunkedVecBytes,
}

pub struct BytesGuard {
    chunked_vec: ChunkedVecBytes,
}

impl BytesGuard {
    pub fn into_inner(self) -> ChunkedVecBytes {
        self.chunked_vec
    }
}

impl BytesWriter {
    pub fn begin(self) -> PartialBytesWriter {
        PartialBytesWriter {
            chunked_vec: self.chunked_vec,
        }
    }

    pub fn write(mut self, data: Option<Bytes>) -> BytesGuard {
        self.chunked_vec.push(data);
        BytesGuard {
            chunked_vec: self.chunked_vec,
        }
    }

    pub fn write_ref(mut self, data: Option<BytesRef<'_>>) -> BytesGuard {
        self.chunked_vec.push_ref(data);
        BytesGuard {
            chunked_vec: self.chunked_vec,
        }
    }

    pub fn write_from_char_iter(self, iter: impl Iterator<Item = char>) -> BytesGuard {
        let mut writer = self.begin();
        for c in iter {
            let mut buf = [0; 4];
            let result = c.encode_utf8(&mut buf);
            writer.partial_write(result.as_bytes());
        }
        writer.finish()
    }

    pub fn write_from_byte_iter(mut self, iter: impl Iterator<Item = u8>) -> BytesGuard {
        self.chunked_vec.data.extend(iter);
        self.chunked_vec.bitmap.push(true);
        self.chunked_vec.finish_append();
        BytesGuard {
            chunked_vec: self.chunked_vec,
        }
    }
}

impl PartialBytesWriter {
    pub fn partial_write(&mut self, data: BytesRef<'_>) {
        self.chunked_vec.data.extend_from_slice(data);
    }

    pub fn finish(mut self) -> BytesGuard {
        self.chunked_vec.bitmap.push(true);
        self.chunked_vec.finish_append();
        BytesGuard {
            chunked_vec: self.chunked_vec,
        }
    }
}

impl<'a> ChunkRef<'a, BytesRef<'a>> for &'a ChunkedVecBytes {
    #[inline]
    fn get_option_ref(self, idx: usize) -> Option<BytesRef<'a>> {
        self.get(idx)
    }

    fn get_bit_vec(self) -> &'a BitVec {
        &self.bitmap
    }

    #[inline]
    fn phantom_data(self) -> Option<BytesRef<'a>> {
        None
    }
}

impl From<Vec<Option<Bytes>>> for ChunkedVecBytes {
    fn from(v: Vec<Option<Bytes>>) -> ChunkedVecBytes {
        ChunkedVecBytes::from_vec(v)
    }
}

impl UnsafeRefInto<&'static ChunkedVecBytes> for &ChunkedVecBytes {
    unsafe fn unsafe_into(self) -> &'static ChunkedVecBytes {
        std::mem::transmute(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capacities(values: &ChunkedVecBytes) -> (usize, usize, usize) {
        (
            values.data.capacity(),
            values.var_offset.capacity(),
            values.bitmap.retained_heap_bytes().unwrap(),
        )
    }

    fn assert_retained_heap_bytes(values: &ChunkedVecBytes) {
        let (data, offsets, bitmap) = capacities(values);
        assert_eq!(
            values.retained_heap_bytes(),
            Some(data + offsets * mem::size_of::<usize>() + bitmap)
        );
        assert_eq!(values.capacity(), data.max(values.len()));
        assert_eq!(values.var_offset.len(), values.len() + 1);
        assert_eq!(values.var_offset.first(), Some(&0));
        assert_eq!(values.var_offset.last(), Some(&values.data.len()));
        assert_eq!(values.bitmap.len(), values.len());
    }

    struct FailReserve;

    impl FailReserve {
        fn after(successful_steps: usize) -> Self {
            RESERVE_FAIL_AFTER.with(|remaining| {
                assert_eq!(remaining.replace(Some(successful_steps)), None);
            });
            Self
        }
    }

    impl Drop for FailReserve {
        fn drop(&mut self) {
            RESERVE_FAIL_AFTER.with(|remaining| remaining.set(None));
        }
    }

    #[test]
    fn test_retained_heap_bytes_after_preallocate_truncate_append_and_clone() {
        let legacy = ChunkedVecBytes::with_capacity(65);
        assert_eq!(legacy.bitmap.capacity(), 0);
        assert_retained_heap_bytes(&legacy);

        let mut left = ChunkedVecBytes::try_with_capacities(65, 128).unwrap();
        assert_retained_heap_bytes(&left);
        assert_eq!(left.len(), 0);
        assert!(left.data.capacity() >= 128);
        assert!(left.var_offset.capacity() >= 66);
        let initial = capacities(&left);
        left.push_ref(Some(b"abc"));
        left.push_ref(None);
        left.push_ref(Some(b""));
        assert_eq!(capacities(&left), initial);
        assert_retained_heap_bytes(&left);
        let cloned = left.clone();
        assert_eq!(cloned, left);
        assert_retained_heap_bytes(&cloned);

        left.truncate(1);
        assert_eq!(capacities(&left), initial);
        assert_eq!(left.data.len(), 3);
        assert_retained_heap_bytes(&left);
        left.truncate(0);
        assert_eq!(capacities(&left), initial);
        assert_retained_heap_bytes(&left);

        let mut right = ChunkedVecBytes::try_with_capacities(65, 128).unwrap();
        right.push_ref(Some(b"xyz"));
        right.push_ref(None);
        let donor = capacities(&right);
        left.append(&mut right);
        assert_eq!(left.get(0), Some(b"xyz".as_slice()));
        assert_eq!(left.get(1), None);
        assert!(right.is_empty());
        assert_eq!(right.data.capacity(), donor.0);
        assert_eq!(right.bitmap.retained_heap_bytes(), Some(donor.2));
        assert_eq!(right.var_offset, vec![0]);
        assert_retained_heap_bytes(&left);
        assert_retained_heap_bytes(&right);
    }

    #[test]
    fn test_reserved_null_empty_and_bitmap_boundaries_do_not_grow() {
        let empty = ChunkedVecBytes::try_with_capacities(0, 0).unwrap();
        assert_eq!(empty.data.capacity(), 0);
        assert!(empty.retained_heap_bytes().unwrap() >= mem::size_of::<usize>());
        assert_retained_heap_bytes(&empty);

        for rows in [1, 63, 64, 65] {
            let mut values = ChunkedVecBytes::try_with_capacities(rows, 0).unwrap();
            let reserved = capacities(&values);
            for index in 0..rows {
                values.push_ref(if index % 2 == 0 { None } else { Some(b"") });
            }
            assert_eq!(capacities(&values), reserved);
            assert_eq!(values.len(), rows);
            assert_eq!(values.data.len(), 0);
            assert!(values.var_offset.iter().all(|offset| *offset == 0));
            for index in 0..rows {
                assert_eq!(
                    values.get(index),
                    if index % 2 == 0 {
                        None
                    } else {
                        Some(b"".as_slice())
                    }
                );
            }
            values.try_reserve_append(0, 0).unwrap();
            assert_eq!(capacities(&values), reserved);
            assert_retained_heap_bytes(&values);
        }
    }

    #[test]
    fn test_reservation_preflights_overflow_before_any_reserve() {
        let failure = FailReserve::after(0);
        let largest_offsets = isize::MAX as usize / mem::size_of::<usize>();
        for (rows, data_bytes) in [
            // Offsets need rows + 1.
            (usize::MAX, 0),
            // Offset byte multiplication overflows.
            (usize::MAX / mem::size_of::<usize>(), 0),
            // Representable byte count, invalid Vec layout.
            (largest_offsets, 0),
            // Invalid data Vec layout.
            (0, isize::MAX as usize + 1),
            // Valid individual layouts, aggregate size overflow.
            (largest_offsets - 1, isize::MAX as usize),
        ] {
            assert!(matches!(
                ChunkedVecBytes::try_with_capacities(rows, data_bytes),
                Err(Error::Other(_))
            ));
            RESERVE_FAIL_AFTER.with(|remaining| assert_eq!(remaining.get(), Some(0)));
        }
        drop(failure);

        let mut values = ChunkedVecBytes::try_with_capacities(1, 1).unwrap();
        values.push_ref(Some(b"a"));
        let old = values.clone();
        let old_capacity = capacities(&values);
        let _failure = FailReserve::after(0);
        for (rows, bytes) in [(usize::MAX, 0), (0, usize::MAX), (largest_offsets, 0)] {
            assert!(matches!(
                values.try_reserve_append(rows, bytes),
                Err(Error::Other(_))
            ));
            assert_eq!(values, old);
            assert_eq!(capacities(&values), old_capacity);
            RESERVE_FAIL_AFTER.with(|remaining| assert_eq!(remaining.get(), Some(0)));
        }
    }

    #[test]
    fn test_reservation_failure_preserves_values_not_capacity_rollback() {
        for failed_step in 0..3 {
            let mut values = ChunkedVecBytes::try_with_capacities(1, 1).unwrap();
            values.push_ref(Some(b"a"));
            let old = values.clone();
            let old_capacity = capacities(&values);
            let bitmap_bits = old_capacity.2 / mem::size_of::<u64>() * 64;
            let additional_rows = old_capacity.1.max(bitmap_bits) + 1;
            let additional_bytes = old_capacity.0 + 1;
            let failure = FailReserve::after(failed_step);
            assert!(matches!(
                values.try_reserve_append(additional_rows, additional_bytes),
                Err(Error::Other(_))
            ));
            RESERVE_FAIL_AFTER.with(|remaining| assert_eq!(remaining.get(), None));
            assert_eq!(values, old);
            assert_retained_heap_bytes(&values);
            let retained = capacities(&values);
            if failed_step == 0 {
                assert_eq!(retained, old_capacity);
            } else {
                assert!(retained.0 >= old.data.len() + additional_bytes);
            }
            if failed_step < 2 {
                assert_eq!(retained.1, old_capacity.1);
            } else {
                assert!(retained.1 >= old.var_offset.len() + additional_rows);
            }
            assert_eq!(retained.2, old_capacity.2);
            drop(failure);

            // Retrying reserves from unchanged lengths, not from old capacities.
            values
                .try_reserve_append(additional_rows, additional_bytes)
                .unwrap();
            let reserved = capacities(&values);
            let payload = vec![b'b'; additional_bytes];
            values.push_ref(Some(&payload));
            for _ in 1..additional_rows {
                values.push_ref(None);
            }
            assert_eq!(capacities(&values), reserved);
            assert_eq!(values.len(), old.len() + additional_rows);
            assert_eq!(values.get(0), Some(b"a".as_slice()));
            assert_eq!(values.get(1), Some(payload.as_slice()));
            assert_retained_heap_bytes(&values);
        }

        for failed_step in 0..3 {
            let _failure = FailReserve::after(failed_step);
            assert!(matches!(
                ChunkedVecBytes::try_with_capacities(65, 128),
                Err(Error::Other(_))
            ));
            RESERVE_FAIL_AFTER.with(|remaining| assert_eq!(remaining.get(), None));
        }
    }

    #[test]
    fn test_slice_vec() {
        let test_bytes: &[Option<Bytes>] = &[
            None,
            Some("我好菜啊".as_bytes().to_vec()),
            None,
            Some("我菜爆了".as_bytes().to_vec()),
            Some("我失败了".as_bytes().to_vec()),
            None,
            Some("💩".as_bytes().to_vec()),
            None,
        ];
        assert_eq!(ChunkedVecBytes::from_slice(test_bytes).to_vec(), test_bytes);
        assert_eq!(ChunkedVecBytes::from_slice(test_bytes).to_vec(), test_bytes);
    }

    #[test]
    fn test_basics() {
        let mut x: ChunkedVecBytes = ChunkedVecBytes::with_capacity(0);
        x.push(None);
        x.push(Some("我好菜啊".as_bytes().to_vec()));
        x.push(None);
        x.push(Some("我菜爆了".as_bytes().to_vec()));
        x.push(Some("我失败了".as_bytes().to_vec()));
        assert_eq!(x.get(0), None);
        assert_eq!(x.get(1), Some("我好菜啊".as_bytes()));
        assert_eq!(x.get(2), None);
        assert_eq!(x.get(3), Some("我菜爆了".as_bytes()));
        assert_eq!(x.get(4), Some("我失败了".as_bytes()));
        assert_eq!(x.len(), 5);
        assert!(!x.is_empty());
    }

    #[test]
    fn test_truncate() {
        let test_bytes: &[Option<Bytes>] = &[
            None,
            None,
            Some("我好菜啊".as_bytes().to_vec()),
            None,
            Some("我菜爆了".as_bytes().to_vec()),
            Some("我失败了".as_bytes().to_vec()),
            None,
            Some("💩".as_bytes().to_vec()),
            None,
        ];
        let mut chunked_vec = ChunkedVecBytes::from_slice(test_bytes);
        chunked_vec.truncate(100);
        assert_eq!(chunked_vec.len(), 9);
        chunked_vec.truncate(3);
        assert_eq!(chunked_vec.len(), 3);
        assert_eq!(chunked_vec.get(0), None);
        assert_eq!(chunked_vec.get(1), None);
        assert_eq!(chunked_vec.get(2), Some("我好菜啊".as_bytes()));
        chunked_vec.truncate(2);
        assert_eq!(chunked_vec.len(), 2);
        assert_eq!(chunked_vec.get(0), None);
        assert_eq!(chunked_vec.get(1), None);
        chunked_vec.truncate(1);
        assert_eq!(chunked_vec.len(), 1);
        assert_eq!(chunked_vec.get(0), None);
        chunked_vec.truncate(0);
        assert_eq!(chunked_vec.len(), 0);
    }

    #[test]
    fn test_append() {
        let test_bytes_1: &[Option<Bytes>] =
            &[None, None, Some("我好菜啊".as_bytes().to_vec()), None];
        let test_bytes_2: &[Option<Bytes>] = &[
            None,
            Some("我菜爆了".as_bytes().to_vec()),
            Some("我失败了".as_bytes().to_vec()),
            None,
            Some("💩".as_bytes().to_vec()),
            None,
        ];
        let mut chunked_vec_1 = ChunkedVecBytes::from_slice(test_bytes_1);
        let mut chunked_vec_2 = ChunkedVecBytes::from_slice(test_bytes_2);
        chunked_vec_1.append(&mut chunked_vec_2);
        assert_eq!(chunked_vec_1.len(), 10);
        assert!(chunked_vec_2.is_empty());
        assert_eq!(
            chunked_vec_1.to_vec(),
            &[
                None,
                None,
                Some("我好菜啊".as_bytes().to_vec()),
                None,
                None,
                Some("我菜爆了".as_bytes().to_vec()),
                Some("我失败了".as_bytes().to_vec()),
                None,
                Some("💩".as_bytes().to_vec()),
                None,
            ]
        );
    }

    fn repeat(data: Bytes, cnt: usize) -> Bytes {
        let mut x = vec![];
        for _ in 0..cnt {
            x.append(&mut data.clone())
        }
        x
    }

    #[test]
    fn test_writer() {
        let test_bytes: &[Option<Bytes>] = &[
            None,
            None,
            Some(
                "TiDB 是PingCAP 公司自主设计、研发的开源分布式关系型数据库，"
                    .as_bytes()
                    .to_vec(),
            ),
            None,
            Some(
                "是一款同时支持在线事务处理与在线分析处理(HTAP)的融合型分布式数据库产品。"
                    .as_bytes()
                    .to_vec(),
            ),
            Some("🐮🐮🐮🐮🐮".as_bytes().to_vec()),
            Some("我成功了".as_bytes().to_vec()),
            None,
            Some("💩💩💩".as_bytes().to_vec()),
            None,
        ];
        let mut chunked_vec = ChunkedVecBytes::with_capacity(0);
        for test_byte in test_bytes {
            let writer = chunked_vec.into_writer();
            let guard = writer.write(test_byte.to_owned());
            chunked_vec = guard.into_inner();
        }
        assert_eq!(chunked_vec.to_vec(), test_bytes);

        let mut chunked_vec = ChunkedVecBytes::with_capacity(0);
        for test_byte in test_bytes {
            let writer = chunked_vec.into_writer();
            let guard = writer.write(test_byte.clone());
            chunked_vec = guard.into_inner();
        }
        assert_eq!(chunked_vec.to_vec(), test_bytes);

        let mut chunked_vec = ChunkedVecBytes::with_capacity(0);
        for test_byte in test_bytes {
            let writer = chunked_vec.into_writer();
            let guard = match test_byte.clone() {
                Some(x) => {
                    let mut writer = writer.begin();
                    writer.partial_write(x.as_slice());
                    writer.partial_write(x.as_slice());
                    writer.partial_write(x.as_slice());
                    writer.finish()
                }
                None => writer.write(None),
            };
            chunked_vec = guard.into_inner();
        }
        assert_eq!(
            chunked_vec.to_vec(),
            test_bytes
                .iter()
                .map(|x| x.as_ref().map(|x| repeat(x.to_vec(), 3)))
                .collect::<Vec<Option<Bytes>>>()
        );
    }
}

#[cfg(test)]
mod benches {
    use super::*;

    #[bench]
    fn bench_bytes_append(b: &mut test::Bencher) {
        let mut bytes_vec: Vec<u8> = vec![];
        for _i in 0..10 {
            bytes_vec.append(&mut b"2333333333".to_vec());
        }
        b.iter(|| {
            let mut chunked_vec_bytes = ChunkedVecBytes::with_capacity(10000);
            for _i in 0..5000 {
                chunked_vec_bytes.push_data_ref(bytes_vec.as_slice());
                chunked_vec_bytes.push(None);
            }
        });
    }

    #[bench]
    fn bench_bytes_iterate(b: &mut test::Bencher) {
        let mut bytes_vec: Vec<u8> = vec![];
        for _i in 0..10 {
            bytes_vec.append(&mut b"2333333333".to_vec());
        }
        let mut chunked_vec_bytes = ChunkedVecBytes::with_capacity(10000);
        for _i in 0..5000 {
            chunked_vec_bytes.push(Some(bytes_vec.clone()));
            chunked_vec_bytes.push(None);
        }
        b.iter(|| {
            let mut sum = 0;
            for i in 0..10000 {
                if let Some(x) = chunked_vec_bytes.get(i) {
                    for i in x {
                        sum += *i as usize;
                    }
                }
            }
            sum
        });
    }
}
