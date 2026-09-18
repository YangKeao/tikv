// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

use super::{ChunkRef, ChunkedVec, Set, SetRef, UnsafeRefInto, bit_vec::BitVec};
use crate::{
    codec::data_type::{ChunkedVecBytes, ChunkedVecSized, Int, retain_lifetime_transmute},
    impl_chunked_vec_common,
};

/// `ChunkedVecSet` is a vector storing `Option<Set>`.
///
/// Inside `ChunkedVecSet`:
/// - `values` stores the set bit masks.
/// - `names` stores the comma-joined element names of the sets.
///
/// # Notes
///
/// `values` and `names` both maintain a duplicated bitmap.
/// We are not able to store the data in a more compact form
/// because we have to borrow the reference of `ChunkedVecSized<Int>`
/// and `ChunkedVecBytes`. The borrowed reference should have the same
/// lifetime with `ChunkedVecSet` which cannot be achieved if we don't
/// store the owned data in struct fields.
#[derive(Debug, Clone)]
pub struct ChunkedVecSet {
    values: ChunkedVecSized<Int>,
    names: ChunkedVecBytes,
}

impl ChunkedVecSet {
    #[inline]
    pub fn get(&self, idx: usize) -> Option<SetRef<'_>> {
        assert!(idx < self.len());
        if let Some(value) = self.values.get_option_ref(idx) {
            let name = self.names.get(idx).unwrap();
            Some(SetRef::new(name, unsafe {
                retain_lifetime_transmute(value)
            }))
        } else {
            None
        }
    }

    #[inline]
    pub fn as_vec_int(&self) -> &ChunkedVecSized<Int> {
        &self.values
    }

    #[inline]
    pub fn as_vec_bytes(&self) -> &ChunkedVecBytes {
        &self.names
    }
}

impl ChunkedVec<Set> for ChunkedVecSet {
    impl_chunked_vec_common! { Set }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            values: ChunkedVecSized::<Int>::with_capacity(capacity),
            names: ChunkedVecBytes::with_capacity(capacity),
        }
    }

    #[inline]
    fn push_data(&mut self, value: Set) {
        self.values.push_data(value.value() as i64);
        self.names.push_data_ref(value.name());
    }

    #[inline]
    fn push_null(&mut self) {
        self.values.push_null();
        self.names.push_null();
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    fn truncate(&mut self, len: usize) {
        if len < self.len() {
            self.values.truncate(len);
            self.names.truncate(len);
        }
    }

    fn capacity(&self) -> usize {
        self.values.capacity().max(self.names.capacity())
    }

    fn append(&mut self, other: &mut Self) {
        self.values.append(&mut other.values);
        self.names.append(&mut other.names);
    }

    fn to_vec(&self) -> Vec<Option<Set>> {
        let mut x = Vec::with_capacity(self.len());
        for i in 0..self.len() {
            if let Some(value) = self.values.get_option_ref(i) {
                let name = self.names.get(i).unwrap().to_vec();
                x.push(Some(Set::new(name, *value as u64)));
            } else {
                x.push(None);
            }
        }
        x
    }
}

impl PartialEq for ChunkedVecSet {
    fn eq(&self, other: &Self) -> bool {
        if self.values.len() != other.values.len() {
            return false;
        }

        if !self.values.eq(&other.values) {
            return false;
        }

        if !self.names.eq(&other.names) {
            return false;
        }

        true
    }
}

impl<'a> ChunkRef<'a, SetRef<'a>> for &'a ChunkedVecSet {
    #[inline]
    fn get_option_ref(self, idx: usize) -> Option<SetRef<'a>> {
        self.get(idx)
    }

    fn get_bit_vec(self) -> &'a BitVec {
        self.values.get_bit_vec()
    }

    #[inline]
    fn phantom_data(self) -> Option<SetRef<'a>> {
        None
    }
}

impl From<Vec<Option<Set>>> for ChunkedVecSet {
    fn from(v: Vec<Option<Set>>) -> ChunkedVecSet {
        ChunkedVecSet::from_vec(v)
    }
}

impl UnsafeRefInto<&'static ChunkedVecSet> for &ChunkedVecSet {
    unsafe fn unsafe_into(self) -> &'static ChunkedVecSet {
        std::mem::transmute(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> ChunkedVecSet {
        ChunkedVecSet::with_capacity(0)
    }

    #[test]
    fn test_basics() {
        let mut x = setup();
        x.push(None);
        x.push(Some(Set::new("b,c".as_bytes().to_vec(), 0b110)));
        x.push(None);
        x.push(Some(Set::new("a".as_bytes().to_vec(), 0b001)));
        x.push(Some(Set::new("a,b".as_bytes().to_vec(), 0b011)));

        assert_eq!(x.get(0), None);
        assert_eq!(x.get(1), Some(SetRef::new(b"b,c", &0b110)));
        assert_eq!(x.get(2), None);
        assert_eq!(x.get(3), Some(SetRef::new(b"a", &0b001)));
        assert_eq!(x.get(4), Some(SetRef::new(b"a,b", &0b011)));
        assert_eq!(x.len(), 5);
        assert!(!x.is_empty());
    }

    #[test]
    fn test_push_populates_names() {
        // The element name must be installed by `push_data`, not only by tests
        // assigning the backing field directly.
        let mut x = setup();
        x.push(Some(Set::new("a,c".as_bytes().to_vec(), 0b101)));

        assert_eq!(x.get(0).unwrap().name(), b"a,c");
        assert_eq!(x.as_vec_bytes().get(0).unwrap(), b"a,c");
        assert_eq!(*x.as_vec_int().get_option_ref(0).unwrap(), 0b101);
    }

    #[test]
    fn test_truncate() {
        let mut x = setup();
        x.push(None);
        x.push(Some(Set::new("b,c".as_bytes().to_vec(), 0b110)));
        x.push(None);
        x.push(Some(Set::new("a".as_bytes().to_vec(), 0b001)));
        x.push(Some(Set::new("a,b".as_bytes().to_vec(), 0b011)));

        x.truncate(100);
        assert_eq!(x.len(), 5);

        x.truncate(3);
        assert_eq!(x.len(), 3);
        assert_eq!(x.get(0), None);
        assert_eq!(x.get(1), Some(SetRef::new(b"b,c", &0b110)));
        assert_eq!(x.get(2), None);

        x.truncate(1);
        assert_eq!(x.len(), 1);
        assert_eq!(x.get(0), None);

        x.truncate(0);
        assert_eq!(x.len(), 0);
    }

    #[test]
    fn test_append() {
        let mut x = setup();
        x.push(None);
        x.push(Some(Set::new("b,c".as_bytes().to_vec(), 0b110)));

        let mut y = setup();
        y.push(None);
        y.push(Some(Set::new("a".as_bytes().to_vec(), 0b001)));
        y.push(Some(Set::new("a,b".as_bytes().to_vec(), 0b011)));

        x.append(&mut y);
        assert_eq!(x.len(), 5);
        assert!(y.is_empty());

        assert_eq!(x.get(0), None);
        assert_eq!(x.get(1), Some(SetRef::new(b"b,c", &0b110)));
        assert_eq!(x.get(2), None);
        assert_eq!(x.get(3), Some(SetRef::new(b"a", &0b001)));
        assert_eq!(x.get(4), Some(SetRef::new(b"a,b", &0b011)));
    }

    #[test]
    fn test_to_vec() {
        let values = vec![
            None,
            Some(Set::new("b".as_bytes().to_vec(), 0b010)),
            None,
            Some(Set::new("a,b".as_bytes().to_vec(), 0b011)),
        ];
        let x = ChunkedVecSet::from(values.clone());
        assert_eq!(x.to_vec(), values);
        assert_eq!(x, ChunkedVecSet::from(values));
    }
}
