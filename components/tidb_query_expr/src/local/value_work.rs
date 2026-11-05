// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Compatibility entrypoints for the removed Demo conversion-work allowance.
//! Values are evaluated normally; the old allowance arguments no longer meter
//! SQL conversion or introduce an additional resource-refusal policy.
use super::LocalResult;

/// No Demo conversion-work allowance is installed.
pub fn value_kernel_work_remaining() -> Option<usize> {
    None
}

/// Compatibility hook; conversion work is no longer charged against Demo caps.
pub fn charge_value_kernel_work(_amount: usize) -> LocalResult<()> {
    Ok(())
}

/// Demo work accounting no longer has a sticky refusal state.
pub fn value_kernel_work_status() -> LocalResult<()> {
    Ok(())
}

pub(crate) fn mark_value_kernel_resource_failure() {}

/// Evaluate the body without the former Demo work allowance. The signature is
/// retained for callers of the independent ready-value interfaces.
pub fn with_value_kernel_work_limit<T>(_maximum: usize, body: impl FnOnce() -> T) -> T {
    body()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_allowance_does_not_reject_conversion_or_retain_state() {
        assert_eq!(value_kernel_work_remaining(), None);
        let result = with_value_kernel_work_limit(0, || {
            charge_value_kernel_work(usize::MAX).unwrap();
            with_value_kernel_work_limit(0, || {
                charge_value_kernel_work(1).unwrap();
                42
            })
        });
        assert_eq!(result, 42);
        let panic = std::panic::catch_unwind(|| {
            with_value_kernel_work_limit(0, || panic!("original failure"));
        });
        assert!(panic.is_err());
        value_kernel_work_status().unwrap();
        assert_eq!(value_kernel_work_remaining(), None);
    }
}
