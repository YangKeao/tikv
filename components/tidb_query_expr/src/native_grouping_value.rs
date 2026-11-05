// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Actual GROUPING id and validated immutable marks, with original NULL demand.
use crate::GroupingMetadata;
/// Resource and missing planner metadata remain distinct.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeGroupingValueError {
    /// Finite work refused before reading mark sets.
    Resource,
    /// The original non-NULL grouping id requires initialized metadata.
    Uninitialized,
}
/// Evaluate the original pure metadata algorithm, without serialized marks.
/// NULL does not read metadata. Existing arbitrary mark counts and wrapping
/// result bits are preserved; each set lookup has a finite machine-width bound.
pub fn native_grouping_value(
    id: Option<u64>,
    metadata: Option<&GroupingMetadata>,
    maximum: usize,
) -> Result<Option<u64>, NativeGroupingValueError> {
    use NativeGroupingValueError as E;

    let Some(id) = id else { return Ok(None) };
    let metadata = metadata.ok_or(E::Uninitialized)?;
    // Admit the bounded header pass before looking at any per-set length.
    // Empty sets and the validated one-key modes do not search a maximum-depth
    // tree. Only real numeric-set searches charge their size-derived bound.
    let mut cost = metadata
        .grouping_marks()
        .len()
        .checked_mul(128)
        .ok_or(E::Resource)?;
    if cost > maximum {
        return Err(E::Resource);
    }
    if metadata.mode() == crate::GroupingMode::NumericSet {
        for mark in metadata.grouping_marks() {
            let bits = usize::BITS - mark.len().leading_zeros();
            cost = cost
                .checked_add((bits as usize).checked_mul(64).ok_or(E::Resource)?)
                .ok_or(E::Resource)?;
            if cost > maximum {
                return Err(E::Resource);
            }
        }
    }
    Ok(Some(metadata.eval(id)))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::GroupingMode as M;
    #[test]
    fn actual_grouping_keeps_null_before_metadata_wrapping_and_bounded_lookup() {
        use NativeGroupingValueError as E;
        let huge = GroupingMetadata::new(M::NumericSet, vec![(0..262144).collect::<BTreeSet<_>>()])
            .unwrap();
        assert_eq!(native_grouping_value(None, Some(&huge), 0), Ok(None));
        assert_eq!(native_grouping_value(None, None, 0), Ok(None));
        assert_eq!(
            native_grouping_value(Some(0), None, 0),
            Err(E::Uninitialized)
        );
        assert_eq!(
            native_grouping_value(None, None, usize::MAX),
            native_grouping_value(None, None, 1 << 30)
        );
        assert_eq!(
            native_grouping_value(Some(42), Some(&huge), 4096),
            Ok(Some(0))
        );
        assert_eq!(
            native_grouping_value(Some(u64::MAX), Some(&huge), 4096),
            Ok(Some(1))
        );
        assert_eq!(
            native_grouping_value(Some(0), Some(&huge), 0),
            Err(E::Resource)
        );
        for n in [0, 1, 63, 64, 65, 127] {
            let m = GroupingMetadata::new(M::NumericSet, vec![BTreeSet::new(); n]).unwrap();
            let expected = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
            assert_eq!(
                native_grouping_value(Some(0), Some(&m), 1 << 20),
                Ok(Some(expected))
            );
        }
    }
}
