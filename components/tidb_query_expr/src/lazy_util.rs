// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Small adapters shared by the hand-written lazy kernels.
//!
//! [`LazyChildren::eval`](crate::LazyChildren::eval) returns a dense owned
//! [`VectorValue`] for exactly the requested rows. Every lazy kernel moves
//! elements of those child vectors into one output vector of the declared
//! return type, so the two ends must share a representation. [`LazyValue`]
//! captures that per-type read/build pair once, instead of every kernel
//! re-deriving the concrete `ChunkedVec` type.

use tidb_query_datatype::codec::data_type::*;

/// How a lazy kernel reads one element of a child's dense result and rebuilds
/// an output vector of the same concrete type.
///
/// `GenericElem` covers every `Evaluable + EvaluableRet` type; `Bytes` and
/// `Json` are owned-result types without an `Evaluable` impl and get their own
/// adapters.
pub(crate) trait LazyValue {
    /// The owned element type the kernel moves between child and output.
    type Value;

    fn read(value: &VectorValue, row: usize) -> Option<Self::Value>;
    fn build(values: Vec<Option<Self::Value>>) -> VectorValue;
}

pub(crate) struct GenericElem<T>(std::marker::PhantomData<T>);

impl<T: Evaluable + EvaluableRet> LazyValue for GenericElem<T> {
    type Value = T;

    #[inline]
    fn read(value: &VectorValue, row: usize) -> Option<T> {
        <T as Evaluable>::borrow_scalar_value_ref(value.get_scalar_ref(row)).cloned()
    }

    #[inline]
    fn build(values: Vec<Option<T>>) -> VectorValue {
        let chunked = <<T as EvaluableRet>::ChunkedType as ChunkedVec<T>>::from_vec(values);
        T::cast_chunk_into_vector_value(chunked)
    }
}

pub(crate) struct BytesElem;

impl LazyValue for BytesElem {
    type Value = Bytes;

    #[inline]
    fn read(value: &VectorValue, row: usize) -> Option<Bytes> {
        let value: Option<BytesRef> =
            EvaluableRef::borrow_scalar_value_ref(value.get_scalar_ref(row));
        value.map(|x| x.to_vec())
    }

    #[inline]
    fn build(values: Vec<Option<Bytes>>) -> VectorValue {
        VectorValue::from(ChunkedVecBytes::from_vec(values))
    }
}

pub(crate) struct JsonElem;

impl LazyValue for JsonElem {
    type Value = Json;

    #[inline]
    fn read(value: &VectorValue, row: usize) -> Option<Json> {
        let value: Option<JsonRef> =
            EvaluableRef::borrow_scalar_value_ref(value.get_scalar_ref(row));
        value.map(|x| x.to_owned())
    }

    #[inline]
    fn build(values: Vec<Option<Json>>) -> VectorValue {
        VectorValue::from(ChunkedVecJson::from_vec(values))
    }
}

/// A dense `rows`-element output vector of NULLs; unlike `vec![None; rows]` it
/// does not require `T: Clone`.
pub(crate) fn null_output<T>(rows: usize) -> Vec<Option<T>> {
    let mut output = Vec::with_capacity(rows);
    output.resize_with(rows, || None);
    output
}

/// Reads one `Int` operand (a condition, an index or a boolean operand) out of
/// a dense child result. `None` is SQL NULL.
#[inline]
pub(crate) fn int_at(value: &VectorValue, row: usize) -> Option<i64> {
    <Int as Evaluable>::borrow_scalar_value_ref(value.get_scalar_ref(row)).copied()
}
