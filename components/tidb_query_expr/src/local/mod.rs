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
//! `prepare_evaluated_bytes` separately prepares an opaque reusable worker for
//! a closed operation on ready nullable Int/Bytes: ASCII, LENGTH/OCTET_LENGTH,
//! BIT_LENGTH, LTRIM, RTRIM, UNHEX, CRC32, REVERSE, CHAR_LENGTH, QUOTE, HEX,
//! BIN, LEFT, RIGHT, REPLACE, BIT_COUNT, bitwise NOT/AND/OR/XOR and shifts,
//! with separate numeric/byte/UTF8 variants as needed. `EvaluatedArgs` owns the
//! fixed argument shapes, including Int2; Int preserves all 64 bits. An
//! operation admits only its canonical one-to-three ordered ColumnRefs
//! and one official FnCall. Its wrapper runs even for ready NULL; QUOTE(NULL)
//! returns owned non-NULL bytes "NULL". All ready owners remain charged through
//! result extraction. Results are owned Int/Bytes, not native SQL descriptors.
//! Frontends retain demand/coercion order, normalization, signature selection
//! and result packing (including CRC32/bitwise UInt). The old single-Bytes
//! eval_one and ASCII-only facade use the same compiler/worker/driver. No
//! native child, binding-service or Host callback, arbitrary signature,
//! original SQL/PB schema, or raw-program escape is exposed. The sealed context
//! and actual wrapper-dispatch witness persist per worker, not per row;
//! diagnostics and execution-owner lifetime stay outside. Wrapper counts are
//! not body counts.

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
        ComputedBytes, ComputedBytesMetadata, ComputedInt, ComputedIntMetadata, ComputedValue,
        EvaluatedArgs, EvaluatedAsciiWorker, EvaluatedBytesOp, EvaluatedBytesWorker, LocalBatch,
        LocalEvalState, WorkerStorage, prepare_evaluated_ascii, prepare_evaluated_bytes,
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
