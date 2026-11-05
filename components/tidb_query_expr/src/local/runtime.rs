// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

#[cfg(test)]
use std::{alloc::Layout, mem};

#[cfg(test)]
use tidb_query_datatype::codec::data_type::{
    ChunkRef, ChunkedVec, ChunkedVecSized, DateTime, Decimal, Duration, Evaluable, EvaluableRet,
    Int, Real,
};
use tidb_query_datatype::{codec::data_type::VectorValue, expr::EvalContext};
use tipb::FieldType;

use super::{LocalResult, host::LocalHostServices};

/// An occurrence is a position in the caller's selection, not a physical row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputRow {
    pub occurrence: usize,
    pub input_row: usize,
}

/// Demand imports a bound value, never a native expression/evaluation closure.
/// The adapter and EvalContext must borrow disjoint caller-owned state.
/// Optional host services use explicit argument requests, not hidden native
/// Expr evaluation or callbacks.
pub trait LocalRuntimeServices {
    /// Static, side-effect-free layout; it must stay stable during an
    /// invocation.
    fn binding_schema(&self) -> &[FieldType];
    fn read_input(
        &mut self,
        ctx: &mut EvalContext,
        slot: usize,
        row: InputRow,
        expected: &FieldType,
    ) -> LocalResult<VectorValue>;

    /// Imports one complete occurrence map in selection order when the caller
    /// can expose a column efficiently. The returned vector owns only this
    /// invocation's compact carrier; callers may fill it from fixed-width
    /// cells or from borrowed byte slices backed by one arena. This hook is
    /// side-effect-free: demand/error ordering belongs to expression kernels,
    /// not to storage access. Adapters without a batch view retain the checked
    /// singleton import below.
    fn read_input_batch(
        &mut self,
        _ctx: &mut EvalContext,
        _slot: usize,
        _physical_rows: &[usize],
        _expected: &FieldType,
    ) -> LocalResult<Option<VectorValue>> {
        Ok(None)
    }

    /// Reborrowing is nonpanicking and side-effect-free. When hosts are bound,
    /// every reborrow during an invocation must expose the same catalog and
    /// task namespace, including during cleanup; a live provider must not
    /// disappear. The default is only for adapters with no host services.
    /// It is not a no-op cancellation implementation: host providers must
    /// implement cancel.
    fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
        None
    }
}

/// Legacy allowance arguments retained for source compatibility.
/// Demo resource policy has been removed; these fields no longer reject
/// execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionLimits {
    pub max_steps: u64,
    pub max_frame_depth: usize,
    pub max_active_tasks: usize,
    pub max_retained_bytes: usize,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            max_steps: u64::MAX,
            max_frame_depth: usize::MAX,
            max_active_tasks: usize::MAX,
            max_retained_bytes: usize::MAX,
        }
    }
}

#[cfg(test)]
pub(crate) use storage_observations::*;
#[cfg(test)]
mod storage_observations {
    use super::*;
    // Conservative heap payload charge for the admitted Int vectors, including
    // the bitmap minimum allocation/growth. Immutable program/input storage and
    // allocator bookkeeping are not part of this retained-scratch budget.
    pub(crate) fn int_storage_bytes(capacity: usize) -> usize {
        if capacity == 0 {
            return 0;
        }
        capacity
            .saturating_mul(std::mem::size_of::<i64>())
            .saturating_add(
                capacity
                    .saturating_add(63)
                    .saturating_div(64)
                    .saturating_mul(2)
                    .max(4)
                    .saturating_mul(std::mem::size_of::<u64>()),
            )
    }

    /// Accounting policy only; this does not admit additional runtime carriers.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum StorageMode {
        ConservativeInt,
        ExactRetained,
    }

    /// Measures owned element buffers in the selected accounting mode.
    /// Unsupported exact carriers and unrepresentable totals use the
    /// budget's rejection sentinel.
    pub(crate) fn vector_storage_bytes(value: &VectorValue, mode: StorageMode) -> usize {
        match mode {
            // Preserve the old formula even for legacy callers with another carrier.
            StorageMode::ConservativeInt => int_storage_bytes(value.capacity()),
            StorageMode::ExactRetained => match value {
                VectorValue::Int(values) => int_vector_storage_bytes(values),
                VectorValue::Real(values) => real_vector_storage_bytes(values),
                VectorValue::Bytes(values) => values.retained_heap_bytes(),
                VectorValue::Decimal(values) => decimal_vector_storage_bytes(values),
                VectorValue::DateTime(values) => sized_vector_storage_bytes(values),
                VectorValue::Duration(values) => sized_vector_storage_bytes(values),
                VectorValue::Json(values) => values.retained_heap_bytes(),
                VectorValue::VectorFloat32(values) => values.retained_heap_bytes(),
                VectorValue::Enum(_) | VectorValue::Set(_) => None,
            }
            .unwrap_or(usize::MAX),
        }
    }

    /// Exact retained Int element-buffer bytes, excluding inline fields and
    /// allocator bookkeeping. The bitmap getter measures Vec capacity even
    /// after truncation.
    pub(crate) fn int_vector_storage_bytes(values: &ChunkedVecSized<Int>) -> Option<usize> {
        values
            .capacity()
            .checked_mul(mem::size_of::<Int>())?
            .checked_add(values.get_bit_vec().retained_heap_bytes()?)
    }

    pub(crate) fn real_vector_storage_bytes(values: &ChunkedVecSized<Real>) -> Option<usize> {
        values
            .capacity()
            .checked_mul(mem::size_of::<Real>())?
            .checked_add(values.get_bit_vec().retained_heap_bytes()?)
    }

    fn sized_vector_storage_bytes<T>(values: &ChunkedVecSized<T>) -> Option<usize>
    where
        T: Evaluable + EvaluableRet,
    {
        values
            .capacity()
            .checked_mul(mem::size_of::<T>())?
            .checked_add(values.get_bit_vec().retained_heap_bytes()?)
    }

    /// Includes every initialized Decimal's spill, even behind a NULL bitmap
    /// bit.
    pub(crate) fn decimal_vector_storage_bytes(values: &ChunkedVecSized<Decimal>) -> Option<usize> {
        values
            .capacity()
            .checked_mul(mem::size_of::<Decimal>())?
            .checked_add(values.get_bit_vec().retained_heap_bytes()?)?
            .checked_add(values.checked_decimal_spill_bytes()?)
    }

    fn minimum_array_bytes<T>(elements: usize) -> Option<usize> {
        let bytes = elements.checked_mul(mem::size_of::<T>())?;
        Layout::array::<T>(elements).ok()?;
        Some(bytes)
    }

    pub(crate) fn bitmap_min_storage_bytes(rows: usize) -> Option<usize> {
        let bits = u64::BITS as usize;
        let words = rows / bits + usize::from(rows % bits != 0);
        minimum_array_bytes::<u64>(words)
    }

    /// Minimum layout bytes for this row count, not a prediction of Vec
    /// capacity or a bound on retained/peak allocation. Measure the actual
    /// owner after reserving.
    pub(crate) fn int_min_storage_bytes(rows: usize) -> Option<usize> {
        minimum_array_bytes::<Int>(rows)?.checked_add(bitmap_min_storage_bytes(rows)?)
    }

    /// Minimum Decimal cell/bitmap layout only; live spills are charged
    /// separately.
    pub(crate) fn decimal_min_storage_bytes(rows: usize) -> Option<usize> {
        minimum_array_bytes::<Decimal>(rows)?.checked_add(bitmap_min_storage_bytes(rows)?)
    }

    pub(crate) fn datetime_min_storage_bytes(rows: usize) -> Option<usize> {
        minimum_array_bytes::<DateTime>(rows)?.checked_add(bitmap_min_storage_bytes(rows)?)
    }

    pub(crate) fn duration_min_storage_bytes(rows: usize) -> Option<usize> {
        minimum_array_bytes::<Duration>(rows)?.checked_add(bitmap_min_storage_bytes(rows)?)
    }

    /// Minimum data/offset/bitmap layout, including the empty vector's zero
    /// offset. NULL and empty strings still consume row metadata. This is
    /// not an allocator capacity prediction, peak-memory bound or pre-bound
    /// on callback payloads.
    pub(crate) fn bytes_min_storage_bytes(rows: usize, data_bytes: usize) -> Option<usize> {
        minimum_array_bytes::<u8>(data_bytes)?
            .checked_add(minimum_array_bytes::<usize>(rows.checked_add(1)?)?)?
            .checked_add(bitmap_min_storage_bytes(rows)?)
    }
}

#[cfg(test)]
mod tests {
    use tidb_query_datatype::{EvalType, codec::data_type::ChunkedVecBytes};

    use super::*;

    #[test]
    fn test_decimal_exact_storage_includes_owned_spill() {
        let decimal = Decimal::try_from_native_digits(false, &[b'7'; 90], 0, 0, 1024).unwrap();
        let spill = decimal.spill_capacity_bytes();
        assert!(spill > 0);
        let mut values = ChunkedVecSized::<Decimal>::with_capacity(3);
        values.push(Some(decimal));
        values.push(None);
        let cells_and_bitmap = values.capacity() * mem::size_of::<Decimal>()
            + values.get_bit_vec().retained_heap_bytes().unwrap();
        assert_eq!(
            decimal_vector_storage_bytes(&values),
            Some(cells_and_bitmap + spill)
        );
        let value = VectorValue::Decimal(values);
        assert_eq!(
            vector_storage_bytes(&value, StorageMode::ExactRetained),
            cells_and_bitmap + spill
        );
        assert_eq!(
            vector_storage_bytes(&value, StorageMode::ConservativeInt),
            int_storage_bytes(value.capacity())
        );
        assert_eq!(decimal_min_storage_bytes(0), Some(0));
        assert_eq!(
            decimal_min_storage_bytes(1),
            Some(mem::size_of::<Decimal>() + mem::size_of::<u64>())
        );
        assert_eq!(decimal_min_storage_bytes(usize::MAX), None);
    }

    #[test]
    fn test_storage_modes_preserve_conservative_and_measure_lineage() {
        let mut ints = ChunkedVecSized::<Int>::with_capacity(65);
        ints.push(None);
        ints.truncate(0);
        assert_eq!(ints.get_bit_vec().capacity(), 0);
        let int_bytes = ints.capacity() * mem::size_of::<Int>()
            + ints.get_bit_vec().retained_heap_bytes().unwrap();
        assert_eq!(int_vector_storage_bytes(&ints), Some(int_bytes));
        assert!(int_bytes >= int_min_storage_bytes(65).unwrap());

        let mut bytes = ChunkedVecBytes::try_with_capacities(65, 128).unwrap();
        bytes.push_ref(Some(b"a"));
        bytes.truncate(0);
        let bytes_bytes = bytes.retained_heap_bytes().unwrap();
        assert!(bytes_bytes >= bytes_min_storage_bytes(65, 128).unwrap());
        let reals = VectorValue::with_capacity(3, EvalType::Real);
        let real_bytes = vector_storage_bytes(&reals, StorageMode::ExactRetained);
        for (value, exact) in [
            (VectorValue::Int(ints), int_bytes),
            (VectorValue::Bytes(bytes), bytes_bytes),
            (reals, real_bytes),
        ] {
            assert_eq!(
                vector_storage_bytes(&value, StorageMode::ConservativeInt),
                int_storage_bytes(value.capacity())
            );
            assert_eq!(
                vector_storage_bytes(&value, StorageMode::ExactRetained),
                exact
            );
        }
    }

    #[test]
    fn test_minimum_storage_layouts_are_checked_not_capacity_predictions() {
        for rows in [0, 1, 63, 64, 65] {
            let bitmap = (rows / 64 + usize::from(rows % 64 != 0)) * mem::size_of::<u64>();
            assert_eq!(
                int_min_storage_bytes(rows),
                Some(rows * mem::size_of::<Int>() + bitmap)
            );
            assert_eq!(
                bytes_min_storage_bytes(rows, 7),
                Some(7 + (rows + 1) * mem::size_of::<usize>() + bitmap)
            );
        }
        assert_eq!(bytes_min_storage_bytes(0, 0), Some(mem::size_of::<usize>()));
        assert_eq!(int_min_storage_bytes(usize::MAX), None);
        assert_eq!(
            int_min_storage_bytes(isize::MAX as usize / mem::size_of::<Int>() + 1),
            None
        );
        assert_eq!(bytes_min_storage_bytes(usize::MAX, 0), None);
        assert_eq!(bytes_min_storage_bytes(0, isize::MAX as usize + 1), None);
        let largest_offsets = isize::MAX as usize / mem::size_of::<usize>();
        assert_eq!(bytes_min_storage_bytes(largest_offsets, 0), None);
        assert_eq!(
            bytes_min_storage_bytes(largest_offsets - 1, isize::MAX as usize),
            None
        );
        // Only arithmetic is tested at this extent, never a large allocation.
        assert_eq!(
            bitmap_min_storage_bytes(usize::MAX),
            Some((usize::MAX / 64 + 1) * mem::size_of::<u64>())
        );
    }
}
