// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Checked in-process construction over the official RPN evaluator.
//!
//! Legacy admission is exact signed LongLong, the seed arithmetic/NULLIF
//! kernels, explicit strict integer controls, and catalog-bound host calls.
//! Bound values and host arguments are imported/evaluated on demand by the
//! official driver. Existing ordinary calls remain eager postfix. The separate
//! `compile_local_profiled` entry admits only exact signed PlusInt203 under
//! explicit TypedRow/PbRow assertions and identity-conversion left NULL-stop.
//! It does not admit AST/argument-major batch profiles or supply native source-
//! shaped diagnostics. Other domains remain gated. Callers must not pre-convert
//! unvisited branches or hide native expression evaluation in services.
//! The optional reported binding entry owns the unchanged error and an exact
//! failing input/ordinary-kernel site; it is not native diagnostic adaptation.
//! `compile_control_with_lineage` separately admits SQL TypedRow Int/Bytes
//! selection controls and signed Int booleans with required result metadata
//! IDs. It does not compose ordinary arithmetic or hosts, infer native/PB
//! origin, or widen the old entrypoints. Its caller owns the matching
//! materialization table and validates fixed native kind/collation contracts at
//! each demanded read.
//! `compile_numeric_batch` is another closed entry: signed LongLong PlusInt203
//! under a whole-source SQL numeric-batch assertion, with complete left then
//! complete right demand before per-lane parent kernels. Its opaque binding
//! facade does not compose controls, hosts or lineage, expose a row fallback,
//! or authenticate native source/consumer origin.
//! `prepare_evaluated_ascii` separately prepares one opaque reusable worker for
//! already-normalized nullable Bytes. Its fixed two-node ASCII7003 program uses
//! the official nullable wrapper even for ready NULL, then owns a computed
//! signed-Int result identity. No native child, binding-service or Host
//! callback, arbitrary signature, original SQL/PB schema, or raw-program escape
//! is exposed. The sealed pure context and actual wrapper-dispatch witness
//! persist per worker, not per row; caller coercion, diagnostics and
//! execution-owner lifetime remain outside this boundary. Wrapper counts are
//! not non-null body counts.

mod batch;
mod compile;
#[cfg(test)]
mod control_tests;
mod diagnostic;
#[cfg(test)]
mod diagnostic_tests;
pub(crate) mod host;
#[cfg(test)]
mod host_tests;
mod lineage;
#[cfg(test)]
mod lineage_tests;
mod profile;
#[cfg(test)]
mod profile_tests;
pub(crate) mod registry;
pub(crate) mod runtime;
mod spec;
#[cfg(test)]
mod tests;

pub use self::{
    batch::{
        ComputedInt, ComputedIntMetadata, EvaluatedAsciiWorker, LocalBatch, LocalEvalState,
        WorkerStorage, prepare_evaluated_ascii,
    },
    compile::{
        LocalNumericBatchProgram, LocalProgram, compile_control_with_lineage, compile_local,
        compile_local_profiled, compile_local_with_hosts, compile_numeric_batch,
    },
    diagnostic::{LocalFailureSite, LocalFailureStage, ReportedLocalFailure},
    host::{
        ArgMode, HostArgReply, HostArgRequest, HostCatalog, HostCatalogKey, HostInvocation,
        HostSignature, HostSlot, HostStart, HostStep, HostTaskId, LocalHostServices,
        PreparedHostCall,
    },
    lineage::{
        ControlLineageFacts, ControlProducerFact, ControlProducerRole, LineageCarrier,
        LineagedBatch, LocalControlProgram, ResultMetaId,
    },
    profile::{
        NumericBatchFacts, OrdinaryCallSite, OrdinaryProfile, OrdinaryProfileSpec, OrdinarySourceId,
    },
    runtime::{ExecutionLimits, InputRow, LocalRuntimeServices},
    spec::{CompileLimits, LocalCompileContext, LocalError, LocalExpr, LocalResult},
};
pub(crate) use self::{diagnostic::FailureRecorder, lineage::CheckedResultFlow};
pub use crate::{
    CallMetadata, FunctionRef, LiteralKind, LocalFunctionId, types::function::PreparedOrdinaryCall,
};
