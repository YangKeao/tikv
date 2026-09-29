// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! An owned failure receipt, not a native diagnostic renderer or warning log.
//!
//! A site is captured only at the actual terminal operation error. It does not
//! identify a warning, prove native PB ingestion, or permit source inference
//! from a kernel pointer, numeric error code or error message.

use std::fmt;

use tidb_query_common::error::ErrorInner;

use super::{InputRow, LocalError, OrdinaryCallSite};

/// Exact operation identity captured at an error, without evaluation borrows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalFailureSite {
    /// An actual profile-checked ordinary kernel returned an error. `call` is
    /// the same immutable record used to prepare that kernel, not an ancestor.
    Kernel {
        call: OrdinaryCallSite,
        row: InputRow,
    },
    /// The specific read_input callback returned an error. A slot identifies a
    /// binding, not a universally unique native leaf: general producers may
    /// reuse a slot. A caller needs its own checked slot-to-source mapping.
    InputSlot { slot: usize, row: InputRow },
}

/// Failure classification. An actual operation site takes precedence over its
/// error variant, so a ResourceLimit returned by read_input is still Input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalFailureStage {
    Kernel,
    Input,
    Resource,
    Validation,
    /// An evaluation error without a checked ordinary-kernel/input site. For
    /// example, a legacy eager kernel must not be relabeled from its code/name.
    Unattributed,
}

/// The original owned error plus optional exact operation identity.
///
/// Construction is private to the runtime. No context, program, frame, input,
/// warning vector or service reference survives in this report. Its existence
/// is not a native SQL error-code/message/severity or activation guarantee.
#[derive(Debug)]
pub struct ReportedLocalFailure {
    error: LocalError,
    site: Option<LocalFailureSite>,
}

impl ReportedLocalFailure {
    pub fn error(&self) -> &LocalError {
        &self.error
    }

    /// Moves back the original error; it is not cloned, rendered or replaced.
    pub fn into_error(self) -> LocalError {
        self.error
    }

    pub fn site(&self) -> Option<&LocalFailureSite> {
        self.site.as_ref()
    }

    pub fn stage(&self) -> LocalFailureStage {
        match self.site.as_ref() {
            Some(LocalFailureSite::Kernel { .. }) => LocalFailureStage::Kernel,
            Some(LocalFailureSite::InputSlot { .. }) => LocalFailureStage::Input,
            None => match &self.error {
                LocalError::ResourceLimit(_) => LocalFailureStage::Resource,
                LocalError::InvalidSpec(_)
                | LocalError::InvalidBatch(_)
                | LocalError::BindingContract(_)
                | LocalError::HostContract(_) => LocalFailureStage::Validation,
                LocalError::Evaluation(_) => LocalFailureStage::Unattributed,
            },
        }
    }

    /// Returns the existing TiKV EvaluateError numeric code, if present.
    ///
    /// This is NOT an overflow classifier or proof of a native SQL diagnosis.
    /// In particular, an Input failure may carry 1690; Other retains the
    /// existing generic 10000. Storage/non-evaluation errors return None.
    /// Never parse or downcast a code back out of an already erased message.
    pub fn sql_error_code(&self) -> Option<i32> {
        match &self.error {
            LocalError::Evaluation(error) => match error.0.as_ref() {
                ErrorInner::Evaluate(error) => Some(error.code()),
                ErrorInner::Storage(_) => None,
            },
            _ => None,
        }
    }
}

impl fmt::Display for ReportedLocalFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

impl std::error::Error for ReportedLocalFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Fresh, stack-local, failure-only state for ONE reported invocation.
///
/// Never install this in a program, EvalState, EvalContext, TaskGuard or frame.
/// Capture only at the actual terminal Err and immediately propagate that same
/// error. Successful operations, validation/budgets, and NULL finish do not
/// capture. There must be no error recovery/retry after a capture. First-write
/// semantics protect an existing child site from an outer decoration attempt.
///
/// Current OrdinaryCallSite/InputRow fields are scalar-only, so capture adds no
/// heap allocation, work tick or fallible operation while preserving an error.
/// Revisit that invariant if those metadata types gain variable-sized fields.
#[derive(Default)]
pub(crate) struct FailureRecorder {
    site: Option<LocalFailureSite>,
}

impl FailureRecorder {
    pub(crate) fn capture_kernel(
        &mut self,
        call: &OrdinaryCallSite,
        row: InputRow,
        error: LocalError,
    ) -> LocalError {
        if self.site.is_none() {
            self.site = Some(LocalFailureSite::Kernel {
                call: call.clone(),
                row,
            });
        }
        error
    }

    pub(crate) fn capture_input(
        &mut self,
        slot: usize,
        row: InputRow,
        error: LocalError,
    ) -> LocalError {
        if self.site.is_none() {
            self.site = Some(LocalFailureSite::InputSlot { slot, row });
        }
        error
    }

    pub(crate) fn into_failure(self, error: LocalError) -> ReportedLocalFailure {
        ReportedLocalFailure {
            error,
            site: self.site,
        }
    }
}
