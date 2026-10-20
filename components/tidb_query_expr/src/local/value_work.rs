// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.
//! Lexical allowance for original value conversion and direct generated
//! kernels.
use super::{LocalError, LocalResult};
thread_local! {static REMAINING:std::cell::Cell<Option<usize>>=const{std::cell::Cell::new(None)};static FAILED:std::cell::Cell<bool>=const{std::cell::Cell::new(false)};}
/// Current lexical value-body allowance, absent for ordinary unscoped callers.
pub fn value_kernel_work_remaining() -> Option<usize> {
    REMAINING.with(std::cell::Cell::get)
}
/// Charge actual demanded conversion work before invoking its original owner.
pub fn charge_value_kernel_work(amount: usize) -> LocalResult<()> {
    value_kernel_work_status()?;
    REMAINING.with(|cell| match cell.get() {
        None => Ok(()),
        Some(n) => match n.checked_sub(amount) {
            Some(remaining) => {
                cell.set(Some(remaining));
                Ok(())
            }
            None => {
                mark_value_kernel_resource_failure();
                Err(resource())
            }
        },
    })
}
fn resource() -> LocalError {
    LocalError::ResourceLimit("value-body work allowance exceeded".into())
}
/// Preserve a resource refusal even if a legacy values helper catches its
/// Result.
pub fn value_kernel_work_status() -> LocalResult<()> {
    if value_kernel_work_remaining().is_some() && FAILED.with(std::cell::Cell::get) {
        Err(resource())
    } else {
        Ok(())
    }
}
pub(crate) fn mark_value_kernel_resource_failure() {
    if value_kernel_work_remaining().is_some() {
        FAILED.with(|c| c.set(true));
    }
}
/// Run a value body under the tighter current/local allowance. Unwind restores
/// the parent and retains work spent by a nested scope; scopes never select an
/// evaluator or turn resource refusal into SQL NULL.
pub fn with_value_kernel_work_limit<T>(maximum: usize, body: impl FnOnce() -> T) -> T {
    let parent = value_kernel_work_remaining();
    let parent_failed = FAILED.with(std::cell::Cell::get);
    let initial = parent.map_or(maximum, |n| n.min(maximum));
    struct Guard {
        parent: Option<usize>,
        initial: usize,
        parent_failed: bool,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let left = value_kernel_work_remaining().unwrap_or(0);
            let spent = self.initial.saturating_sub(left);
            let failed = FAILED.with(std::cell::Cell::get);
            REMAINING.with(|c| c.set(self.parent.map(|n| n.saturating_sub(spent))));
            FAILED.with(|c| c.set(self.parent.is_some() && (self.parent_failed || failed)));
        }
    }
    REMAINING.with(|c| c.set(Some(initial)));
    FAILED.with(|c| c.set(parent.is_some() && parent_failed));
    let _guard = Guard {
        parent,
        initial,
        parent_failed,
    };
    body()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scope_work_is_nested_terminal_and_restored_after_unwind() {
        assert_eq!(value_kernel_work_remaining(), None);
        with_value_kernel_work_limit(100, || {
            charge_value_kernel_work(10).unwrap();
            with_value_kernel_work_limit(20, || {
                charge_value_kernel_work(7).unwrap();
            });
            assert_eq!(value_kernel_work_remaining(), Some(83));
            let _ = std::panic::catch_unwind(|| {
                with_value_kernel_work_limit(10, || {
                    charge_value_kernel_work(3).unwrap();
                    panic!("scope unwind")
                })
            });
            assert_eq!(value_kernel_work_remaining(), Some(80));
            value_kernel_work_status().unwrap();
            assert_eq!(
                std::thread::spawn(value_kernel_work_remaining)
                    .join()
                    .unwrap(),
                None
            );
            with_value_kernel_work_limit(5, || {
                assert!(charge_value_kernel_work(6).is_err());
                assert!(value_kernel_work_status().is_err());
                assert!(charge_value_kernel_work(0).is_err());
            });
            assert!(
                value_kernel_work_status().is_err(),
                "nested refusal is sticky in parent"
            );
        });
        assert_eq!(value_kernel_work_remaining(), None);
        value_kernel_work_status().unwrap();
    }
}
#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::local::*;
    #[test]
    fn lexical_work_limit_reaches_existing_worker_before_real_dispatch_and_releases() {
        let mut worker = prepare_evaluated_bytes(
            EvaluatedBytesOp::Length,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            1 << 20,
        )
        .unwrap();
        let bytes = Vec::<u8>::with_capacity(8192);
        let maximum = bytes.capacity() - 1;
        with_value_kernel_work_limit(maximum, || {
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::Bytes(Some(bytes))),
                Err(LocalError::ResourceLimit(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(
                value_kernel_work_status().is_err(),
                "catching a worker refusal cannot authorize SQL NULL"
            );
            assert!(worker.is_healthy());
        });
        assert!(
            worker
                .eval_args(EvaluatedArgs::Bytes(Some(b"abc".to_vec())))
                .is_ok()
        );
        assert_eq!(worker.kernel_invocations(), 1);
        assert!(worker.is_healthy());
    }
}
