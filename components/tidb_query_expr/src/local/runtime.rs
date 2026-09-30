// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use std::{alloc::Layout, mem};

use tidb_query_datatype::{
    codec::data_type::{ChunkRef, ChunkedVec, ChunkedVecSized, Decimal, Int, VectorValue},
    expr::EvalContext,
};
use tipb::FieldType;

use super::{LocalError, LocalResult, host::LocalHostServices};

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

/// Explicit Demo policy, not a production memory/performance recommendation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionLimits {
    pub max_steps: u64,
    pub max_frame_depth: usize,
    /// Bounds active host tasks, including a potential task reserved before
    /// start even when that call ultimately returns Ready.
    pub max_active_tasks: usize,
    pub max_retained_bytes: usize,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            max_steps: u64::MAX,
            max_frame_depth: 1024,
            max_active_tasks: 256,
            max_retained_bytes: 64 * 1024 * 1024,
        }
    }
}

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

/// Measures owned element buffers in the selected accounting mode. Unsupported
/// exact carriers and unrepresentable totals use the budget's rejection
/// sentinel.
pub(crate) fn vector_storage_bytes(value: &VectorValue, mode: StorageMode) -> usize {
    match mode {
        // Preserve the old formula even for legacy callers with another carrier.
        StorageMode::ConservativeInt => int_storage_bytes(value.capacity()),
        StorageMode::ExactRetained => match value {
            VectorValue::Int(values) => int_vector_storage_bytes(values),
            VectorValue::Bytes(values) => values.retained_heap_bytes(),
            VectorValue::Decimal(values) => decimal_vector_storage_bytes(values),
            _ => None,
        }
        .unwrap_or(usize::MAX),
    }
}

/// Exact retained Int element-buffer bytes, excluding inline fields and
/// allocator bookkeeping. The bitmap getter measures Vec capacity even after
/// truncation.
pub(crate) fn int_vector_storage_bytes(values: &ChunkedVecSized<Int>) -> Option<usize> {
    values
        .capacity()
        .checked_mul(mem::size_of::<Int>())?
        .checked_add(values.get_bit_vec().retained_heap_bytes()?)
}

/// Includes every initialized Decimal's spill, even behind a NULL bitmap bit.
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

fn bitmap_min_storage_bytes(rows: usize) -> Option<usize> {
    let bits = u64::BITS as usize;
    let words = rows / bits + usize::from(rows % bits != 0);
    minimum_array_bytes::<u64>(words)
}

/// Minimum layout bytes for this row count, not a prediction of Vec capacity or
/// a bound on retained/peak allocation. Measure the actual owner after
/// reserving.
pub(crate) fn int_min_storage_bytes(rows: usize) -> Option<usize> {
    minimum_array_bytes::<Int>(rows)?.checked_add(bitmap_min_storage_bytes(rows)?)
}

/// Minimum Decimal cell/bitmap layout only; live spills are charged separately.
pub(crate) fn decimal_min_storage_bytes(rows: usize) -> Option<usize> {
    minimum_array_bytes::<Decimal>(rows)?.checked_add(bitmap_min_storage_bytes(rows)?)
}

/// Minimum data/offset/bitmap layout, including the empty vector's zero offset.
/// NULL and empty strings still consume row metadata. This is not an allocator
/// capacity prediction, peak-memory bound or pre-bound on callback payloads.
pub(crate) fn bytes_min_storage_bytes(rows: usize, data_bytes: usize) -> Option<usize> {
    minimum_array_bytes::<u8>(data_bytes)?
        .checked_add(minimum_array_bytes::<usize>(rows.checked_add(1)?)?)?
        .checked_add(bitmap_min_storage_bytes(rows)?)
}

pub(crate) struct EvalBudget {
    limits: ExecutionLimits,
    steps: u64,
    output_bytes: usize,
    checked: bool,
    mode: StorageMode,
}

impl EvalBudget {
    pub(crate) fn local(limits: ExecutionLimits, output_rows: usize) -> LocalResult<Self> {
        let result = Self {
            limits,
            steps: 0,
            output_bytes: int_storage_bytes(output_rows),
            checked: true,
            mode: StorageMode::ConservativeInt,
        };
        result.storage(0)?;
        Ok(result)
    }

    pub(crate) fn lineaged(limits: ExecutionLimits) -> LocalResult<Self> {
        Self::exact(limits)
    }

    /// Checked actual-buffer accounting with no output precharge. Execution
    /// domain and result annotations are independent driver policies.
    pub(crate) fn exact(limits: ExecutionLimits) -> LocalResult<Self> {
        let result = Self {
            limits,
            steps: 0,
            output_bytes: 0,
            checked: true,
            mode: StorageMode::ExactRetained,
        };
        result.storage(0)?;
        Ok(result)
    }

    pub(crate) fn legacy() -> Self {
        Self {
            limits: ExecutionLimits::default(),
            steps: 0,
            output_bytes: 0,
            checked: false,
            mode: StorageMode::ConservativeInt,
        }
    }

    pub(crate) fn mode(&self) -> StorageMode {
        self.mode
    }

    pub(crate) fn output_bytes(&self) -> usize {
        self.output_bytes
    }

    /// Checks a replacement output charge without adding the current output a
    /// second time. The caller includes live sources and any old/new output
    /// overlap in `scratch_bytes` before copying or proceeding to another
    /// effect.
    pub(crate) fn check_output(
        &self,
        new_output_bytes: usize,
        scratch_bytes: usize,
    ) -> LocalResult<()> {
        if self.checked
            && (new_output_bytes == usize::MAX
                || scratch_bytes == usize::MAX
                || new_output_bytes
                    .checked_add(scratch_bytes)
                    .is_none_or(|total| total > self.limits.max_retained_bytes))
        {
            return Err(LocalError::ResourceLimit(
                "retained evaluation storage exceeded".into(),
            ));
        }
        Ok(())
    }

    /// Installs an already-measured output charge after checking it. Checking
    /// simultaneously live scratch remains the caller's responsibility.
    pub(crate) fn set_output_bytes(&mut self, bytes: usize) -> LocalResult<()> {
        self.check_output(bytes, 0)?;
        self.output_bytes = bytes;
        Ok(())
    }

    pub(crate) fn is_checked(&self) -> bool {
        self.checked
    }

    pub(crate) fn charge(&mut self) -> LocalResult<()> {
        if self.checked {
            self.steps = self
                .steps
                .checked_add(1)
                .ok_or_else(|| LocalError::ResourceLimit("evaluation work overflow".into()))?;
            if self.steps > self.limits.max_steps {
                return Err(LocalError::ResourceLimit(
                    "evaluation work budget exceeded".into(),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn depth(&self, depth: usize) -> LocalResult<()> {
        if self.checked && depth > self.limits.max_frame_depth {
            return Err(LocalError::ResourceLimit(
                "evaluation frame depth exceeded".into(),
            ));
        }
        Ok(())
    }

    /// Check the active task count including the caller's potential reservation
    /// before start. Ready does not exempt start from this conservative check.
    pub(crate) fn task_count(&self, count: usize) -> LocalResult<()> {
        if self.checked && count > self.limits.max_active_tasks {
            return Err(LocalError::ResourceLimit(
                "active host task limit exceeded".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn storage(&self, bytes: usize) -> LocalResult<()> {
        if self.checked
            && (bytes == usize::MAX
                || self.output_bytes == usize::MAX
                || self
                    .output_bytes
                    .checked_add(bytes)
                    .is_none_or(|total| total > self.limits.max_retained_bytes))
        {
            return Err(LocalError::ResourceLimit(
                "retained evaluation storage exceeded".into(),
            ));
        }
        Ok(())
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
        for (value, exact) in [
            (VectorValue::Int(ints), int_bytes),
            (VectorValue::Bytes(bytes), bytes_bytes),
            (VectorValue::with_capacity(3, EvalType::Real), usize::MAX),
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

    #[test]
    fn test_lineaged_budget_checks_replacement_before_setting_output() {
        let mut budget = EvalBudget::lineaged(ExecutionLimits {
            max_steps: 0,
            max_retained_bytes: 100,
            ..ExecutionLimits::default()
        })
        .unwrap();
        assert!(budget.is_checked());
        assert_eq!(budget.mode(), StorageMode::ExactRetained);
        assert_eq!(budget.output_bytes(), 0);
        assert!(budget.check_output(64, 36).is_ok());
        assert!(matches!(
            budget.check_output(64, 37),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(budget.output_bytes(), 0);
        budget.set_output_bytes(64).unwrap();
        assert!(budget.storage(36).is_ok());
        assert!(matches!(
            budget.storage(37),
            Err(LocalError::ResourceLimit(_))
        ));
        // Replacement checks do not double-charge the currently recorded output.
        assert!(budget.check_output(0, 100).is_ok());
        for rejected in [101, usize::MAX] {
            assert!(matches!(
                budget.set_output_bytes(rejected),
                Err(LocalError::ResourceLimit(_))
            ));
            assert_eq!(budget.output_bytes(), 64);
        }
        assert!(matches!(
            budget.check_output(0, usize::MAX),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(budget.steps, 0);
        budget.set_output_bytes(0).unwrap();
        assert_eq!(budget.output_bytes(), 0);
    }

    #[test]
    fn test_conservative_budget_and_storage_overflow_policy_are_unchanged() {
        let bytes = int_storage_bytes(1);
        let budget = EvalBudget::local(
            ExecutionLimits {
                max_retained_bytes: bytes,
                ..ExecutionLimits::default()
            },
            1,
        )
        .unwrap();
        assert_eq!(budget.mode(), StorageMode::ConservativeInt);
        assert_eq!(budget.output_bytes(), bytes);
        assert!(budget.storage(0).is_ok());
        assert!(matches!(
            budget.storage(1),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(budget.steps, 0);

        let legacy = EvalBudget::legacy();
        assert_eq!(legacy.mode(), StorageMode::ConservativeInt);
        assert!(!legacy.is_checked());
        assert!(legacy.storage(usize::MAX).is_ok());
        assert!(legacy.check_output(usize::MAX, usize::MAX).is_ok());

        let budget = EvalBudget::exact(ExecutionLimits {
            max_retained_bytes: usize::MAX,
            ..ExecutionLimits::default()
        })
        .unwrap();
        assert!(budget.check_output(usize::MAX - 1, 1).is_ok());
        for (output, scratch) in [(usize::MAX, 0), (0, usize::MAX), (usize::MAX - 1, 2)] {
            assert!(matches!(
                budget.check_output(output, scratch),
                Err(LocalError::ResourceLimit(_))
            ));
        }
    }
}
