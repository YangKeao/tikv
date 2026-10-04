// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    cell::{Cell, RefCell},
    mem,
    sync::{Arc, atomic::AtomicUsize},
};

use tidb_query_common::error::{ErrorInner, EvaluateError};
use tidb_query_datatype::{
    EvalType,
    codec::{
        batch::LazyBatchColumnVec,
        collation::native::NativeCollation,
        data_type::{
            BATCH_MAX_SIZE, Bytes, ChunkedVecBytes, ScalarValue, ScalarValueRef, VectorValue,
        },
        mysql::{
            DEFAULT_DIV_FRAC_INCR, Decimal, NativeVectorError, NativeVectorFloat32, Tz,
            decimal::NativeDecimalError, deserialize_native_vector_float32,
            peek_native_vector_float32, time::NativeSessionTimeZone,
        },
    },
    expr::{EvalConfig, EvalContext},
};

use super::{
    ExecutionLimits, FailureRecorder, LineageCarrier, LineagedBatch, LocalCompileContext,
    LocalControlProgram, LocalError, LocalProgram, LocalResult, LocalRuntimeServices,
    ReportedLocalFailure, ResultMetaId,
    compile::{
        LocalNumericBatchProgram, ProgramEntry, compile_evaluated_bytes,
        evaluated_ascii_bytes_type, evaluated_ascii_decimal_type, evaluated_ascii_int_type,
        evaluated_native_vector_type,
    },
    runtime::{EvalBudget, bytes_min_storage_bytes, int_min_storage_bytes, vector_storage_bytes},
};
use crate::{
    BinaryArithmeticErrorKind, BinaryArithmeticOperation, LegacyBinaryArithmeticError,
    NativeAesError, NativeAesOperation, NativeAesProfile, NativeBinaryArithmeticError,
    NativeDecimalFastOutcome, NativeLikeInvocation, NativeLikeKind, NativeRegexpError,
    NativeRegexpInvocation, NativeUnaryMinusError, RpnExpressionNode, RpnStackNode,
    RpnStackNodeVectorValue,
    impl_arithmetic::NativeDecimalDivisionDisposition,
    impl_string::{
        ConcatKind, FieldKind, PreparedCharArgs, PreparedConcatArgs, PreparedExportSetArgs,
        PreparedFieldArgs, PreparedFindInSetKeys, PreparedMakeSetArgs,
    },
    types::expr_eval::{EvalInput, EvaluatedAsciiWitness, FrameResult, evaluated_bytes_shape},
};

#[derive(Debug)]
pub(crate) struct NativeTemporalCallMetadata {
    zone: RefCell<Option<NativeSessionTimeZone>>,
}

impl NativeTemporalCallMetadata {
    pub(crate) fn new() -> Self {
        Self {
            zone: RefCell::new(None),
        }
    }

    pub(crate) fn zone(&self) -> LocalResult<std::cell::Ref<'_, NativeSessionTimeZone>> {
        let zone = self
            .zone
            .try_borrow()
            .map_err(|_| LocalError::InvalidSpec("temporal zone borrow conflict".into()))?;
        std::cell::Ref::filter_map(zone, Option::as_ref)
            .map_err(|_| LocalError::InvalidSpec("temporal call has no bound zone".into()))
    }

    pub(crate) fn is_unbound(&self) -> bool {
        self.zone.try_borrow().is_ok_and(|zone| zone.is_none())
    }

    fn bind(&self, zone: NativeSessionTimeZone) -> LocalResult<()> {
        let mut target = self
            .zone
            .try_borrow_mut()
            .map_err(|_| LocalError::InvalidSpec("temporal zone bind conflict".into()))?;
        if target.is_some() {
            return Err(LocalError::InvalidSpec(
                "temporal zone is already bound".into(),
            ));
        }
        *target = Some(zone);
        Ok(())
    }

    fn unbind(&self) -> LocalResult<()> {
        let owned = self
            .zone
            .try_borrow_mut()
            .map_err(|_| LocalError::InvalidSpec("temporal zone unbind conflict".into()))?
            .take();
        drop(owned);
        Ok(())
    }
}

fn temporal_zone_heap_bytes(zone: &NativeSessionTimeZone) -> usize {
    match zone {
        NativeSessionTimeZone::Fixed { name, .. } => name.capacity(),
        _ => 0,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeRegexpKind {
    Like,
    Substr,
    Instr,
    Replace,
}

/// A fixed-size, initially unbound call payload. The statement owns the actual
/// caches: this worker holds their handles only during one guarded invocation.
/// Known cache structures/capacities are recorded at the actual use site; the
/// opaque regex engine and its TLS scratch are outside this Demo accounting.
#[derive(Debug)]
pub(crate) struct NativeRegexpCallMetadata {
    kind: NativeRegexpKind,
    invocation: RefCell<Option<NativeRegexpInvocation>>,
    known_cache_bytes: Cell<Option<usize>>,
    known_cache_limit: Cell<usize>,
}

impl NativeRegexpCallMetadata {
    pub(crate) fn new(kind: NativeRegexpKind) -> Self {
        Self {
            kind,
            invocation: RefCell::new(None),
            known_cache_bytes: Cell::new(Some(0)),
            known_cache_limit: Cell::new(0),
        }
    }

    pub(crate) fn invocation(&self) -> LocalResult<NativeRegexpInvocation> {
        self.invocation
            .try_borrow()
            .map_err(|_| LocalError::InvalidSpec("regexp invocation is already borrowed".into()))?
            .as_ref()
            .cloned()
            .ok_or_else(|| LocalError::InvalidSpec("regexp call has no bound invocation".into()))
    }

    pub(crate) fn record_known_cache_bytes(&self, bytes: Option<usize>) -> LocalResult<()> {
        if self
            .invocation
            .try_borrow()
            .map_err(|_| LocalError::InvalidSpec("regexp invocation is already borrowed".into()))?
            .is_none()
        {
            return Err(LocalError::InvalidSpec(
                "regexp cache observation has no invocation".into(),
            ));
        }
        let total = self
            .known_cache_bytes
            .get()
            .and_then(|old| old.checked_add(bytes?));
        // Retain an overflow/refusal until the worker has inspected it. It must
        // never be authenticated as a native SQL failure, even if wrapped in Caused.
        self.known_cache_bytes.set(total);
        match total {
            Some(total) if total <= self.known_cache_limit.get() => Ok(()),
            _ => Err(evaluated_ascii_storage_overflow()),
        }
    }

    fn is_unbound(&self) -> bool {
        self.invocation
            .try_borrow()
            .is_ok_and(|value| value.is_none())
            && self.known_cache_bytes.get() == Some(0)
            && self.known_cache_limit.get() == 0
    }

    fn bind(&self, invocation: NativeRegexpInvocation, limit: usize) -> LocalResult<()> {
        if !self.is_unbound() {
            return Err(LocalError::InvalidSpec(
                "regexp call retains a previous binding".into(),
            ));
        }
        let mut target = self
            .invocation
            .try_borrow_mut()
            .map_err(|_| LocalError::InvalidSpec("regexp invocation bind conflict".into()))?;
        self.known_cache_limit.set(limit);
        *target = Some(invocation);
        Ok(())
    }

    fn unbind(&self) -> LocalResult<()> {
        let owned = self
            .invocation
            .try_borrow_mut()
            .map_err(|_| LocalError::InvalidSpec("regexp invocation unbind conflict".into()))?
            .take();
        self.known_cache_bytes.set(Some(0));
        self.known_cache_limit.set(0);
        drop(owned);
        Ok(())
    }
}

/// LIKE uses the same invocation-scoped binding discipline as regexp, with a
/// distinct typed holder. No context/cache/collation is a SQL operand, and no
/// cache observation occurs before the actual generated wrapper executes.
#[derive(Debug)]
pub(crate) struct NativeLikeCallMetadata {
    kind: NativeLikeKind,
    invocation: RefCell<Option<NativeLikeInvocation>>,
    known_cache_bytes: Cell<Option<usize>>,
    known_cache_limit: Cell<usize>,
}

impl NativeLikeCallMetadata {
    pub(crate) fn new(kind: NativeLikeKind) -> Self {
        Self {
            kind,
            invocation: RefCell::new(None),
            known_cache_bytes: Cell::new(Some(0)),
            known_cache_limit: Cell::new(0),
        }
    }

    pub(crate) fn invocation(&self) -> LocalResult<NativeLikeInvocation> {
        self.invocation
            .try_borrow()
            .map_err(|_| LocalError::InvalidSpec("LIKE invocation is already borrowed".into()))?
            .as_ref()
            .cloned()
            .ok_or_else(|| LocalError::InvalidSpec("LIKE call has no bound invocation".into()))
    }

    pub(crate) fn record_known_cache_bytes(&self, bytes: Option<usize>) -> LocalResult<()> {
        if self
            .invocation
            .try_borrow()
            .map_err(|_| LocalError::InvalidSpec("LIKE invocation is already borrowed".into()))?
            .is_none()
        {
            return Err(LocalError::InvalidSpec(
                "LIKE cache observation has no invocation".into(),
            ));
        }
        let total = self
            .known_cache_bytes
            .get()
            .and_then(|old| old.checked_add(bytes?));
        // Preserve refusal/overflow until the worker checks the use-site record.
        self.known_cache_bytes.set(total);
        match total {
            Some(total) if total <= self.known_cache_limit.get() => Ok(()),
            _ => Err(evaluated_ascii_storage_overflow()),
        }
    }

    fn is_unbound(&self) -> bool {
        self.invocation
            .try_borrow()
            .is_ok_and(|value| value.is_none())
            && self.known_cache_bytes.get() == Some(0)
            && self.known_cache_limit.get() == 0
    }

    fn bind(&self, invocation: NativeLikeInvocation, limit: usize) -> LocalResult<()> {
        if !self.is_unbound() || invocation.kind() != self.kind {
            return Err(LocalError::InvalidSpec(
                "LIKE binding is retained or has the wrong kind".into(),
            ));
        }
        let mut target = self
            .invocation
            .try_borrow_mut()
            .map_err(|_| LocalError::InvalidSpec("LIKE invocation bind conflict".into()))?;
        self.known_cache_limit.set(limit);
        *target = Some(invocation);
        Ok(())
    }

    fn unbind(&self) -> LocalResult<()> {
        let owned = self
            .invocation
            .try_borrow_mut()
            .map_err(|_| LocalError::InvalidSpec("LIKE invocation unbind conflict".into()))?
            .take();
        self.known_cache_bytes.set(Some(0));
        self.known_cache_limit.set(0);
        drop(owned);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeDecimalDivisionKind {
    Native,
    Legacy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecimalDivisionCallState {
    Unbound,
    Bound(u32),
    Entered,
    Completed(Option<NativeDecimalDivisionDisposition>),
    Consumed,
    Invalid,
}

/// Fixed invocation state only: the official vector, never this metadata,
/// owns the Decimal result. Invalid transitions remain sticky through cleanup.
#[derive(Debug)]
pub(crate) struct NativeDecimalDivisionCallMetadata {
    kind: NativeDecimalDivisionKind,
    state: Cell<DecimalDivisionCallState>,
}

impl NativeDecimalDivisionCallMetadata {
    pub(crate) fn new(kind: NativeDecimalDivisionKind) -> Self {
        Self {
            kind,
            state: Cell::new(DecimalDivisionCallState::Unbound),
        }
    }

    fn refuse(&self) -> LocalError {
        self.state.set(DecimalDivisionCallState::Invalid);
        LocalError::InvalidSpec("Decimal division invocation state differs from its call".into())
    }

    fn is_unbound(&self) -> bool {
        self.state.get() == DecimalDivisionCallState::Unbound
    }

    fn bind(&self, frac_increment: u32) -> LocalResult<()> {
        if !self.is_unbound() {
            return Err(self.refuse());
        }
        self.state
            .set(DecimalDivisionCallState::Bound(frac_increment));
        Ok(())
    }

    pub(crate) fn begin_kernel(&self, expected: NativeDecimalDivisionKind) -> LocalResult<u32> {
        if self.kind != expected {
            return Err(self.refuse());
        }
        match self.state.get() {
            DecimalDivisionCallState::Bound(increment) => {
                self.state.set(DecimalDivisionCallState::Entered);
                Ok(increment)
            }
            _ => Err(self.refuse()),
        }
    }

    pub(crate) fn record_disposition(
        &self,
        status: NativeDecimalDivisionDisposition,
    ) -> LocalResult<()> {
        self.record(Some(status))
    }

    pub(crate) fn record_error(&self) -> LocalResult<()> {
        self.record(None)
    }

    fn record(&self, status: Option<NativeDecimalDivisionDisposition>) -> LocalResult<()> {
        if self.state.get() != DecimalDivisionCallState::Entered {
            return Err(self.refuse());
        }
        self.state.set(DecimalDivisionCallState::Completed(status));
        Ok(())
    }

    fn consume(
        &self,
        calls: Option<u64>,
        success: bool,
    ) -> LocalResult<Option<NativeDecimalDivisionDisposition>> {
        let status = match (calls, success, self.state.get()) {
            (Some(1), true, DecimalDivisionCallState::Completed(Some(status))) => Some(status),
            // A driver/output budget error can follow an actual successful body.
            (Some(1), false, DecimalDivisionCallState::Completed(_))
            | (Some(0), false, DecimalDivisionCallState::Bound(_)) => None,
            _ => return Err(self.refuse()),
        };
        self.state.set(DecimalDivisionCallState::Consumed);
        Ok(status)
    }

    fn finish(&self, calls: Option<u64>, success: bool) -> LocalResult<()> {
        if self.state.get() == DecimalDivisionCallState::Consumed
            && (calls == Some(1) || (!success && calls == Some(0)))
        {
            return Ok(());
        }
        // Accounting can refuse before eval_ready reaches the official driver.
        if !success && calls == Some(0) {
            return self.consume(calls, false).map(|_| ());
        }
        Err(self.refuse())
    }

    fn unbind(&self) -> LocalResult<()> {
        if self.state.get() == DecimalDivisionCallState::Invalid {
            return Err(self.refuse());
        }
        self.state.set(DecimalDivisionCallState::Unbound);
        Ok(())
    }
}

pub struct LocalBatch<'a> {
    pub columns: &'a LazyBatchColumnVec,
    pub physical_rows: usize,
    pub selection: &'a [usize],
}

/// Reusable index-only scratch and Demo limits. No input/program borrow or
/// mutable service survives evaluation. The external ctx retains its warnings.
pub struct LocalEvalState {
    row: [usize; 1],
    limits: ExecutionLimits,
}

impl LocalEvalState {
    pub fn new(max_steps: u64) -> Self {
        Self::with_limits(ExecutionLimits {
            max_steps,
            ..ExecutionLimits::default()
        })
    }
    pub fn with_limits(limits: ExecutionLimits) -> Self {
        Self { row: [0], limits }
    }
}

impl Default for LocalEvalState {
    fn default() -> Self {
        Self::new(u64::MAX)
    }
}

fn validate_selection(physical_rows: usize, selection: &[usize]) -> LocalResult<()> {
    if selection.iter().any(|&row| row >= physical_rows) {
        return Err(LocalError::InvalidBatch(
            "selection is outside the physical row universe".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum OutputMode {
    ConservativeInt,
    Lineaged,
}

/// Collection policy only: both variants use the same preflight and occurrence
/// loop, and the same RPN driver evaluates every demanded descendant.
enum RowCollector {
    ConservativeInt(VectorValue),
    Lineaged(LineageOutput),
}

impl RowCollector {
    fn append(&mut self, result: FrameResult<'_>, budget: &mut EvalBudget) -> LocalResult<()> {
        match self {
            Self::ConservativeInt(output) => {
                // Keep the original Int collector, including its materialization
                // and donor behavior, outside the new exact-storage policy.
                let mut values = match result.node {
                    RpnStackNode::Scalar { value, .. } => VectorValue::from_scalar(value, 1),
                    RpnStackNode::Vector { value, .. } => {
                        value.take_vector_value().map_err(LocalError::Evaluation)?
                    }
                };
                if values.eval_type() != EvalType::Int || values.len() != 1 {
                    return Err(LocalError::InvalidBatch(
                        "RPN returned an invalid result shape".into(),
                    ));
                }
                output.append(&mut values);
                Ok(())
            }
            Self::Lineaged(output) => output.append(result, budget),
        }
    }

    fn finish(&self, budget: &mut EvalBudget) -> LocalResult<()> {
        if let Self::Lineaged(output) = self {
            if output.values.len() != output.rows || output.result_metadata.len() != output.rows {
                return Err(LocalError::InvalidBatch(
                    "lineaged output value/metadata count differs from selection".into(),
                ));
            }
            // Remeasure actual capacity before publication, including empty
            // Bytes' sentinel. There is no donor/reset allocation at this edge.
            let bytes = output.storage_bytes(budget)?;
            budget.check_output(bytes, 0)?;
            budget.set_output_bytes(bytes)?;
        }
        Ok(())
    }

    fn into_legacy(self) -> LocalResult<VectorValue> {
        match self {
            Self::ConservativeInt(values) => Ok(values),
            Self::Lineaged(_) => Err(LocalError::InvalidSpec(
                "lineaged output cannot discard its result metadata".into(),
            )),
        }
    }

    fn into_lineaged(self) -> LocalResult<LineagedBatch> {
        match self {
            Self::Lineaged(output) => Ok(LineagedBatch {
                values: output.values,
                result_metadata: output.result_metadata,
            }),
            Self::ConservativeInt(_) => Err(LocalError::InvalidSpec(
                "legacy output has no checked result metadata".into(),
            )),
        }
    }
}

struct LineageOutput {
    values: VectorValue,
    result_metadata: Vec<ResultMetaId>,
    rows: usize,
    payload_bytes: usize,
}

fn output_size(bytes: Option<usize>) -> LocalResult<usize> {
    bytes
        .filter(|&bytes| bytes != usize::MAX)
        .ok_or_else(|| LocalError::ResourceLimit("lineaged output storage size overflow".into()))
}

fn add_output_size(left: usize, right: usize) -> LocalResult<usize> {
    output_size(left.checked_add(right))
}

impl LineageOutput {
    fn new(carrier: LineageCarrier, rows: usize, budget: &mut EvalBudget) -> LocalResult<Self> {
        let ids_min = output_size(rows.checked_mul(mem::size_of::<ResultMetaId>()))?;
        let values_min = output_size(match carrier {
            LineageCarrier::Int => int_min_storage_bytes(rows),
            LineageCarrier::Bytes => bytes_min_storage_bytes(rows, 0),
        })?;
        budget.check_output(add_output_size(ids_min, values_min)?, 0)?;

        let mut result_metadata = Vec::new();
        result_metadata.try_reserve_exact(rows).map_err(|_| {
            LocalError::ResourceLimit("lineaged result metadata allocation failed".into())
        })?;
        let ids_actual = output_size(
            result_metadata
                .capacity()
                .checked_mul(mem::size_of::<ResultMetaId>()),
        )?;
        // A successful try_reserve_exact is not a promise of exact capacity.
        budget.check_output(add_output_size(ids_actual, values_min)?, 0)?;
        let values = match carrier {
            LineageCarrier::Int => VectorValue::with_capacity(rows, EvalType::Int),
            LineageCarrier::Bytes => {
                VectorValue::Bytes(ChunkedVecBytes::try_with_capacities(rows, 0).map_err(|_| {
                    LocalError::ResourceLimit("lineaged Bytes output allocation failed".into())
                })?)
            }
        };
        let output = Self {
            values,
            result_metadata,
            rows,
            payload_bytes: 0,
        };
        let bytes = output.storage_bytes(budget)?;
        budget.check_output(bytes, 0)?;
        budget.set_output_bytes(bytes)?;
        Ok(output)
    }

    fn id_bytes(&self) -> LocalResult<usize> {
        output_size(
            self.result_metadata
                .capacity()
                .checked_mul(mem::size_of::<ResultMetaId>()),
        )
    }

    fn storage_bytes(&self, budget: &EvalBudget) -> LocalResult<usize> {
        add_output_size(
            self.id_bytes()?,
            vector_storage_bytes(&self.values, budget.mode()),
        )
    }

    fn append(&mut self, result: FrameResult<'_>, budget: &mut EvalBudget) -> LocalResult<()> {
        let id = result.meta.ok_or_else(|| {
            LocalError::InvalidSpec("lineaged RPN result is missing its checked metadata ID".into())
        })?;
        if self.values.len() != self.result_metadata.len() || self.values.len() >= self.rows {
            return Err(LocalError::InvalidBatch(
                "lineaged output occurrence count changed".into(),
            ));
        }
        // Check the logical singleton before using the scalar-ref accessor. A
        // selected child's FieldType may differ from the declared root type;
        // its already-checked ID, not a new type/kind probe, owns that identity.
        let singleton = match &result.node {
            RpnStackNode::Scalar { .. } => true,
            RpnStackNode::Vector {
                value: RpnStackNodeVectorValue::Generated { physical_value },
                ..
            } => physical_value.len() == 1,
            RpnStackNode::Vector {
                value:
                    RpnStackNodeVectorValue::Ref {
                        physical_value,
                        logical_rows,
                    },
                ..
            } => logical_rows.len() == 1 && logical_rows[0] < physical_value.len(),
        };
        if !singleton {
            return Err(LocalError::InvalidBatch(
                "lineaged RPN returned an invalid result shape".into(),
            ));
        }
        let scalar = result.node.get_logical_scalar_ref(0);
        if scalar.eval_type() != self.values.eval_type() {
            return Err(LocalError::InvalidBatch(
                "lineaged RPN result differs from its checked carrier".into(),
            ));
        }
        let source = result.retained_heap_bytes(budget.mode());
        let ids = self.id_bytes()?;
        let old_heap = vector_storage_bytes(&self.values, budget.mode());
        budget.check_output(add_output_size(ids, old_heap)?, source)?;

        match (&mut self.values, scalar) {
            (values @ VectorValue::Int(_), ScalarValueRef::Int(value)) => {
                // All rows were reserved initially. Copy only the nullable bits,
                // without a scalar/vector donor or native signedness inference.
                values.push_int(value.copied());
            }
            (VectorValue::Bytes(values), ScalarValueRef::Bytes(value)) => {
                let added = value.map_or(0, |bytes| bytes.len());
                let payload = output_size(self.payload_bytes.checked_add(added))?;
                let minimum = output_size(bytes_min_storage_bytes(self.rows, payload))?;
                budget.check_output(add_output_size(ids, old_heap.max(minimum))?, source)?;
                // Reserve with the selected source still alive. The minimum is
                // not a capacity prediction, and a failed reserve may grow some
                // buffers: failure aborts this invocation rather than rollback.
                values.try_reserve_append(1, added).map_err(|_| {
                    LocalError::ResourceLimit("lineaged Bytes output reservation failed".into())
                })?;
                let actual = output_size(values.retained_heap_bytes())?;
                let overlap = if actual > old_heap {
                    add_output_size(source, old_heap)?
                } else {
                    source
                };
                let output = add_output_size(ids, actual)?;
                // Account for old/new allocation overlap before copying the
                // payload or performing any later occurrence's input effect.
                budget.check_output(output, overlap)?;
                budget.set_output_bytes(output)?;
                values.push_ref(value);
                self.payload_bytes = payload;
            }
            _ => {
                return Err(LocalError::InvalidBatch(
                    "lineaged RPN result differs from its checked carrier".into(),
                ));
            }
        }
        // The ID buffer has full selection capacity. Remeasure the value heap
        // while the selected source is still live, even on the final row.
        self.result_metadata.push(id);
        let bytes = self.storage_bytes(budget)?;
        budget.check_output(bytes, source)?;
        budget.set_output_bytes(bytes)?;
        Ok(())
    }
}

impl LocalProgram {
    pub fn eval(
        &mut self,
        state: &mut LocalEvalState,
        ctx: &mut EvalContext,
        batch: LocalBatch<'_>,
    ) -> LocalResult<VectorValue> {
        if batch.columns.columns_len() != self.schema.len() {
            return Err(LocalError::InvalidBatch(
                "column count differs from compiled schema".into(),
            ));
        }
        for index in 0..batch.columns.columns_len() {
            let column = &batch.columns[index];
            if !column.is_decoded() {
                return Err(LocalError::InvalidBatch(format!(
                    "column {} is not decoded",
                    index
                )));
            }
            if column.decoded().eval_type() != EvalType::Int || column.len() != batch.physical_rows
            {
                return Err(LocalError::InvalidBatch(format!(
                    "column {} has incorrect type/length",
                    index
                )));
            }
        }
        validate_selection(batch.physical_rows, batch.selection)?;
        self.check_entry(ProgramEntry::Row)?;
        if self.host_catalog.is_some() {
            return Err(LocalError::HostContract(
                "host program requires runtime services".into(),
            ));
        }
        self.eval_rows(
            state,
            ctx,
            batch.selection,
            EvalInput::Decoded(batch.columns),
            None,
            OutputMode::ConservativeInt,
        )?
        .into_legacy()
    }

    /// Validates static layout and row bounds before effects, but imports
    /// values only through a demanded ColumnRef in the same official RPN
    /// driver.
    pub fn eval_with_bindings(
        &mut self,
        state: &mut LocalEvalState,
        ctx: &mut EvalContext,
        physical_rows: usize,
        selection: &[usize],
        services: &mut dyn LocalRuntimeServices,
    ) -> LocalResult<VectorValue> {
        self.eval_bindings(
            state,
            ctx,
            physical_rows,
            selection,
            services,
            None,
            OutputMode::ConservativeInt,
        )?
        .into_legacy()
    }

    /// Runs the same binding evaluator, retaining the original owned error and
    /// an exact site only when read_input or a checked ordinary kernel fails.
    /// Hosts are outside this reporting slice; the original entry still admits
    /// them. Unannotated eager errors, validation and budgets gain no fake
    /// site.
    ///
    /// Nothing is retained in state/program/ctx between calls. Warning count
    /// and stored details stay untouched by reporting; callers may snapshot
    /// both before/after normal return. Panic remains unwind, not a
    /// reported error.
    pub fn eval_with_bindings_reported(
        &mut self,
        state: &mut LocalEvalState,
        ctx: &mut EvalContext,
        physical_rows: usize,
        selection: &[usize],
        services: &mut dyn LocalRuntimeServices,
    ) -> std::result::Result<VectorValue, ReportedLocalFailure> {
        let mut recorder = FailureRecorder::default();
        // Exactly one invocation; no recovery/retry after a captured failure.
        let result = self
            .eval_bindings(
                state,
                ctx,
                physical_rows,
                selection,
                services,
                Some(&mut recorder),
                OutputMode::ConservativeInt,
            )
            .and_then(RowCollector::into_legacy);
        result.map_err(|error| recorder.into_failure(error))
    }

    /// Only stable schema and row-bound checks: no value import, host hook or
    /// kernel. Every facade follows this with its compiled entry-tag check.
    fn validate_bindings_preflight(
        &self,
        physical_rows: usize,
        selection: &[usize],
        services: &dyn LocalRuntimeServices,
    ) -> LocalResult<()> {
        if services.binding_schema() != self.schema.as_slice() {
            return Err(LocalError::InvalidBatch(
                "binding schema differs from compiled complete field types".into(),
            ));
        }
        validate_selection(physical_rows, selection)
    }

    fn eval_bindings(
        &mut self,
        state: &mut LocalEvalState,
        ctx: &mut EvalContext,
        physical_rows: usize,
        selection: &[usize],
        services: &mut dyn LocalRuntimeServices,
        recorder: Option<&mut FailureRecorder>,
        mode: OutputMode,
    ) -> LocalResult<RowCollector> {
        self.validate_bindings_preflight(physical_rows, selection, services)?;
        self.check_entry(match mode {
            OutputMode::ConservativeInt => ProgramEntry::Row,
            OutputMode::Lineaged => ProgramEntry::ControlLineage,
        })?;
        if recorder.is_some() && self.host_catalog.is_some() {
            return Err(LocalError::HostContract(
                "host programs are outside reported evaluation admission".into(),
            ));
        }
        if matches!(mode, OutputMode::Lineaged) && self.host_catalog.is_some() {
            return Err(LocalError::HostContract(
                "host programs are outside lineaged evaluation admission".into(),
            ));
        }
        // Pure capability validation, including empty batches. A host-free D1
        // program never calls this optional hook at all.
        if let Some(expected) = self.host_catalog {
            let host = services.host_services().ok_or_else(|| {
                LocalError::HostContract("compiled host catalog has no runtime provider".into())
            })?;
            if host.catalog_key() != &expected {
                return Err(LocalError::HostContract(
                    "runtime host catalog differs from compiled identity".into(),
                ));
            }
        }
        self.eval_rows(
            state,
            ctx,
            selection,
            EvalInput::Bindings(services),
            recorder,
            mode,
        )
    }

    fn eval_rows(
        &mut self,
        state: &mut LocalEvalState,
        ctx: &mut EvalContext,
        selection: &[usize],
        mut input: EvalInput<'_, '_>,
        mut recorder: Option<&mut FailureRecorder>,
        mode: OutputMode,
    ) -> LocalResult<RowCollector> {
        // One occurrence loop and one work budget for both facades. Lineage
        // adds collection/storage policy, never another expression evaluator.
        let (mut budget, mut output) = match mode {
            OutputMode::ConservativeInt => (
                EvalBudget::local(state.limits, selection.len())?,
                RowCollector::ConservativeInt(VectorValue::with_capacity(
                    selection.len(),
                    EvalType::Int,
                )),
            ),
            OutputMode::Lineaged => {
                let flow = self.expression.checked_result_flow().ok_or_else(|| {
                    LocalError::InvalidSpec(
                        "lineaged program has no checked root result flow".into(),
                    )
                })?;
                let mut budget = EvalBudget::lineaged(state.limits)?;
                let output = LineageOutput::new(flow.carrier(), selection.len(), &mut budget)?;
                (budget, RowCollector::Lineaged(output))
            }
        };
        // Empty selection imports no values or frames, but exact Bytes output
        // still reserves and checks its offset sentinel before publication.
        for (occurrence, &row) in selection.iter().enumerate() {
            state.row[0] = row;
            let result = match mode {
                OutputMode::ConservativeInt => FrameResult {
                    node: self.expression.eval_with_input_recording(
                        ctx,
                        &self.schema,
                        &mut input,
                        &state.row,
                        1,
                        occurrence,
                        self.host_catalog,
                        &mut budget,
                        recorder.as_deref_mut(),
                    )?,
                    meta: None,
                },
                OutputMode::Lineaged => self.expression.eval_with_input_lineaged(
                    ctx,
                    &self.schema,
                    &mut input,
                    &state.row,
                    1,
                    occurrence,
                    self.host_catalog,
                    &mut budget,
                    recorder.as_deref_mut(),
                )?,
            };
            output.append(result, &mut budget)?;
        }
        output.finish(&mut budget)?;
        Ok(output)
    }
}

impl LocalControlProgram {
    /// Evaluates checked control lineage in selection-occurrence order. Values
    /// and result IDs are collected together; no source-kind callback, decoded
    /// facade or untagged RPN escape is provided.
    pub fn eval_with_bindings(
        &mut self,
        state: &mut LocalEvalState,
        ctx: &mut EvalContext,
        physical_rows: usize,
        selection: &[usize],
        services: &mut dyn LocalRuntimeServices,
    ) -> LocalResult<LineagedBatch> {
        self.inner
            .eval_bindings(
                state,
                ctx,
                physical_rows,
                selection,
                services,
                None,
                OutputMode::Lineaged,
            )?
            .into_lineaged()
    }

    /// Uses the same collector and driver, with a fresh failure-only recorder.
    /// Validation/output-budget failures gain no input/kernel site; warnings
    /// stay in the caller's existing context, including on a refused final row.
    pub fn eval_with_bindings_reported(
        &mut self,
        state: &mut LocalEvalState,
        ctx: &mut EvalContext,
        physical_rows: usize,
        selection: &[usize],
        services: &mut dyn LocalRuntimeServices,
    ) -> std::result::Result<LineagedBatch, ReportedLocalFailure> {
        let mut recorder = FailureRecorder::default();
        let result = self
            .inner
            .eval_bindings(
                state,
                ctx,
                physical_rows,
                selection,
                services,
                Some(&mut recorder),
                OutputMode::Lineaged,
            )
            .and_then(RowCollector::into_lineaged);
        result.map_err(|error| recorder.into_failure(error))
    }
}

/// Publishes the already-owned root without a second collector or donor copy.
/// Driver frames have been dropped, so the root is charged once as output.
fn finish_numeric_output(
    output: VectorValue,
    rows: usize,
    budget: &mut EvalBudget,
) -> LocalResult<VectorValue> {
    if output.eval_type() != EvalType::Int || output.len() != rows {
        return Err(LocalError::InvalidBatch(
            "numeric batch RPN returned an invalid Int result shape".into(),
        ));
    }
    let bytes = vector_storage_bytes(&output, budget.mode());
    budget.check_output(bytes, 0)?;
    budget.set_output_bytes(bytes)?;
    Ok(output)
}

impl LocalNumericBatchProgram {
    /// Evaluates the checked SQL numeric batch domain once over the whole
    /// borrowed selection. The original owned error is returned unchanged.
    pub fn eval_with_bindings(
        &mut self,
        state: &mut LocalEvalState,
        ctx: &mut EvalContext,
        physical_rows: usize,
        selection: &[usize],
        services: &mut dyn LocalRuntimeServices,
    ) -> LocalResult<VectorValue> {
        self.eval_with_bindings_reported(state, ctx, physical_rows, selection, services)
            .map_err(ReportedLocalFailure::into_error)
    }

    /// Uses the same single invocation with a fresh, failure-only recorder.
    /// Pure preflight, resource and publication failures remain unsited; the
    /// caller's warning count and stored warnings are never reset or replayed.
    pub fn eval_with_bindings_reported(
        &mut self,
        state: &mut LocalEvalState,
        ctx: &mut EvalContext,
        physical_rows: usize,
        selection: &[usize],
        services: &mut dyn LocalRuntimeServices,
    ) -> std::result::Result<VectorValue, ReportedLocalFailure> {
        let mut recorder = FailureRecorder::default();
        let result = self.eval_numeric_bindings(
            state,
            ctx,
            physical_rows,
            selection,
            services,
            &mut recorder,
        );
        result.map_err(|error| recorder.into_failure(error))
    }

    fn eval_numeric_bindings(
        &mut self,
        state: &mut LocalEvalState,
        ctx: &mut EvalContext,
        physical_rows: usize,
        selection: &[usize],
        services: &mut dyn LocalRuntimeServices,
        recorder: &mut FailureRecorder,
    ) -> LocalResult<VectorValue> {
        self.inner
            .validate_bindings_preflight(physical_rows, selection, services)?;
        self.inner.check_entry(ProgramEntry::SqlNumericBatch)?;
        if self.inner.host_catalog.is_some()
            || self.inner.expression.checked_result_flow().is_some()
        {
            return Err(LocalError::InvalidSpec(
                "numeric batch program carries host or result-lineage state".into(),
            ));
        }
        // Bound selected occurrences, not the caller's physical row universe.
        // Route validation above applies even when no values will be demanded.
        let rows = selection.len();
        if rows > BATCH_MAX_SIZE {
            return Err(LocalError::ResourceLimit(
                "numeric batch selection exceeds 1024 occurrences".into(),
            ));
        }
        let mut budget = EvalBudget::exact(state.limits)?;
        if rows == 0 {
            return finish_numeric_output(
                VectorValue::with_capacity(0, EvalType::Int),
                0,
                &mut budget,
            );
        }

        let mut input = EvalInput::Bindings(services);
        // Exactly one driver invocation. The driver owns all intermediate and
        // root charges until it returns; there is no per-occurrence root loop.
        let result = self.inner.expression.eval_with_input_numeric_batch(
            ctx,
            &self.inner.schema,
            &mut input,
            selection,
            &mut budget,
            Some(recorder),
        )?;
        let output = match result {
            RpnStackNode::Scalar { value, .. } => {
                let ScalarValue::Int(value) = value else {
                    return Err(LocalError::InvalidBatch(
                        "numeric batch scalar result is not Int".into(),
                    ));
                };
                let minimum = int_min_storage_bytes(rows).ok_or_else(|| {
                    LocalError::ResourceLimit("numeric batch output layout overflow".into())
                })?;
                budget.check_output(minimum, 0)?;
                let mut output = VectorValue::with_capacity(rows, EvalType::Int);
                // Requested rows are not an actual-capacity prediction. All
                // Int/bitmap capacity is checked before filling reserved slots.
                budget.check_output(vector_storage_bytes(&output, budget.mode()), 0)?;
                for _ in 0..rows {
                    output.push_int(*value);
                }
                output
            }
            RpnStackNode::Vector {
                value: RpnStackNodeVectorValue::Generated { physical_value },
                ..
            } => physical_value,
            RpnStackNode::Vector {
                value: RpnStackNodeVectorValue::Ref { .. },
                ..
            } => {
                return Err(LocalError::InvalidBatch(
                    "numeric batch bindings returned a borrowed root".into(),
                ));
            }
        };
        finish_numeric_output(output, rows, &mut budget)
    }
}

/// Closed operations over already-evaluated nullable Int/Bytes, explicit
/// IEEE754 bits or NoArgs. Private identities have independently fixed roles:
/// raw math's Byte8 storage never admits ordinary Bytes, nullable IP predicates
/// do consume Bytes, and PI consumes no argument. None reserves a wire
/// signature. LENGTH/OCTET_LENGTH share Length and SHA/SHA1 share Sha1; no
/// arbitrary signature or SQL descriptor is accepted. Hashes consume raw ready
/// Bytes. UTF8 variants require the caller's normalized UTF8;
/// this boundary never chooses a SQL charset or performs lossy conversion.
/// Quote uses its official nullable kernel: a NULL input yields non-NULL
/// "NULL". Boolean operations take frontend-normalized Int truth/presence
/// (None/Some(0)/Some(1)), never raw SQL values. Each operation fixes its
/// entire input shape and either one kernel or one of three exact base-then-NOT
/// pairs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EvaluatedBytesOp {
    Ascii,
    Length,
    BitLength,
    LTrim,
    RTrim,
    UnHex,
    Crc32,
    Reverse,
    ReverseUtf8,
    CharLength,
    CharLengthUtf8,
    Quote,
    HexInt,
    HexStr,
    Bin,
    Left,
    LeftUtf8,
    Right,
    RightUtf8,
    Replace,
    BitCount,
    BitNeg,
    BitAnd,
    BitOr,
    BitXor,
    LeftShift,
    RightShift,
    UnaryNot,
    IsNull,
    IsTrue,
    IsFalse,
    IsTrueWithNull,
    IsNotNull,
    IsNotTrue,
    IsNotFalse,
    Md5,
    Sha1,
    LogicalAnd,
    LogicalOr,
    LogicalXor,
    InetAton,
    InetNtoa,
    Inet6Aton,
    Inet6Ntoa,
    AsinRaw,
    AcosRaw,
    SqrtRaw,
    SignRaw,
    RadiansRaw,
    DegreesRaw,
    PiRaw,
    IsIpv4Nullable,
    IsIpv6Nullable,
    IsIpv4CompatNullable,
    IsIpv4MappedNullable,
    SpaceNative,
    RepeatNative,
    ToBase64Native,
    FromBase64Native,
    FromBase64ValueNative,
    Lower,
    Upper,
    LowerUtf8Ready,
    UpperUtf8Ready,
    Sha2Native,
    OrdNative,
    TrimBothNative,
    TrimLeadingNative,
    TrimTrailingNative,
    SubstringIndexSignedNative,
    SubstringIndexUnsignedNative,
    LpadBytesNative,
    RpadBytesNative,
    LpadUtf8Native,
    RpadUtf8Native,
    LnNative,
    LogNative,
    Log2Native,
    PowNative,
    UncompressedLengthNative,
    Insert,
    InsertUtf8Native,
    LowerAsciiNative,
    UpperAsciiNative,
    Substring2BytesNative,
    Substring3BytesNative,
    Substring2Utf8Native,
    Substring3Utf8Native,
    Substring2BytesLegacy,
    Substring3BytesLegacy,
    Substring2Utf8Legacy,
    Substring3Utf8Legacy,
    StrcmpNative,
    Locate2Native,
    Locate3Native,
    Locate3BytesExtNative,
    Locate3Utf8ExtNative,
    FindInSetNative,
    FindInSetPreparedNative,
    OctInt,
    OctStringNative,
    ConcatNative,
    ConcatWsNative,
    EltNative,
    FieldBytesNative,
    FieldIntNative,
    FieldRealNative,
    MakeSetNative,
    ExportSetNative,
    AbsIntNative,
    AbsUIntNative,
    AbsRealNative,
    AbsDecimalNative,
    CeilIntNative,
    FloorIntNative,
    CeilRealNative,
    FloorRealNative,
    CeilDecimalNative,
    FloorDecimalNative,
    RoundIntNative,
    RoundIntWithScaleNative,
    RoundRealNative,
    RoundDecimalNative,
    TruncateIntNative,
    TruncateUIntNative,
    TruncateIntUnsignedScaleNative,
    TruncateRealNative,
    TruncateDecimalNative,
    RoundInt128Legacy,
    RoundRealLegacy,
    RoundDecimalLegacy,
    MathNullWitnessNative,
    CharNative,
    ConvNative,
    ConvBinaryLiteralNative,
    ConvLegacy,
    SinGoNative,
    CosGoNative,
    TanGoNative,
    CotGoNative,
    AtanGoNative,
    Atan2GoNative,
    SinLibmLegacy,
    CosLibmLegacy,
    CotLibmLegacy,
    AtanLibmLegacy,
    Atan2LibmLegacy,
    ExpGoNative,
    Log10GoNative,
    CompressGoNative,
    UncompressNative,
    JsonValidTextNative,
    JsonValidBinaryNative,
    JsonValidOtherNative,
    JsonTypeTextNative,
    JsonTypeBinaryNative,
    JsonDepthNative,
    JsonStorageFreeNative,
    JsonStorageSizeNative,
    JsonQuoteNative,
    YearCoreNative,
    MonthCoreNative,
    DayOfMonthCoreNative,
    QuarterCoreNative,
    HourTextNative,
    MinuteTextNative,
    SecondTextNative,
    HourNanosNative,
    MinuteNanosNative,
    SecondNanosNative,
    MonthNameTextNative,
    TimeToSecTextNative,
    PeriodAddNative,
    PeriodDiffNative,
    GetFormatNative,
    GetFormatNullNative,
    DayOfWeekTextNative,
    WeekdayTextNative,
    DayOfYearTextNative,
    DayNameTextNative,
    DateDiffTextNative,
    DateDiffNullNative,
    DateDiffCoreNative,
    ToDaysTextNative,
    ToSecondsTextNative,
    TsoLogicalNative,
    WeekDateTextNative,
    WeekTextNative,
    YearWeekTextNative,
    WeekOfYearTextNative,
    WeekNullNative,
    WeekCoreNative,
    PasswordNative,
    Sm3Native,
    MakeDateNative,
    FromDaysNative,
    MakeTimePartsNative,
    SecToTimeNative,
    DateFormatTextNative,
    DateFormatCoreNative,
    DateFormatNullNative,
    DateFormatMissingNative,
    DurationTextProbeNative,
    TimeFormatTextNative,
    LastDayTextNative,
    IsUuidNative,
    UuidVersionNative,
    UuidTimestampNative,
    UuidToBinParseNative,
    UuidToBinSwapNative,
    BinToUuidNative,
    TranslateUtf8Native,
    TranslateBinaryNative,
    TranslateNullNative,
    SqlEncodeNative,
    SqlDecodeNative,
    SqlCryptNullNative,
    TidbShardNative,
    VitessHashNative,
    FormatBytesNative,
    FormatNanoTimeNative,
    VecAsTextNative,
    VecDimsNative,
    VecL1DistanceNative,
    VecL2DistanceNative,
    VecNegativeInnerProductNative,
    VecCosineDistanceNative,
    VecL2NormNative,
    VecFromTextNative,
    VecRealNullNative,
    LikeNative,
    IlikeNative,
    LikeLegacyNative,
    LikeNullIntNative,
    LikeMissingLegacyNative,
    RegexpLikeNative,
    RegexpSubstrNative,
    RegexpInstrNative,
    RegexpReplaceNative,
    RegexpLikeLegacyCiNative,
    RegexpLikeLegacyBinNative,
    RegexpNullIntNative,
    RegexpNullBytesNative,
    RegexpMissingLegacyNative,
    UnaryPlusIntNative,
    UnaryPlusBitsNative,
    UnaryPlusDecimalNative,
    UnaryPlusBytesNative,
    UnaryMinusIntNative,
    UnaryMinusUIntNative,
    UnaryMinusIntConstantNative,
    UnaryMinusUIntConstantNative,
    UnaryMinusBitsNative,
    UnaryMinusDecimalNative,
    UnaryNullNative,
    AddIntSsNative,
    AddIntSuNative,
    AddIntUsNative,
    AddIntUuNative,
    SubIntSsNative,
    SubIntSuNative,
    SubIntUsNative,
    SubIntUuNative,
    SubIntSuForcedNative,
    SubIntUsForcedNative,
    SubIntUuForcedNative,
    MulIntSignedNative,
    MulIntUnsignedNative,
    AddRealNative,
    SubRealNative,
    MulRealNative,
    AddDecimalNative,
    SubDecimalNative,
    MulDecimalNative,
    AddVectorNative,
    SubVectorNative,
    MulVectorNative,
    BinaryArithmeticNullNative,
    AddInt128SignedLegacy,
    AddInt128UnsignedLegacy,
    AddInt128RejectLeftLegacy,
    AddInt128RejectRightLegacy,
    SubInt128SignedLegacy,
    SubInt128UnsignedLegacy,
    SubInt128RejectLeftLegacy,
    SubInt128RejectRightLegacy,
    MulInt128SignedLegacy,
    MulInt128UnsignedLegacy,
    AddRealLegacy,
    SubRealLegacy,
    MulRealLegacy,
    AddDecimalLegacy,
    SubDecimalLegacy,
    MulDecimalLegacy,
    BinaryArithmeticMissingLegacy,
    AddDecimalFastNative,
    SubDecimalFastNative,
    MulDecimalFastNative,
    ModIntSsNative,
    ModIntSuNative,
    ModIntUsNative,
    ModIntUuNative,
    ModInt128Legacy,
    ModRealNative,
    ModRealLegacy,
    ModDecimalNative,
    DivRealNative,
    DivRealLegacy,
    DivDecimalNative,
    DivDecimalLegacy,
    AesEncrypt128EcbNative,
    AesEncrypt192EcbNative,
    AesEncrypt256EcbNative,
    AesDecrypt128EcbNative,
    AesDecrypt192EcbNative,
    AesDecrypt256EcbNative,
    AesEncrypt128CbcNative,
    AesEncrypt192CbcNative,
    AesEncrypt256CbcNative,
    AesDecrypt128CbcNative,
    AesDecrypt192CbcNative,
    AesDecrypt256CbcNative,
    AesEncrypt128OfbNative,
    AesEncrypt192OfbNative,
    AesEncrypt256OfbNative,
    AesDecrypt128OfbNative,
    AesDecrypt192OfbNative,
    AesDecrypt256OfbNative,
    AesEncrypt128CfbNative,
    AesEncrypt192CfbNative,
    AesEncrypt256CfbNative,
    AesDecrypt128CfbNative,
    AesDecrypt192CfbNative,
    AesDecrypt256CfbNative,
    AesNullNative,
    CompareIntSsNative(crate::ComparisonOp),
    CompareIntSuNative(crate::ComparisonOp),
    CompareIntUsNative(crate::ComparisonOp),
    CompareIntUuNative(crate::ComparisonOp),
    CompareInt128Legacy(crate::ComparisonOp),
    CompareRealNative(crate::ComparisonOp),
    CompareRealLegacy(crate::ComparisonOp),
    CompareDecimalNative(crate::ComparisonOp),
    CompareBytesNative(crate::ComparisonOp),
    CompareVectorNative(crate::ComparisonOp),
    CompareTimeCoreNative(crate::ComparisonOp),
    CompareDurationNative(crate::ComparisonOp),
    CompareJsonNative(crate::ComparisonOp),
    CompareNullNative,
    CompareMissingLegacy,
    GroupingBitAndNative,
    GroupingNumericCmpNative,
    GroupingNumericSetNative,
    GroupingNullNative,
    JsonContainsSerdeNative,
    JsonContainsPathSerdeNative,
    JsonOverlapsSerdeNative,
    JsonMemberOfSerdeNative,
    JsonLengthSerdeNative,
    JsonLengthPathSerdeNative,
    JsonPathExistsSerdeNative,
    JsonMemberOfBinaryLegacy,
    JsonPredicateNullNative,
    JsonPredicateMissingLegacy,
    JsonArraySerdeNative,
    JsonObjectSerdeNative,
    JsonKeysSerdeNative,
    JsonKeysPathSerdeNative,
    JsonPrettySerdeNative,
    JsonOutputNullNative,
    JsonExtractSerdeNative,
    JsonInsertSerdeNative,
    JsonSetSerdeNative,
    JsonReplaceSerdeNative,
    JsonRemoveSerdeNative,
    JsonArrayAppendSerdeNative,
    JsonArrayInsertSerdeNative,
    JsonReplaceRawLegacy,
    JsonArrayAppendRawLegacy,
    JsonArrayAppendEmptyLegacy,
    JsonValueAbsentLegacy,
    JsonUnquoteTextNative,
    JsonUnquoteBinaryNative,
    UtcDateNative,
    UtcTimestampNative,
    CurrentTimeWithoutFspNative,
    CurrentTimeWithFspNative,
    UtcTimeWithoutFspNative,
    UtcTimeWithFspNative,
    UtcTimeNullNative,
    JsonMergeSerdeNative,
    JsonMergePatchSerdeNative,
    JsonMergePatchRawLegacy,
    NowNative,
    CurrentDateNative,
    SysdateNative,
    DateCoreNative,
    DateCorePredicateLegacy,
    WeightStringNative,
    WeightStringCharNative,
    WeightStringBinaryNative,
    WeightStringNumericNative,
    FormatLocaleNative,
    AnyValueNative,
    NameConstNative,
    TidbParseTsoNative,
    TimeDiffTextNative,
    IntDivIntSsNative,
    IntDivIntUsNative,
    IntDivIntSuNative,
    IntDivIntUuNative,
    IntDivInt128Legacy,
    IntDivDecimalSignedNative,
    IntDivDecimalUnsignedNative,
    IntDivDecimalLegacy,
    TimeNative,
    MicrosecondNative,
    MicrosecondLegacy,
    AddTimeNative,
    SubTimeNative,
    TimeAddRightDatetimeNative,
    TimestampAddNative,
    TimestampAddPrefixNullNative,
    JsonSearchSerdeNative,
    DateLiteralNative,
    TimestampLiteralNative,
    ConvertTzNative,
}

/// A private recipe identity, never a consumer-provided function descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EvaluatedKernelKind {
    Wire(tipb::ScalarFuncSig),
    ClosedPrivate(crate::LocalFunctionId),
}

/// Logical admission remains distinct even when transport uses the same Bytes
/// storage. Ordinary Bytes, IEEE754 bits and time core bits cannot impersonate
/// one another, including when their physical value is NULL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EvaluatedArgsRole {
    TemporalText,
    Values,
    DecimalBinary,
    DecimalDivision,
    Int1282,
    Like,
    NativeRegexpLike,
    NativeRegexpSubstr,
    NativeRegexpInstr,
    NativeRegexpReplace,
    NativeVector,
    NativeVector2,
    Ieee754Bits,
    TimeCoreBits,
    TimeCoreBits2,
    TimeCoreBitsBytes,
    Ieee754Bits2,
    NoArgs,
    Packet,
    ReadyBytesInt,
    ReadyBytesBytesInt,
    PadPacket,
    SubstringNative,
    SubstringLegacy,
    CollatedBytes2,
    NativeSearch,
    FindInSetPrepared,
    ConcatPacked,
    EltReady,
    FieldPacked,
    MakeSetPacked,
    ExportSetPacked,
    DecimalUnary,
    DecimalInt,
    Ieee754Int,
    MakeTimeParts,
    Int128,
    NullWitness,
    CharReady,
    ConvNative,
    ConvLegacy,
}

impl EvaluatedBytesOp {
    pub(crate) fn kernel_kind(self) -> EvaluatedKernelKind {
        use tipb::ScalarFuncSig;
        let signature = match self {
            Self::ConvertTzNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::ConvertTzNative);
            }
            Self::DateLiteralNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DateLiteralNative,
                );
            }
            Self::TimestampLiteralNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TimestampLiteralNative,
                );
            }
            Self::Ascii => ScalarFuncSig::Ascii,
            Self::Length => ScalarFuncSig::Length,
            Self::BitLength => ScalarFuncSig::BitLength,
            Self::LTrim => ScalarFuncSig::LTrim,
            Self::RTrim => ScalarFuncSig::RTrim,
            Self::UnHex => ScalarFuncSig::UnHex,
            Self::Crc32 => ScalarFuncSig::Crc32,
            Self::Reverse => ScalarFuncSig::Reverse,
            Self::ReverseUtf8 => ScalarFuncSig::ReverseUtf8,
            Self::CharLength => ScalarFuncSig::CharLength,
            Self::CharLengthUtf8 => ScalarFuncSig::CharLengthUtf8,
            Self::Quote => ScalarFuncSig::Quote,
            Self::HexInt => ScalarFuncSig::HexIntArg,
            Self::HexStr => ScalarFuncSig::HexStrArg,
            Self::Bin => ScalarFuncSig::Bin,
            Self::OctInt => ScalarFuncSig::OctInt,
            Self::Left => ScalarFuncSig::Left,
            Self::LeftUtf8 => ScalarFuncSig::LeftUtf8,
            Self::Right => ScalarFuncSig::Right,
            Self::RightUtf8 => ScalarFuncSig::RightUtf8,
            Self::Replace => ScalarFuncSig::Replace,
            Self::BitCount => ScalarFuncSig::BitCount,
            Self::BitNeg => ScalarFuncSig::BitNegSig,
            Self::BitAnd => ScalarFuncSig::BitAndSig,
            Self::BitOr => ScalarFuncSig::BitOrSig,
            Self::BitXor => ScalarFuncSig::BitXorSig,
            Self::LeftShift => ScalarFuncSig::LeftShift,
            Self::RightShift => ScalarFuncSig::RightShift,
            Self::UnaryNot | Self::IsNotNull | Self::IsNotTrue | Self::IsNotFalse => {
                ScalarFuncSig::UnaryNotInt
            }
            Self::IsNull => ScalarFuncSig::IntIsNull,
            Self::IsTrue => ScalarFuncSig::IntIsTrue,
            Self::IsFalse => ScalarFuncSig::IntIsFalse,
            Self::IsTrueWithNull => ScalarFuncSig::IntIsTrueWithNull,
            Self::Md5 => ScalarFuncSig::Md5,
            Self::Sha1 => ScalarFuncSig::Sha1,
            Self::LogicalAnd => ScalarFuncSig::LogicalAnd,
            Self::LogicalOr => ScalarFuncSig::LogicalOr,
            Self::LogicalXor => ScalarFuncSig::LogicalXor,
            Self::InetAton => ScalarFuncSig::InetAton,
            Self::InetNtoa => ScalarFuncSig::InetNtoa,
            Self::Inet6Aton => ScalarFuncSig::Inet6Aton,
            Self::Inet6Ntoa => ScalarFuncSig::Inet6Ntoa,
            Self::Lower => ScalarFuncSig::Lower,
            Self::Upper => ScalarFuncSig::Upper,
            Self::Insert => ScalarFuncSig::Insert,
            Self::AsinRaw => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AsinRaw);
            }
            Self::AcosRaw => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AcosRaw);
            }
            Self::SqrtRaw => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SqrtRaw);
            }
            Self::SignRaw => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SignRaw);
            }
            Self::RadiansRaw => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::RadiansRaw);
            }
            Self::DegreesRaw => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::DegreesRaw);
            }
            Self::PiRaw => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::PiRaw);
            }
            Self::IsIpv4Nullable => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::IsIpv4Nullable);
            }
            Self::IsIpv6Nullable => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::IsIpv6Nullable);
            }
            Self::IsIpv4CompatNullable => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::IsIpv4CompatNullable,
                );
            }
            Self::IsIpv4MappedNullable => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::IsIpv4MappedNullable,
                );
            }
            Self::SpaceNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SpaceNative);
            }
            Self::RepeatNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::RepeatNative);
            }
            Self::ToBase64Native => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::ToBase64Native);
            }
            Self::FromBase64Native => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::FromBase64Native,
                );
            }
            Self::FromBase64ValueNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::FromBase64ValueNative,
                );
            }
            Self::LowerUtf8Ready => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::LowerUtf8Ready);
            }
            Self::UpperUtf8Ready => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::UpperUtf8Ready);
            }
            Self::Sha2Native => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::Sha2Native);
            }
            Self::OrdNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::OrdNative);
            }
            Self::TrimBothNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::TrimBothNative);
            }
            Self::TrimLeadingNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TrimLeadingNative,
                );
            }
            Self::TrimTrailingNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TrimTrailingNative,
                );
            }
            Self::SubstringIndexSignedNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::SubstringIndexSignedNative,
                );
            }
            Self::SubstringIndexUnsignedNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::SubstringIndexUnsignedNative,
                );
            }
            Self::LpadBytesNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::LpadBytesNative);
            }
            Self::RpadBytesNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::RpadBytesNative);
            }
            Self::LpadUtf8Native => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::LpadUtf8Native);
            }
            Self::RpadUtf8Native => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::RpadUtf8Native);
            }
            Self::LnNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::LnNative);
            }
            Self::LogNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::LogNative);
            }
            Self::Log2Native => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::Log2Native);
            }
            Self::PowNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::PowNative);
            }
            Self::UncompressedLengthNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UncompressedLengthNative,
                );
            }
            Self::InsertUtf8Native => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::InsertUtf8Native,
                );
            }
            Self::LowerAsciiNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::LowerAsciiNative,
                );
            }
            Self::UpperAsciiNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UpperAsciiNative,
                );
            }
            Self::Substring2BytesNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::Substring2BytesNative,
                );
            }
            Self::Substring3BytesNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::Substring3BytesNative,
                );
            }
            Self::Substring2Utf8Native => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::Substring2Utf8Native,
                );
            }
            Self::Substring3Utf8Native => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::Substring3Utf8Native,
                );
            }
            Self::Substring2BytesLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::Substring2BytesLegacy,
                );
            }
            Self::Substring3BytesLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::Substring3BytesLegacy,
                );
            }
            Self::Substring2Utf8Legacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::Substring2Utf8Legacy,
                );
            }
            Self::Substring3Utf8Legacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::Substring3Utf8Legacy,
                );
            }
            Self::StrcmpNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::StrcmpNative);
            }
            Self::Locate2Native => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::Locate2Native);
            }
            Self::Locate3Native => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::Locate3Native);
            }
            Self::Locate3BytesExtNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::Locate3BytesExtNative,
                );
            }
            Self::Locate3Utf8ExtNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::Locate3Utf8ExtNative,
                );
            }
            Self::FindInSetNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::FindInSetNative);
            }
            Self::FindInSetPreparedNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::FindInSetPreparedNative,
                );
            }
            Self::OctStringNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::OctStringNative);
            }
            Self::ConcatNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::ConcatNative);
            }
            Self::ConcatWsNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::ConcatWsNative);
            }
            Self::EltNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::EltNative);
            }
            Self::FieldBytesNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::FieldBytesNative,
                );
            }
            Self::FieldIntNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::FieldIntNative);
            }
            Self::FieldRealNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::FieldRealNative);
            }
            Self::MakeSetNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::MakeSetNative);
            }
            Self::ExportSetNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::ExportSetNative);
            }
            Self::AbsIntNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AbsIntNative);
            }
            Self::AbsUIntNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AbsUIntNative);
            }
            Self::AbsRealNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AbsRealNative);
            }
            Self::AbsDecimalNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AbsDecimalNative,
                );
            }
            Self::CeilIntNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::CeilIntNative);
            }
            Self::FloorIntNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::FloorIntNative);
            }
            Self::CeilRealNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::CeilRealNative);
            }
            Self::FloorRealNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::FloorRealNative);
            }
            Self::CeilDecimalNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CeilDecimalNative,
                );
            }
            Self::FloorDecimalNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::FloorDecimalNative,
                );
            }
            Self::RoundIntNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::RoundIntNative);
            }
            Self::RoundIntWithScaleNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::RoundIntWithScaleNative,
                );
            }
            Self::RoundRealNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::RoundRealNative);
            }
            Self::RoundDecimalNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::RoundDecimalNative,
                );
            }
            Self::TruncateIntNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TruncateIntNative,
                );
            }
            Self::TruncateUIntNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TruncateUIntNative,
                );
            }
            Self::TruncateIntUnsignedScaleNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TruncateIntUnsignedScaleNative,
                );
            }
            Self::TruncateRealNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TruncateRealNative,
                );
            }
            Self::TruncateDecimalNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TruncateDecimalNative,
                );
            }
            Self::RoundInt128Legacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::RoundInt128Legacy,
                );
            }
            Self::RoundRealLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::RoundRealLegacy);
            }
            Self::RoundDecimalLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::RoundDecimalLegacy,
                );
            }
            Self::MathNullWitnessNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::MathNullWitnessNative,
                );
            }
            Self::CharNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::CharNative);
            }
            Self::ConvNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::ConvNative);
            }
            Self::ConvBinaryLiteralNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::ConvBinaryLiteralNative,
                );
            }
            Self::ConvLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::ConvLegacy);
            }
            Self::SinGoNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SinGoNative);
            }
            Self::CosGoNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::CosGoNative);
            }
            Self::TanGoNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::TanGoNative);
            }
            Self::CotGoNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::CotGoNative);
            }
            Self::AtanGoNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AtanGoNative);
            }
            Self::Atan2GoNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::Atan2GoNative);
            }
            Self::SinLibmLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SinLibmLegacy);
            }
            Self::CosLibmLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::CosLibmLegacy);
            }
            Self::CotLibmLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::CotLibmLegacy);
            }
            Self::AtanLibmLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AtanLibmLegacy);
            }
            Self::Atan2LibmLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::Atan2LibmLegacy);
            }
            Self::ExpGoNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::ExpGoNative);
            }
            Self::Log10GoNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::Log10GoNative);
            }
            Self::CompressGoNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompressGoNative,
                );
            }
            Self::UncompressNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UncompressNative,
                );
            }
            Self::JsonValidTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonValidTextNative,
                );
            }
            Self::JsonValidBinaryNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonValidBinaryNative,
                );
            }
            Self::JsonValidOtherNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonValidOtherNative,
                );
            }
            Self::JsonTypeTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonTypeTextNative,
                );
            }
            Self::JsonTypeBinaryNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonTypeBinaryNative,
                );
            }
            Self::JsonDepthNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::JsonDepthNative);
            }
            Self::JsonStorageFreeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonStorageFreeNative,
                );
            }
            Self::JsonStorageSizeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonStorageSizeNative,
                );
            }
            Self::JsonQuoteNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::JsonQuoteNative);
            }
            Self::YearCoreNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::YearCoreNative);
            }
            Self::MonthCoreNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::MonthCoreNative);
            }
            Self::DayOfMonthCoreNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DayOfMonthCoreNative,
                );
            }
            Self::QuarterCoreNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::QuarterCoreNative,
                );
            }
            Self::HourTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::HourTextNative);
            }
            Self::MinuteTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::MinuteTextNative,
                );
            }
            Self::SecondTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::SecondTextNative,
                );
            }
            Self::HourNanosNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::HourNanosNative);
            }
            Self::MinuteNanosNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::MinuteNanosNative,
                );
            }
            Self::SecondNanosNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::SecondNanosNative,
                );
            }
            Self::MonthNameTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::MonthNameTextNative,
                );
            }
            Self::TimeToSecTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TimeToSecTextNative,
                );
            }
            Self::PeriodAddNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::PeriodAddNative);
            }
            Self::PeriodDiffNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::PeriodDiffNative,
                );
            }
            Self::GetFormatNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::GetFormatNative);
            }
            Self::GetFormatNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::GetFormatNullNative,
                );
            }
            Self::DayOfWeekTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DayOfWeekTextNative,
                );
            }
            Self::WeekdayTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::WeekdayTextNative,
                );
            }
            Self::DayOfYearTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DayOfYearTextNative,
                );
            }
            Self::DayNameTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DayNameTextNative,
                );
            }
            Self::DateDiffTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DateDiffTextNative,
                );
            }
            Self::DateDiffNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DateDiffNullNative,
                );
            }
            Self::DateDiffCoreNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DateDiffCoreNative,
                );
            }
            Self::ToDaysTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::ToDaysTextNative,
                );
            }
            Self::ToSecondsTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::ToSecondsTextNative,
                );
            }
            Self::TsoLogicalNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TsoLogicalNative,
                );
            }
            Self::WeekDateTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::WeekDateTextNative,
                );
            }
            Self::WeekTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::WeekTextNative);
            }
            Self::YearWeekTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::YearWeekTextNative,
                );
            }
            Self::WeekOfYearTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::WeekOfYearTextNative,
                );
            }
            Self::WeekNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::WeekNullNative);
            }
            Self::WeekCoreNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::WeekCoreNative);
            }
            Self::PasswordNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::PasswordNative);
            }
            Self::Sm3Native => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::Sm3Native);
            }
            Self::MakeDateNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::MakeDateNative);
            }
            Self::FromDaysNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::FromDaysNative);
            }
            Self::MakeTimePartsNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::MakeTimePartsNative,
                );
            }
            Self::SecToTimeNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SecToTimeNative);
            }
            Self::DateFormatTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DateFormatTextNative,
                );
            }
            Self::DateFormatCoreNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DateFormatCoreNative,
                );
            }
            Self::DateFormatNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DateFormatNullNative,
                );
            }
            Self::DateFormatMissingNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DateFormatMissingNative,
                );
            }
            Self::DurationTextProbeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DurationTextProbeNative,
                );
            }
            Self::TimeFormatTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TimeFormatTextNative,
                );
            }
            Self::LastDayTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::LastDayTextNative,
                );
            }
            Self::IsUuidNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::IsUuidNative);
            }
            Self::UuidVersionNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UuidVersionNative,
                );
            }
            Self::UuidTimestampNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UuidTimestampNative,
                );
            }
            Self::UuidToBinParseNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UuidToBinParseNative,
                );
            }
            Self::UuidToBinSwapNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UuidToBinSwapNative,
                );
            }
            Self::BinToUuidNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::BinToUuidNative);
            }
            Self::TranslateUtf8Native => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TranslateUtf8Native,
                );
            }
            Self::TranslateBinaryNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TranslateBinaryNative,
                );
            }
            Self::TranslateNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TranslateNullNative,
                );
            }
            Self::SqlEncodeNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SqlEncodeNative);
            }
            Self::SqlDecodeNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SqlDecodeNative);
            }
            Self::SqlCryptNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::SqlCryptNullNative,
                );
            }
            Self::TidbShardNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::TidbShardNative);
            }
            Self::VitessHashNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::VitessHashNative,
                );
            }
            Self::FormatBytesNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::FormatBytesNative,
                );
            }
            Self::FormatNanoTimeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::FormatNanoTimeNative,
                );
            }
            Self::VecAsTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::VecAsTextNative);
            }
            Self::VecDimsNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::VecDimsNative);
            }
            Self::VecL1DistanceNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::VecL1DistanceNative,
                );
            }
            Self::VecL2DistanceNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::VecL2DistanceNative,
                );
            }
            Self::VecNegativeInnerProductNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::VecNegativeInnerProductNative,
                );
            }
            Self::VecCosineDistanceNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::VecCosineDistanceNative,
                );
            }
            Self::VecL2NormNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::VecL2NormNative);
            }
            Self::VecFromTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::VecFromTextNative,
                );
            }
            Self::VecRealNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::VecRealNullNative,
                );
            }
            Self::LikeNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::LikeNative);
            }
            Self::IlikeNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::IlikeNative);
            }
            Self::LikeLegacyNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::LikeLegacyNative,
                );
            }
            Self::LikeNullIntNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::LikeNullIntNative,
                );
            }
            Self::LikeMissingLegacyNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::LikeMissingLegacyNative,
                );
            }
            Self::RegexpLikeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::RegexpLikeNative,
                );
            }
            Self::RegexpSubstrNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::RegexpSubstrNative,
                );
            }
            Self::RegexpInstrNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::RegexpInstrNative,
                );
            }
            Self::RegexpReplaceNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::RegexpReplaceNative,
                );
            }
            Self::RegexpLikeLegacyCiNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::RegexpLikeLegacyCiNative,
                );
            }
            Self::RegexpLikeLegacyBinNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::RegexpLikeLegacyBinNative,
                );
            }
            Self::RegexpNullIntNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::RegexpNullIntNative,
                );
            }
            Self::RegexpNullBytesNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::RegexpNullBytesNative,
                );
            }
            Self::RegexpMissingLegacyNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::RegexpMissingLegacyNative,
                );
            }
            Self::UnaryPlusIntNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UnaryPlusIntNative,
                );
            }
            Self::UnaryPlusBitsNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UnaryPlusBitsNative,
                );
            }
            Self::UnaryPlusDecimalNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UnaryPlusDecimalNative,
                );
            }
            Self::UnaryPlusBytesNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UnaryPlusBytesNative,
                );
            }
            Self::UnaryMinusIntNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UnaryMinusIntNative,
                );
            }
            Self::UnaryMinusUIntNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UnaryMinusUIntNative,
                );
            }
            Self::UnaryMinusIntConstantNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UnaryMinusIntConstantNative,
                );
            }
            Self::UnaryMinusUIntConstantNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UnaryMinusUIntConstantNative,
                );
            }
            Self::UnaryMinusBitsNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UnaryMinusBitsNative,
                );
            }
            Self::UnaryMinusDecimalNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UnaryMinusDecimalNative,
                );
            }
            Self::UnaryNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::UnaryNullNative);
            }
            Self::AddIntSsNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AddIntSsNative);
            }
            Self::AddIntSuNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AddIntSuNative);
            }
            Self::AddIntUsNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AddIntUsNative);
            }
            Self::AddIntUuNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AddIntUuNative);
            }
            Self::SubIntSsNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SubIntSsNative);
            }
            Self::SubIntSuNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SubIntSuNative);
            }
            Self::SubIntUsNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SubIntUsNative);
            }
            Self::SubIntUuNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SubIntUuNative);
            }
            Self::SubIntSuForcedNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::SubIntSuForcedNative,
                );
            }
            Self::SubIntUsForcedNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::SubIntUsForcedNative,
                );
            }
            Self::SubIntUuForcedNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::SubIntUuForcedNative,
                );
            }
            Self::MulIntSignedNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::MulIntSignedNative,
                );
            }
            Self::MulIntUnsignedNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::MulIntUnsignedNative,
                );
            }
            Self::AddRealNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AddRealNative);
            }
            Self::SubRealNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SubRealNative);
            }
            Self::MulRealNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::MulRealNative);
            }
            Self::AddDecimalNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AddDecimalNative,
                );
            }
            Self::SubDecimalNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::SubDecimalNative,
                );
            }
            Self::MulDecimalNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::MulDecimalNative,
                );
            }
            Self::AddVectorNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AddVectorNative);
            }
            Self::SubVectorNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SubVectorNative);
            }
            Self::MulVectorNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::MulVectorNative);
            }
            Self::BinaryArithmeticNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::BinaryArithmeticNullNative,
                );
            }
            Self::AddInt128SignedLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AddInt128SignedLegacy,
                );
            }
            Self::AddInt128UnsignedLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AddInt128UnsignedLegacy,
                );
            }
            Self::AddInt128RejectLeftLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AddInt128RejectLeftLegacy,
                );
            }
            Self::AddInt128RejectRightLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AddInt128RejectRightLegacy,
                );
            }
            Self::SubInt128SignedLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::SubInt128SignedLegacy,
                );
            }
            Self::SubInt128UnsignedLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::SubInt128UnsignedLegacy,
                );
            }
            Self::SubInt128RejectLeftLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::SubInt128RejectLeftLegacy,
                );
            }
            Self::SubInt128RejectRightLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::SubInt128RejectRightLegacy,
                );
            }
            Self::MulInt128SignedLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::MulInt128SignedLegacy,
                );
            }
            Self::MulInt128UnsignedLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::MulInt128UnsignedLegacy,
                );
            }
            Self::AddRealLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AddRealLegacy);
            }
            Self::SubRealLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SubRealLegacy);
            }
            Self::MulRealLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::MulRealLegacy);
            }
            Self::AddDecimalLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AddDecimalLegacy,
                );
            }
            Self::SubDecimalLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::SubDecimalLegacy,
                );
            }
            Self::MulDecimalLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::MulDecimalLegacy,
                );
            }
            Self::BinaryArithmeticMissingLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::BinaryArithmeticMissingLegacy,
                );
            }
            Self::AddDecimalFastNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AddDecimalFastNative,
                );
            }
            Self::SubDecimalFastNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::SubDecimalFastNative,
                );
            }
            Self::MulDecimalFastNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::MulDecimalFastNative,
                );
            }
            Self::ModIntSsNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::ModIntSsNative);
            }
            Self::ModIntSuNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::ModIntSuNative);
            }
            Self::ModIntUsNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::ModIntUsNative);
            }
            Self::ModIntUuNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::ModIntUuNative);
            }
            Self::ModInt128Legacy => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::ModInt128Legacy);
            }
            Self::ModRealNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::ModRealNative);
            }
            Self::ModRealLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::ModRealLegacy);
            }
            Self::ModDecimalNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::ModDecimalNative,
                );
            }
            Self::DivRealNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::DivRealNative);
            }
            Self::DivRealLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::DivRealLegacy);
            }
            Self::DivDecimalNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DivDecimalNative,
                );
            }
            Self::DivDecimalLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DivDecimalLegacy,
                );
            }
            Self::AesEncrypt128EcbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesEncrypt128EcbNative,
                );
            }
            Self::AesEncrypt192EcbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesEncrypt192EcbNative,
                );
            }
            Self::AesEncrypt256EcbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesEncrypt256EcbNative,
                );
            }
            Self::AesDecrypt128EcbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesDecrypt128EcbNative,
                );
            }
            Self::AesDecrypt192EcbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesDecrypt192EcbNative,
                );
            }
            Self::AesDecrypt256EcbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesDecrypt256EcbNative,
                );
            }
            Self::AesEncrypt128CbcNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesEncrypt128CbcNative,
                );
            }
            Self::AesEncrypt192CbcNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesEncrypt192CbcNative,
                );
            }
            Self::AesEncrypt256CbcNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesEncrypt256CbcNative,
                );
            }
            Self::AesDecrypt128CbcNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesDecrypt128CbcNative,
                );
            }
            Self::AesDecrypt192CbcNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesDecrypt192CbcNative,
                );
            }
            Self::AesDecrypt256CbcNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesDecrypt256CbcNative,
                );
            }
            Self::AesEncrypt128OfbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesEncrypt128OfbNative,
                );
            }
            Self::AesEncrypt192OfbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesEncrypt192OfbNative,
                );
            }
            Self::AesEncrypt256OfbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesEncrypt256OfbNative,
                );
            }
            Self::AesDecrypt128OfbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesDecrypt128OfbNative,
                );
            }
            Self::AesDecrypt192OfbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesDecrypt192OfbNative,
                );
            }
            Self::AesDecrypt256OfbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesDecrypt256OfbNative,
                );
            }
            Self::AesEncrypt128CfbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesEncrypt128CfbNative,
                );
            }
            Self::AesEncrypt192CfbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesEncrypt192CfbNative,
                );
            }
            Self::AesEncrypt256CfbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesEncrypt256CfbNative,
                );
            }
            Self::AesDecrypt128CfbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesDecrypt128CfbNative,
                );
            }
            Self::AesDecrypt192CfbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesDecrypt192CfbNative,
                );
            }
            Self::AesDecrypt256CfbNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::AesDecrypt256CfbNative,
                );
            }
            Self::AesNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AesNullNative);
            }
            Self::CompareIntSsNative(op) => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompareIntSsNative(op),
                );
            }
            Self::CompareIntSuNative(op) => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompareIntSuNative(op),
                );
            }
            Self::CompareIntUsNative(op) => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompareIntUsNative(op),
                );
            }
            Self::CompareIntUuNative(op) => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompareIntUuNative(op),
                );
            }
            Self::CompareInt128Legacy(op) => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompareInt128Legacy(op),
                );
            }
            Self::CompareRealNative(op) => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompareRealNative(op),
                );
            }
            Self::CompareRealLegacy(op) => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompareRealLegacy(op),
                );
            }
            Self::CompareDecimalNative(op) => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompareDecimalNative(op),
                );
            }
            Self::CompareBytesNative(op) => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompareBytesNative(op),
                );
            }
            Self::CompareVectorNative(op) => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompareVectorNative(op),
                );
            }
            Self::CompareTimeCoreNative(op) => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompareTimeCoreNative(op),
                );
            }
            Self::CompareDurationNative(op) => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompareDurationNative(op),
                );
            }
            Self::CompareJsonNative(op) => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompareJsonNative(op),
                );
            }
            Self::CompareNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompareNullNative,
                );
            }
            Self::CompareMissingLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CompareMissingLegacy,
                );
            }
            Self::GroupingBitAndNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::GroupingBitAndNative,
                );
            }
            Self::GroupingNumericCmpNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::GroupingNumericCmpNative,
                );
            }
            Self::GroupingNumericSetNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::GroupingNumericSetNative,
                );
            }
            Self::GroupingNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::GroupingNullNative,
                );
            }
            Self::JsonContainsSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonContainsSerdeNative,
                );
            }
            Self::JsonContainsPathSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonContainsPathSerdeNative,
                );
            }
            Self::JsonOverlapsSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonOverlapsSerdeNative,
                );
            }
            Self::JsonMemberOfSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonMemberOfSerdeNative,
                );
            }
            Self::JsonLengthSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonLengthSerdeNative,
                );
            }
            Self::JsonLengthPathSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonLengthPathSerdeNative,
                );
            }
            Self::JsonPathExistsSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonPathExistsSerdeNative,
                );
            }
            Self::JsonMemberOfBinaryLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonMemberOfBinaryLegacy,
                );
            }
            Self::JsonPredicateNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonPredicateNullNative,
                );
            }
            Self::JsonPredicateMissingLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonPredicateMissingLegacy,
                );
            }
            Self::JsonArraySerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonArraySerdeNative,
                );
            }
            Self::JsonObjectSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonObjectSerdeNative,
                );
            }
            Self::JsonKeysSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonKeysSerdeNative,
                );
            }
            Self::JsonKeysPathSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonKeysPathSerdeNative,
                );
            }
            Self::JsonPrettySerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonPrettySerdeNative,
                );
            }
            Self::JsonOutputNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonOutputNullNative,
                );
            }
            Self::JsonExtractSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonExtractSerdeNative,
                );
            }
            Self::JsonInsertSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonInsertSerdeNative,
                );
            }
            Self::JsonSetSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonSetSerdeNative,
                );
            }
            Self::JsonReplaceSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonReplaceSerdeNative,
                );
            }
            Self::JsonRemoveSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonRemoveSerdeNative,
                );
            }
            Self::JsonArrayAppendSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonArrayAppendSerdeNative,
                );
            }
            Self::JsonArrayInsertSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonArrayInsertSerdeNative,
                );
            }
            Self::JsonReplaceRawLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonReplaceRawLegacy,
                );
            }
            Self::JsonArrayAppendRawLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonArrayAppendRawLegacy,
                );
            }
            Self::JsonArrayAppendEmptyLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonArrayAppendEmptyLegacy,
                );
            }
            Self::JsonValueAbsentLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonValueAbsentLegacy,
                );
            }
            Self::JsonUnquoteTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonUnquoteTextNative,
                );
            }
            Self::JsonUnquoteBinaryNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonUnquoteBinaryNative,
                );
            }
            Self::UtcDateNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::UtcDateNative);
            }
            Self::UtcTimestampNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UtcTimestampNative,
                );
            }
            Self::CurrentTimeWithoutFspNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CurrentTimeWithoutFspNative,
                );
            }
            Self::CurrentTimeWithFspNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CurrentTimeWithFspNative,
                );
            }
            Self::UtcTimeWithoutFspNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UtcTimeWithoutFspNative,
                );
            }
            Self::UtcTimeWithFspNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UtcTimeWithFspNative,
                );
            }
            Self::UtcTimeNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::UtcTimeNullNative,
                );
            }
            Self::JsonMergeSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonMergeSerdeNative,
                );
            }
            Self::JsonMergePatchSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonMergePatchSerdeNative,
                );
            }
            Self::JsonMergePatchRawLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonMergePatchRawLegacy,
                );
            }
            Self::NowNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::NowNative);
            }
            Self::CurrentDateNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::CurrentDateNative,
                );
            }
            Self::SysdateNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SysdateNative);
            }
            Self::DateCoreNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::DateCoreNative);
            }
            Self::DateCorePredicateLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::DateCorePredicateLegacy,
                );
            }
            Self::WeightStringNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::WeightStringNative,
                );
            }
            Self::WeightStringCharNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::WeightStringCharNative,
                );
            }
            Self::WeightStringBinaryNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::WeightStringBinaryNative,
                );
            }
            Self::WeightStringNumericNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::WeightStringNumericNative,
                );
            }
            Self::FormatLocaleNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::FormatLocaleNative,
                );
            }
            Self::AnyValueNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AnyValueNative);
            }
            Self::NameConstNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::NameConstNative);
            }
            Self::TidbParseTsoNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TidbParseTsoNative,
                );
            }
            Self::TimeDiffTextNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TimeDiffTextNative,
                );
            }
            Self::IntDivIntSsNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::IntDivIntSsNative,
                );
            }
            Self::IntDivIntUsNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::IntDivIntUsNative,
                );
            }
            Self::IntDivIntSuNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::IntDivIntSuNative,
                );
            }
            Self::IntDivIntUuNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::IntDivIntUuNative,
                );
            }
            Self::IntDivInt128Legacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::IntDivInt128Legacy,
                );
            }
            Self::IntDivDecimalSignedNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::IntDivDecimalSignedNative,
                );
            }
            Self::IntDivDecimalUnsignedNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::IntDivDecimalUnsignedNative,
                );
            }
            Self::IntDivDecimalLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::IntDivDecimalLegacy,
                );
            }
            Self::TimeNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::TimeNative);
            }
            Self::MicrosecondNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::MicrosecondNative,
                );
            }
            Self::MicrosecondLegacy => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::MicrosecondLegacy,
                );
            }
            Self::AddTimeNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::AddTimeNative);
            }
            Self::SubTimeNative => {
                return EvaluatedKernelKind::ClosedPrivate(crate::LocalFunctionId::SubTimeNative);
            }
            Self::TimeAddRightDatetimeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TimeAddRightDatetimeNative,
                );
            }
            Self::TimestampAddNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TimestampAddNative,
                );
            }
            Self::TimestampAddPrefixNullNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::TimestampAddPrefixNullNative,
                );
            }
            Self::JsonSearchSerdeNative => {
                return EvaluatedKernelKind::ClosedPrivate(
                    crate::LocalFunctionId::JsonSearchSerdeNative,
                );
            }
        };
        EvaluatedKernelKind::Wire(signature)
    }

    pub(crate) fn function_ref(self) -> crate::FunctionRef {
        match self.kernel_kind() {
            EvaluatedKernelKind::Wire(signature) => crate::FunctionRef::TiPb(signature),
            EvaluatedKernelKind::ClosedPrivate(id) => crate::FunctionRef::Local(id),
        }
    }

    pub(crate) fn regexp_kind(self) -> Option<NativeRegexpKind> {
        match self {
            Self::RegexpLikeNative => Some(NativeRegexpKind::Like),
            Self::RegexpSubstrNative => Some(NativeRegexpKind::Substr),
            Self::RegexpInstrNative => Some(NativeRegexpKind::Instr),
            Self::RegexpReplaceNative => Some(NativeRegexpKind::Replace),
            _ => None,
        }
    }

    pub(crate) fn like_kind(self) -> Option<NativeLikeKind> {
        match self {
            Self::LikeNative => Some(NativeLikeKind::Like),
            Self::IlikeNative => Some(NativeLikeKind::Ilike),
            Self::LikeLegacyNative => Some(NativeLikeKind::Legacy),
            _ => None,
        }
    }

    pub(crate) fn is_weight_or_format_native(self) -> bool {
        matches!(
            self,
            Self::WeightStringNative
                | Self::WeightStringCharNative
                | Self::WeightStringBinaryNative
                | Self::WeightStringNumericNative
                | Self::FormatLocaleNative
        )
    }

    pub(crate) fn weight_or_format_args_valid(
        self,
        first: Option<&[u8]>,
        second: Option<&[u8]>,
        number: Option<i64>,
    ) -> bool {
        match (self, first, second, number) {
            (Self::WeightStringNative, input, metadata, None) => {
                crate::native_weight_string_args_valid(input, metadata)
            }
            (Self::WeightStringCharNative, input, metadata, None) => {
                crate::native_weight_char_args_valid(input, metadata)
            }
            (Self::WeightStringBinaryNative, input, metadata, None) => {
                crate::native_weight_binary_args_valid(input, metadata)
            }
            (Self::WeightStringNumericNative, None, None, kind) => {
                crate::native_weight_numeric_type_valid(kind)
            }
            (Self::FormatLocaleNative, input, locale, precision) => {
                crate::impl_string::format_locale_native_args_valid(input, locale, precision)
            }
            _ => false,
        }
    }

    pub(crate) fn is_clock_value(self) -> bool {
        matches!(
            self,
            Self::UtcDateNative
                | Self::UtcTimestampNative
                | Self::CurrentTimeWithoutFspNative
                | Self::CurrentTimeWithFspNative
                | Self::UtcTimeWithoutFspNative
                | Self::UtcTimeWithFspNative
                | Self::NowNative
                | Self::CurrentDateNative
                | Self::SysdateNative
        )
    }

    pub(crate) fn clock_args_valid(self, clock: Option<&[u8]>, fsp: Option<i64>) -> bool {
        match (self, clock, fsp) {
            (
                Self::UtcDateNative
                | Self::CurrentTimeWithoutFspNative
                | Self::UtcTimeWithoutFspNative
                | Self::CurrentDateNative,
                Some(clock),
                None,
            ) => crate::impl_time::native_clock_args_valid(clock),
            (
                Self::UtcTimestampNative
                | Self::CurrentTimeWithFspNative
                | Self::UtcTimeWithFspNative
                | Self::NowNative
                | Self::SysdateNative,
                Some(clock),
                Some(fsp),
            ) => crate::impl_time::native_clock_fsp_args_valid(clock, fsp),
            _ => false,
        }
    }

    pub(crate) fn is_json_output_value(self) -> bool {
        matches!(
            self,
            Self::JsonArraySerdeNative
                | Self::JsonSearchSerdeNative
                | Self::JsonObjectSerdeNative
                | Self::JsonKeysSerdeNative
                | Self::JsonKeysPathSerdeNative
                | Self::JsonPrettySerdeNative
                | Self::JsonExtractSerdeNative
                | Self::JsonInsertSerdeNative
                | Self::JsonSetSerdeNative
                | Self::JsonReplaceSerdeNative
                | Self::JsonRemoveSerdeNative
                | Self::JsonArrayAppendSerdeNative
                | Self::JsonArrayInsertSerdeNative
                | Self::JsonReplaceRawLegacy
                | Self::JsonArrayAppendRawLegacy
                | Self::JsonArrayAppendEmptyLegacy
                | Self::JsonUnquoteTextNative
                | Self::JsonUnquoteBinaryNative
                | Self::JsonMergeSerdeNative
                | Self::JsonMergePatchSerdeNative
                | Self::JsonMergePatchRawLegacy
        )
    }

    pub(crate) fn json_output_args_valid(self, values: &[Option<&[u8]>]) -> bool {
        use crate::impl_json::{
            json_array_append_empty_legacy_args_valid, json_array_append_raw_legacy_args_valid,
            json_array_append_serde_args_valid, json_array_insert_serde_args_valid,
            json_array_serde_args_valid, json_extract_serde_args_valid,
            json_merge_patch_raw_legacy_args_valid, json_merge_patch_serde_args_valid,
            json_modify_serde_args_valid, json_object_serde_args_valid,
            json_remove_serde_args_valid, json_replace_raw_legacy_args_valid,
            json_serde_native_args_valid, json_unquote_binary_native_args_valid,
            json_unquote_text_native_args_valid,
        };
        match (self, values) {
            (Self::JsonSearchSerdeNative, [document, paths, spec]) => {
                crate::impl_json::json_search_native_args_valid(*document, *paths, *spec)
            }
            (Self::JsonArraySerdeNative | Self::JsonMergeSerdeNative, [Some(values)]) => {
                json_array_serde_args_valid(values)
            }
            (Self::JsonMergePatchSerdeNative, [Some(values)]) => {
                json_merge_patch_serde_args_valid(values)
            }
            (Self::JsonMergePatchRawLegacy, [Some(values)]) => {
                json_merge_patch_raw_legacy_args_valid(values)
            }
            (Self::JsonObjectSerdeNative, [Some(pairs)]) => json_object_serde_args_valid(pairs),
            (Self::JsonKeysSerdeNative | Self::JsonPrettySerdeNative, [Some(document)]) => {
                json_serde_native_args_valid(document, None, None)
            }
            (Self::JsonKeysPathSerdeNative, [Some(document), Some(path)]) => {
                json_serde_native_args_valid(document, None, Some((path, false)))
            }
            (Self::JsonExtractSerdeNative, [Some(document), Some(paths)]) => {
                json_extract_serde_args_valid(document, paths)
            }
            (Self::JsonRemoveSerdeNative, [Some(document), Some(paths)]) => {
                json_remove_serde_args_valid(document, paths)
            }
            (
                Self::JsonInsertSerdeNative
                | Self::JsonSetSerdeNative
                | Self::JsonReplaceSerdeNative,
                [Some(document), Some(paths), Some(values)],
            ) => json_modify_serde_args_valid(document, paths, values),
            (Self::JsonArrayAppendSerdeNative, [Some(document), Some(paths), Some(values)]) => {
                json_array_append_serde_args_valid(document, paths, values)
            }
            (Self::JsonArrayInsertSerdeNative, [Some(document), Some(paths), Some(values)]) => {
                json_array_insert_serde_args_valid(document, paths, values)
            }
            (Self::JsonReplaceRawLegacy, [Some(document), Some(paths), Some(values)]) => {
                json_replace_raw_legacy_args_valid(document, paths, values)
            }
            (Self::JsonArrayAppendRawLegacy, [Some(document), Some(paths), Some(values)]) => {
                json_array_append_raw_legacy_args_valid(document, paths, values)
            }
            (Self::JsonArrayAppendEmptyLegacy, [Some(document)]) => {
                json_array_append_empty_legacy_args_valid(document)
            }
            (Self::JsonUnquoteTextNative, [Some(text)]) => {
                json_unquote_text_native_args_valid(text)
            }
            (Self::JsonUnquoteBinaryNative, [Some(raw)]) => {
                json_unquote_binary_native_args_valid(raw)
            }
            _ => false,
        }
    }

    pub(crate) fn is_json_predicate_value(self) -> bool {
        matches!(
            self,
            Self::JsonContainsSerdeNative
                | Self::JsonContainsPathSerdeNative
                | Self::JsonOverlapsSerdeNative
                | Self::JsonMemberOfSerdeNative
                | Self::JsonLengthSerdeNative
                | Self::JsonLengthPathSerdeNative
                | Self::JsonPathExistsSerdeNative
                | Self::JsonMemberOfBinaryLegacy
        )
    }

    pub(crate) fn json_predicate_args_valid(self, values: &[Option<&[u8]>]) -> bool {
        use crate::impl_json::{
            json_member_binary_legacy_args_valid, json_serde_native_args_valid,
        };
        match (self, values) {
            (
                Self::JsonContainsSerdeNative
                | Self::JsonOverlapsSerdeNative
                | Self::JsonMemberOfSerdeNative,
                [Some(first), Some(second)],
            ) => json_serde_native_args_valid(first, Some(second), None),
            (Self::JsonContainsPathSerdeNative, [Some(first), Some(second), Some(path)]) => {
                json_serde_native_args_valid(first, Some(second), Some((path, false)))
            }
            (Self::JsonLengthSerdeNative, [Some(first)]) => {
                json_serde_native_args_valid(first, None, None)
            }
            (Self::JsonLengthPathSerdeNative, [Some(first), Some(path)]) => {
                json_serde_native_args_valid(first, None, Some((path, false)))
            }
            (Self::JsonPathExistsSerdeNative, [Some(first), Some(path)]) => {
                json_serde_native_args_valid(first, None, Some((path, true)))
            }
            (Self::JsonMemberOfBinaryLegacy, [Some(first), Some(second)]) => {
                json_member_binary_legacy_args_valid(first, second)
            }
            _ => false,
        }
    }

    pub(crate) fn grouping_mode(self) -> Option<crate::GroupingMode> {
        match self {
            Self::GroupingBitAndNative => Some(crate::GroupingMode::BitAnd),
            Self::GroupingNumericCmpNative => Some(crate::GroupingMode::NumericCmp),
            Self::GroupingNumericSetNative => Some(crate::GroupingMode::NumericSet),
            _ => None,
        }
    }

    pub(crate) fn comparison_op(self) -> Option<crate::ComparisonOp> {
        match self {
            Self::CompareIntSsNative(op)
            | Self::CompareIntSuNative(op)
            | Self::CompareIntUsNative(op)
            | Self::CompareIntUuNative(op)
            | Self::CompareInt128Legacy(op)
            | Self::CompareRealNative(op)
            | Self::CompareRealLegacy(op)
            | Self::CompareDecimalNative(op)
            | Self::CompareBytesNative(op)
            | Self::CompareVectorNative(op)
            | Self::CompareTimeCoreNative(op)
            | Self::CompareDurationNative(op)
            | Self::CompareJsonNative(op) => Some(op),
            _ => None,
        }
    }

    pub(crate) fn aes_profile(self) -> Option<(NativeAesOperation, NativeAesProfile)> {
        use NativeAesOperation::{Decrypt, Encrypt};
        use NativeAesProfile::*;
        Some(match self {
            Self::AesEncrypt128EcbNative => (Encrypt, Aes128Ecb),
            Self::AesEncrypt192EcbNative => (Encrypt, Aes192Ecb),
            Self::AesEncrypt256EcbNative => (Encrypt, Aes256Ecb),
            Self::AesDecrypt128EcbNative => (Decrypt, Aes128Ecb),
            Self::AesDecrypt192EcbNative => (Decrypt, Aes192Ecb),
            Self::AesDecrypt256EcbNative => (Decrypt, Aes256Ecb),
            Self::AesEncrypt128CbcNative => (Encrypt, Aes128Cbc),
            Self::AesEncrypt192CbcNative => (Encrypt, Aes192Cbc),
            Self::AesEncrypt256CbcNative => (Encrypt, Aes256Cbc),
            Self::AesDecrypt128CbcNative => (Decrypt, Aes128Cbc),
            Self::AesDecrypt192CbcNative => (Decrypt, Aes192Cbc),
            Self::AesDecrypt256CbcNative => (Decrypt, Aes256Cbc),
            Self::AesEncrypt128OfbNative => (Encrypt, Aes128Ofb),
            Self::AesEncrypt192OfbNative => (Encrypt, Aes192Ofb),
            Self::AesEncrypt256OfbNative => (Encrypt, Aes256Ofb),
            Self::AesDecrypt128OfbNative => (Decrypt, Aes128Ofb),
            Self::AesDecrypt192OfbNative => (Decrypt, Aes192Ofb),
            Self::AesDecrypt256OfbNative => (Decrypt, Aes256Ofb),
            Self::AesEncrypt128CfbNative => (Encrypt, Aes128Cfb),
            Self::AesEncrypt192CfbNative => (Encrypt, Aes192Cfb),
            Self::AesEncrypt256CfbNative => (Encrypt, Aes256Cfb),
            Self::AesDecrypt128CfbNative => (Decrypt, Aes128Cfb),
            Self::AesDecrypt192CfbNative => (Decrypt, Aes192Cfb),
            Self::AesDecrypt256CfbNative => (Decrypt, Aes256Cfb),
            _ => return None,
        })
    }

    fn aes_error_profile(self) -> Option<(NativeAesOperation, NativeAesProfile)> {
        self.aes_profile().filter(|(_, profile)| {
            !matches!(
                profile,
                NativeAesProfile::Aes128Ecb
                    | NativeAesProfile::Aes192Ecb
                    | NativeAesProfile::Aes256Ecb
            )
        })
    }

    pub(crate) fn decimal_division_kind(self) -> Option<NativeDecimalDivisionKind> {
        match self {
            Self::DivDecimalNative => Some(NativeDecimalDivisionKind::Native),
            Self::DivDecimalLegacy => Some(NativeDecimalDivisionKind::Legacy),
            _ => None,
        }
    }

    pub(crate) fn is_division_value(self) -> bool {
        matches!(
            self,
            Self::DivRealNative
                | Self::DivRealLegacy
                | Self::DivDecimalNative
                | Self::DivDecimalLegacy
        )
    }

    pub(crate) fn is_temporal_literal(self) -> bool {
        matches!(self, Self::DateLiteralNative | Self::TimestampLiteralNative)
    }

    pub(crate) fn metadata_matches(self, metadata: &(dyn std::any::Any + Send)) -> bool {
        if self.is_temporal_literal() {
            return metadata.is::<NativeTemporalCallMetadata>();
        }
        if let Some(kind) = self.decimal_division_kind() {
            return metadata
                .downcast_ref::<NativeDecimalDivisionCallMetadata>()
                .is_some_and(|payload| payload.kind == kind);
        }
        if let Some(kind) = self.like_kind() {
            return metadata
                .downcast_ref::<NativeLikeCallMetadata>()
                .is_some_and(|payload| payload.kind == kind);
        }
        match self.regexp_kind() {
            Some(kind) => metadata
                .downcast_ref::<NativeRegexpCallMetadata>()
                .is_some_and(|payload| payload.kind == kind),
            None => metadata.is::<()>(),
        }
    }

    pub(crate) fn input_role(self) -> EvaluatedArgsRole {
        if self.is_temporal_literal() {
            return EvaluatedArgsRole::TemporalText;
        }
        // A private identity does not determine its carrier or packet policy.
        // In particular, value-only FROM_BASE64 keeps the ordinary Bytes role.
        match self {
            Self::CompareInt128Legacy(_) => EvaluatedArgsRole::Int1282,
            Self::CompareRealNative(_) | Self::CompareRealLegacy(_) => {
                EvaluatedArgsRole::Ieee754Bits2
            }
            Self::CompareDecimalNative(_) => EvaluatedArgsRole::DecimalBinary,
            Self::CompareBytesNative(_) => EvaluatedArgsRole::CollatedBytes2,
            Self::CompareVectorNative(_) => EvaluatedArgsRole::NativeVector2,
            Self::CompareTimeCoreNative(_) => EvaluatedArgsRole::TimeCoreBits2,
            Self::CompareNullNative
            | Self::GroupingNullNative
            | Self::JsonPredicateNullNative
            | Self::JsonOutputNullNative
            | Self::UtcTimeNullNative => EvaluatedArgsRole::NullWitness,
            Self::CompareMissingLegacy
            | Self::JsonPredicateMissingLegacy
            | Self::JsonValueAbsentLegacy => EvaluatedArgsRole::NoArgs,
            Self::AesNullNative => EvaluatedArgsRole::NullWitness,
            Self::DivDecimalNative | Self::DivDecimalLegacy => EvaluatedArgsRole::DecimalDivision,
            Self::AddDecimalNative
            | Self::SubDecimalNative
            | Self::MulDecimalNative
            | Self::ModDecimalNative
            | Self::AddDecimalLegacy
            | Self::SubDecimalLegacy
            | Self::MulDecimalLegacy
            | Self::AddDecimalFastNative
            | Self::SubDecimalFastNative
            | Self::MulDecimalFastNative => EvaluatedArgsRole::DecimalBinary,
            Self::AddInt128SignedLegacy
            | Self::AddInt128UnsignedLegacy
            | Self::AddInt128RejectLeftLegacy
            | Self::AddInt128RejectRightLegacy
            | Self::SubInt128SignedLegacy
            | Self::SubInt128UnsignedLegacy
            | Self::SubInt128RejectLeftLegacy
            | Self::SubInt128RejectRightLegacy
            | Self::MulInt128SignedLegacy
            | Self::ModInt128Legacy
            | Self::MulInt128UnsignedLegacy
            | Self::IntDivInt128Legacy => EvaluatedArgsRole::Int1282,
            Self::DivRealNative
            | Self::DivRealLegacy
            | Self::AddRealNative
            | Self::SubRealNative
            | Self::MulRealNative
            | Self::ModRealNative
            | Self::ModRealLegacy
            | Self::AddRealLegacy
            | Self::SubRealLegacy
            | Self::MulRealLegacy => EvaluatedArgsRole::Ieee754Bits2,
            Self::AddVectorNative | Self::SubVectorNative | Self::MulVectorNative => {
                EvaluatedArgsRole::NativeVector2
            }
            Self::BinaryArithmeticNullNative => EvaluatedArgsRole::NullWitness,
            Self::BinaryArithmeticMissingLegacy => EvaluatedArgsRole::NoArgs,
            Self::VecAsTextNative | Self::VecDimsNative | Self::VecL2NormNative => {
                EvaluatedArgsRole::NativeVector
            }
            Self::VecL1DistanceNative
            | Self::VecL2DistanceNative
            | Self::VecNegativeInnerProductNative
            | Self::VecCosineDistanceNative => EvaluatedArgsRole::NativeVector2,
            Self::LikeNative | Self::IlikeNative | Self::LikeLegacyNative => {
                EvaluatedArgsRole::Like
            }
            Self::LikeNullIntNative => EvaluatedArgsRole::NullWitness,
            Self::LikeMissingLegacyNative => EvaluatedArgsRole::NoArgs,
            Self::RegexpLikeNative => EvaluatedArgsRole::NativeRegexpLike,
            Self::RegexpSubstrNative => EvaluatedArgsRole::NativeRegexpSubstr,
            Self::RegexpInstrNative => EvaluatedArgsRole::NativeRegexpInstr,
            Self::RegexpReplaceNative => EvaluatedArgsRole::NativeRegexpReplace,
            Self::ConcatNative | Self::ConcatWsNative => EvaluatedArgsRole::ConcatPacked,
            Self::EltNative => EvaluatedArgsRole::EltReady,
            Self::FieldBytesNative | Self::FieldIntNative | Self::FieldRealNative => {
                EvaluatedArgsRole::FieldPacked
            }
            Self::MakeSetNative => EvaluatedArgsRole::MakeSetPacked,
            Self::ExportSetNative => EvaluatedArgsRole::ExportSetPacked,
            Self::AbsDecimalNative
            | Self::CeilDecimalNative
            | Self::FloorDecimalNative
            | Self::RoundDecimalLegacy
            | Self::UnaryPlusDecimalNative
            | Self::UnaryMinusDecimalNative => EvaluatedArgsRole::DecimalUnary,
            Self::RoundDecimalNative | Self::TruncateDecimalNative => EvaluatedArgsRole::DecimalInt,
            Self::RoundRealNative | Self::TruncateRealNative | Self::SecToTimeNative => {
                EvaluatedArgsRole::Ieee754Int
            }
            Self::MakeTimePartsNative => EvaluatedArgsRole::MakeTimeParts,
            Self::RoundInt128Legacy => EvaluatedArgsRole::Int128,
            Self::MathNullWitnessNative
            | Self::DateDiffNullNative
            | Self::WeekNullNative
            | Self::DateFormatNullNative
            | Self::TranslateNullNative
            | Self::SqlCryptNullNative
            | Self::VecRealNullNative
            | Self::RegexpNullIntNative
            | Self::RegexpNullBytesNative
            | Self::UnaryNullNative => EvaluatedArgsRole::NullWitness,
            Self::DateDiffCoreNative => EvaluatedArgsRole::TimeCoreBits2,
            Self::DateFormatCoreNative => EvaluatedArgsRole::TimeCoreBitsBytes,
            Self::CharNative => EvaluatedArgsRole::CharReady,
            Self::ConvNative | Self::ConvBinaryLiteralNative => EvaluatedArgsRole::ConvNative,
            Self::ConvLegacy => EvaluatedArgsRole::ConvLegacy,
            Self::YearCoreNative
            | Self::MonthCoreNative
            | Self::DayOfMonthCoreNative
            | Self::QuarterCoreNative
            | Self::WeekCoreNative
            | Self::DateCorePredicateLegacy => EvaluatedArgsRole::TimeCoreBits,
            Self::AbsRealNative
            | Self::CeilRealNative
            | Self::FloorRealNative
            | Self::RoundRealLegacy
            | Self::FormatBytesNative
            | Self::FormatNanoTimeNative => EvaluatedArgsRole::Ieee754Bits,
            Self::StrcmpNative | Self::FindInSetNative => EvaluatedArgsRole::CollatedBytes2,
            Self::Locate2Native | Self::Locate3Native => EvaluatedArgsRole::NativeSearch,
            Self::FindInSetPreparedNative => EvaluatedArgsRole::FindInSetPrepared,
            Self::PiRaw
            | Self::JsonValidOtherNative
            | Self::DateFormatMissingNative
            | Self::RegexpMissingLegacyNative => EvaluatedArgsRole::NoArgs,
            Self::Sha2Native => EvaluatedArgsRole::ReadyBytesInt,
            Self::LogNative | Self::PowNative | Self::Atan2GoNative | Self::Atan2LibmLegacy => {
                EvaluatedArgsRole::Ieee754Bits2
            }
            Self::Substring2BytesNative
            | Self::Substring3BytesNative
            | Self::Substring2Utf8Native
            | Self::Substring3Utf8Native => EvaluatedArgsRole::SubstringNative,
            Self::Substring2BytesLegacy
            | Self::Substring3BytesLegacy
            | Self::Substring2Utf8Legacy
            | Self::Substring3Utf8Legacy => EvaluatedArgsRole::SubstringLegacy,
            Self::SubstringIndexSignedNative | Self::SubstringIndexUnsignedNative => {
                EvaluatedArgsRole::ReadyBytesBytesInt
            }
            Self::LpadBytesNative
            | Self::RpadBytesNative
            | Self::LpadUtf8Native
            | Self::RpadUtf8Native => EvaluatedArgsRole::PadPacket,
            Self::SpaceNative
            | Self::RepeatNative
            | Self::ToBase64Native
            | Self::FromBase64Native => EvaluatedArgsRole::Packet,
            Self::AsinRaw
            | Self::AcosRaw
            | Self::SqrtRaw
            | Self::SignRaw
            | Self::RadiansRaw
            | Self::DegreesRaw
            | Self::LnNative
            | Self::Log2Native
            | Self::SinGoNative
            | Self::CosGoNative
            | Self::TanGoNative
            | Self::CotGoNative
            | Self::AtanGoNative
            | Self::SinLibmLegacy
            | Self::CosLibmLegacy
            | Self::CotLibmLegacy
            | Self::AtanLibmLegacy
            | Self::ExpGoNative
            | Self::Log10GoNative
            | Self::UnaryPlusBitsNative
            | Self::UnaryMinusBitsNative => EvaluatedArgsRole::Ieee754Bits,
            _ => EvaluatedArgsRole::Values,
        }
    }

    pub(crate) fn is_pad_native(self) -> bool {
        matches!(
            self,
            Self::LpadBytesNative
                | Self::RpadBytesNative
                | Self::LpadUtf8Native
                | Self::RpadUtf8Native
        )
    }

    pub(crate) fn is_insert(self) -> bool {
        matches!(self, Self::Insert | Self::InsertUtf8Native)
    }

    pub(crate) fn field_kind(self) -> Option<FieldKind> {
        match self {
            Self::FieldBytesNative => Some(FieldKind::Bytes),
            Self::FieldIntNative => Some(FieldKind::Int),
            Self::FieldRealNative => Some(FieldKind::Real),
            _ => None,
        }
    }

    pub(crate) fn concat_kind(self) -> Option<ConcatKind> {
        match self {
            Self::ConcatNative => Some(ConcatKind::Concat),
            Self::ConcatWsNative => Some(ConcatKind::ConcatWs),
            _ => None,
        }
    }

    pub(crate) fn is_locate3_native(self) -> bool {
        self == Self::Locate3Native
    }

    pub(crate) fn is_substring_native(self) -> bool {
        matches!(
            self,
            Self::Substring2BytesNative
                | Self::Substring3BytesNative
                | Self::Substring2Utf8Native
                | Self::Substring3Utf8Native
        )
    }

    pub(crate) fn is_substring_legacy(self) -> bool {
        matches!(
            self,
            Self::Substring2BytesLegacy
                | Self::Substring3BytesLegacy
                | Self::Substring2Utf8Legacy
                | Self::Substring3Utf8Legacy
        )
    }

    pub(crate) fn substring_is_utf8(self) -> bool {
        matches!(
            self,
            Self::Substring2Utf8Native
                | Self::Substring3Utf8Native
                | Self::Substring2Utf8Legacy
                | Self::Substring3Utf8Legacy
        )
    }

    /// ORD receives one already-encoded native character, never an arbitrary
    /// string. This is a prepared-value domain check, not a memory budget.
    pub(crate) fn ready_bytes_match(self, value: Option<&[u8]>) -> bool {
        self != Self::OrdNative || value.is_none_or(|bytes| bytes.len() <= 4)
    }

    fn native_binary_error_profile(
        self,
    ) -> Option<(BinaryArithmeticOperation, BinaryArithmeticErrorKind)> {
        use BinaryArithmeticErrorKind::{DecimalOverflow, FloatOverflow, IntOverflow};
        use BinaryArithmeticOperation::{Add, Multiply, Subtract};
        Some(match self {
            Self::AddIntSsNative
            | Self::AddIntSuNative
            | Self::AddIntUsNative
            | Self::AddIntUuNative => (Add, IntOverflow),
            Self::SubIntSsNative
            | Self::SubIntSuNative
            | Self::SubIntUsNative
            | Self::SubIntUuNative
            | Self::SubIntSuForcedNative
            | Self::SubIntUsForcedNative
            | Self::SubIntUuForcedNative => (Subtract, IntOverflow),
            Self::MulIntSignedNative | Self::MulIntUnsignedNative => (Multiply, IntOverflow),
            Self::IntDivIntSsNative
            | Self::IntDivIntUsNative
            | Self::IntDivIntSuNative
            | Self::IntDivIntUuNative => (BinaryArithmeticOperation::IntDivide, IntOverflow),
            Self::AddRealNative => (Add, FloatOverflow),
            Self::SubRealNative => (Subtract, FloatOverflow),
            Self::MulRealNative => (Multiply, FloatOverflow),
            Self::ModRealNative => (BinaryArithmeticOperation::Modulo, FloatOverflow),
            Self::DivRealNative => (BinaryArithmeticOperation::Divide, FloatOverflow),
            Self::AddDecimalNative => (Add, DecimalOverflow),
            Self::SubDecimalNative => (Subtract, DecimalOverflow),
            Self::MulDecimalNative => (Multiply, DecimalOverflow),
            _ => return None,
        })
    }

    fn legacy_binary_error_profile(self) -> Option<(BinaryArithmeticOperation, bool)> {
        use BinaryArithmeticOperation::{Add, Multiply, Subtract};
        Some(match self {
            Self::AddInt128SignedLegacy => (Add, false),
            Self::AddInt128UnsignedLegacy
            | Self::AddInt128RejectLeftLegacy
            | Self::AddInt128RejectRightLegacy => (Add, true),
            Self::SubInt128SignedLegacy => (Subtract, false),
            Self::SubInt128UnsignedLegacy
            | Self::SubInt128RejectLeftLegacy
            | Self::SubInt128RejectRightLegacy => (Subtract, true),
            Self::MulInt128SignedLegacy => (Multiply, false),
            Self::MulInt128UnsignedLegacy => (Multiply, true),
            _ => return None,
        })
    }

    fn is_decimal_int_div_budgeted(self) -> bool {
        self.is_native_decimal_int_div() || self == Self::IntDivDecimalLegacy
    }

    pub(crate) fn is_native_decimal_int_div(self) -> bool {
        matches!(
            self,
            Self::IntDivDecimalSignedNative | Self::IntDivDecimalUnsignedNative
        )
    }

    pub(crate) fn is_integer_division_value(self) -> bool {
        matches!(
            self,
            Self::IntDivIntSsNative
                | Self::IntDivIntUsNative
                | Self::IntDivIntSuNative
                | Self::IntDivIntUuNative
                | Self::IntDivInt128Legacy
        )
    }

    /// Value-only MOD recipes reserve a successful NULL result for zero
    /// divisors. Other arithmetic retains its existing nullable operand
    /// domain.
    pub(crate) fn is_modulo_value(self) -> bool {
        matches!(
            self,
            Self::ModIntSsNative
                | Self::ModIntSuNative
                | Self::ModIntUsNative
                | Self::ModIntUuNative
                | Self::ModInt128Legacy
                | Self::ModRealNative
                | Self::ModRealLegacy
                | Self::ModDecimalNative
        )
    }

    pub(crate) fn is_binary_decimal(self) -> bool {
        matches!(
            self,
            Self::AddDecimalNative
                | Self::SubDecimalNative
                | Self::MulDecimalNative
                | Self::ModDecimalNative
                | Self::AddDecimalLegacy
                | Self::SubDecimalLegacy
                | Self::MulDecimalLegacy
                | Self::AddDecimalFastNative
                | Self::SubDecimalFastNative
                | Self::MulDecimalFastNative
        )
    }

    pub(crate) fn is_binary_int128(self) -> bool {
        matches!(
            self,
            Self::AddInt128SignedLegacy
                | Self::AddInt128UnsignedLegacy
                | Self::AddInt128RejectLeftLegacy
                | Self::AddInt128RejectRightLegacy
                | Self::SubInt128SignedLegacy
                | Self::SubInt128UnsignedLegacy
                | Self::SubInt128RejectLeftLegacy
                | Self::SubInt128RejectRightLegacy
                | Self::MulInt128SignedLegacy
                | Self::ModInt128Legacy
                | Self::MulInt128UnsignedLegacy
                | Self::IntDivInt128Legacy
        )
    }

    fn returns_decimal_fast(self) -> bool {
        matches!(
            self,
            Self::AddDecimalFastNative | Self::SubDecimalFastNative | Self::MulDecimalFastNative
        )
    }

    fn returns_json_report(self) -> bool {
        matches!(
            self,
            Self::JsonTypeTextNative
                | Self::JsonTypeBinaryNative
                | Self::JsonDepthNative
                | Self::JsonStorageFreeNative
                | Self::JsonStorageSizeNative
        )
    }

    fn returns_ieee754_bits(self) -> bool {
        matches!(
            self,
            Self::DivRealNative
                | Self::DivRealLegacy
                | Self::AddRealNative
                | Self::SubRealNative
                | Self::MulRealNative
                | Self::ModRealNative
                | Self::ModRealLegacy
                | Self::AddRealLegacy
                | Self::SubRealLegacy
                | Self::MulRealLegacy
                | Self::UnaryPlusBitsNative
                | Self::UnaryMinusBitsNative
                | Self::AsinRaw
                | Self::AcosRaw
                | Self::SqrtRaw
                | Self::RadiansRaw
                | Self::DegreesRaw
                | Self::PiRaw
                | Self::LnNative
                | Self::LogNative
                | Self::Log2Native
                | Self::PowNative
                | Self::AbsRealNative
                | Self::CeilRealNative
                | Self::FloorRealNative
                | Self::RoundRealNative
                | Self::TruncateRealNative
                | Self::RoundRealLegacy
                | Self::RoundDecimalLegacy
                | Self::SinGoNative
                | Self::CosGoNative
                | Self::TanGoNative
                | Self::CotGoNative
                | Self::AtanGoNative
                | Self::Atan2GoNative
                | Self::SinLibmLegacy
                | Self::CosLibmLegacy
                | Self::CotLibmLegacy
                | Self::AtanLibmLegacy
                | Self::Atan2LibmLegacy
                | Self::ExpGoNative
                | Self::Log10GoNative
                | Self::MakeTimePartsNative
                | Self::VecL1DistanceNative
                | Self::VecL2DistanceNative
                | Self::VecNegativeInnerProductNative
                | Self::VecCosineDistanceNative
                | Self::VecL2NormNative
                | Self::VecRealNullNative
        )
    }

    pub(crate) fn fn_meta(self) -> crate::RpnFnMeta {
        // Fixed identity witnesses for common preparation. Only the closed
        // factory also uses the private getters to select a non-wire call;
        // no caller-supplied metadata or alternative algorithm is accepted.
        match self {
            Self::ConvertTzNative => crate::impl_time::convert_tz_native_fn_meta(),
            Self::DateLiteralNative => crate::impl_time::date_literal_native_fn_meta(),
            Self::TimestampLiteralNative => crate::impl_time::timestamp_literal_native_fn_meta(),
            Self::AddIntSsNative => crate::impl_arithmetic::add_int_ss_native_fn_meta(),
            Self::AddIntSuNative => crate::impl_arithmetic::add_int_su_native_fn_meta(),
            Self::AddIntUsNative => crate::impl_arithmetic::add_int_us_native_fn_meta(),
            Self::AddIntUuNative => crate::impl_arithmetic::add_int_uu_native_fn_meta(),
            Self::SubIntSsNative => crate::impl_arithmetic::sub_int_ss_native_fn_meta(),
            Self::SubIntSuNative => crate::impl_arithmetic::sub_int_su_native_fn_meta(),
            Self::SubIntUsNative => crate::impl_arithmetic::sub_int_us_native_fn_meta(),
            Self::SubIntUuNative => crate::impl_arithmetic::sub_int_uu_native_fn_meta(),
            Self::SubIntSuForcedNative => {
                crate::impl_arithmetic::sub_int_su_forced_native_fn_meta()
            }
            Self::SubIntUsForcedNative => {
                crate::impl_arithmetic::sub_int_us_forced_native_fn_meta()
            }
            Self::SubIntUuForcedNative => {
                crate::impl_arithmetic::sub_int_uu_forced_native_fn_meta()
            }
            Self::MulIntSignedNative => crate::impl_arithmetic::mul_int_signed_native_fn_meta(),
            Self::MulIntUnsignedNative => crate::impl_arithmetic::mul_int_unsigned_native_fn_meta(),
            Self::AddRealNative => crate::impl_arithmetic::add_real_native_fn_meta(),
            Self::SubRealNative => crate::impl_arithmetic::sub_real_native_fn_meta(),
            Self::MulRealNative => crate::impl_arithmetic::mul_real_native_fn_meta(),
            Self::AddDecimalNative => crate::impl_arithmetic::add_decimal_native_fn_meta(),
            Self::SubDecimalNative => crate::impl_arithmetic::sub_decimal_native_fn_meta(),
            Self::MulDecimalNative => crate::impl_arithmetic::mul_decimal_native_fn_meta(),
            Self::AddVectorNative => crate::impl_vec::add_vector_native_fn_meta(),
            Self::SubVectorNative => crate::impl_vec::sub_vector_native_fn_meta(),
            Self::MulVectorNative => crate::impl_vec::mul_vector_native_fn_meta(),
            Self::BinaryArithmeticNullNative => {
                crate::impl_arithmetic::binary_arithmetic_null_native_fn_meta()
            }
            Self::AddInt128SignedLegacy => {
                crate::impl_arithmetic::add_int128_signed_legacy_fn_meta()
            }
            Self::AddInt128UnsignedLegacy => {
                crate::impl_arithmetic::add_int128_unsigned_legacy_fn_meta()
            }
            Self::AddInt128RejectLeftLegacy => {
                crate::impl_arithmetic::add_int128_reject_left_legacy_fn_meta()
            }
            Self::AddInt128RejectRightLegacy => {
                crate::impl_arithmetic::add_int128_reject_right_legacy_fn_meta()
            }
            Self::SubInt128SignedLegacy => {
                crate::impl_arithmetic::sub_int128_signed_legacy_fn_meta()
            }
            Self::SubInt128UnsignedLegacy => {
                crate::impl_arithmetic::sub_int128_unsigned_legacy_fn_meta()
            }
            Self::SubInt128RejectLeftLegacy => {
                crate::impl_arithmetic::sub_int128_reject_left_legacy_fn_meta()
            }
            Self::SubInt128RejectRightLegacy => {
                crate::impl_arithmetic::sub_int128_reject_right_legacy_fn_meta()
            }
            Self::MulInt128SignedLegacy => {
                crate::impl_arithmetic::mul_int128_signed_legacy_fn_meta()
            }
            Self::MulInt128UnsignedLegacy => {
                crate::impl_arithmetic::mul_int128_unsigned_legacy_fn_meta()
            }
            Self::AddRealLegacy => crate::impl_arithmetic::add_real_legacy_fn_meta(),
            Self::SubRealLegacy => crate::impl_arithmetic::sub_real_legacy_fn_meta(),
            Self::MulRealLegacy => crate::impl_arithmetic::mul_real_legacy_fn_meta(),
            Self::AddDecimalLegacy => crate::impl_arithmetic::add_decimal_legacy_fn_meta(),
            Self::SubDecimalLegacy => crate::impl_arithmetic::sub_decimal_legacy_fn_meta(),
            Self::MulDecimalLegacy => crate::impl_arithmetic::mul_decimal_legacy_fn_meta(),
            Self::BinaryArithmeticMissingLegacy => {
                crate::impl_arithmetic::binary_arithmetic_missing_legacy_fn_meta()
            }
            Self::AddDecimalFastNative => crate::impl_arithmetic::add_decimal_fast_native_fn_meta(),
            Self::SubDecimalFastNative => crate::impl_arithmetic::sub_decimal_fast_native_fn_meta(),
            Self::MulDecimalFastNative => crate::impl_arithmetic::mul_decimal_fast_native_fn_meta(),
            Self::IntDivDecimalSignedNative => {
                crate::impl_arithmetic::int_div_decimal_signed_native_fn_meta()
            }
            Self::IntDivDecimalUnsignedNative => {
                crate::impl_arithmetic::int_div_decimal_unsigned_native_fn_meta()
            }
            Self::IntDivDecimalLegacy => crate::impl_arithmetic::int_div_decimal_legacy_fn_meta(),
            Self::IntDivIntSsNative => crate::impl_arithmetic::int_div_int_ss_native_fn_meta(),
            Self::IntDivIntUsNative => crate::impl_arithmetic::int_div_int_us_native_fn_meta(),
            Self::IntDivIntSuNative => crate::impl_arithmetic::int_div_int_su_native_fn_meta(),
            Self::IntDivIntUuNative => crate::impl_arithmetic::int_div_int_uu_native_fn_meta(),
            Self::IntDivInt128Legacy => crate::impl_arithmetic::int_div_int128_legacy_fn_meta(),
            Self::ModIntSsNative => crate::impl_arithmetic::mod_int_ss_native_fn_meta(),
            Self::ModIntSuNative => crate::impl_arithmetic::mod_int_su_native_fn_meta(),
            Self::ModIntUsNative => crate::impl_arithmetic::mod_int_us_native_fn_meta(),
            Self::ModIntUuNative => crate::impl_arithmetic::mod_int_uu_native_fn_meta(),
            Self::ModInt128Legacy => crate::impl_arithmetic::mod_int128_legacy_fn_meta(),
            Self::ModRealNative => crate::impl_arithmetic::mod_real_native_fn_meta(),
            Self::ModRealLegacy => crate::impl_arithmetic::mod_real_legacy_fn_meta(),
            Self::ModDecimalNative => crate::impl_arithmetic::mod_decimal_native_fn_meta(),
            Self::DivRealNative => crate::impl_arithmetic::div_real_native_fn_meta(),
            Self::DivRealLegacy => crate::impl_arithmetic::div_real_legacy_fn_meta(),
            Self::DivDecimalNative => crate::impl_arithmetic::div_decimal_native_fn_meta(),
            Self::DivDecimalLegacy => crate::impl_arithmetic::div_decimal_legacy_fn_meta(),
            Self::AesEncrypt128EcbNative => {
                crate::impl_encryption::aes_encrypt_128_ecb_native_fn_meta()
            }
            Self::AesEncrypt192EcbNative => {
                crate::impl_encryption::aes_encrypt_192_ecb_native_fn_meta()
            }
            Self::AesEncrypt256EcbNative => {
                crate::impl_encryption::aes_encrypt_256_ecb_native_fn_meta()
            }
            Self::AesDecrypt128EcbNative => {
                crate::impl_encryption::aes_decrypt_128_ecb_native_fn_meta()
            }
            Self::AesDecrypt192EcbNative => {
                crate::impl_encryption::aes_decrypt_192_ecb_native_fn_meta()
            }
            Self::AesDecrypt256EcbNative => {
                crate::impl_encryption::aes_decrypt_256_ecb_native_fn_meta()
            }
            Self::AesEncrypt128CbcNative => {
                crate::impl_encryption::aes_encrypt_128_cbc_native_fn_meta()
            }
            Self::AesEncrypt192CbcNative => {
                crate::impl_encryption::aes_encrypt_192_cbc_native_fn_meta()
            }
            Self::AesEncrypt256CbcNative => {
                crate::impl_encryption::aes_encrypt_256_cbc_native_fn_meta()
            }
            Self::AesDecrypt128CbcNative => {
                crate::impl_encryption::aes_decrypt_128_cbc_native_fn_meta()
            }
            Self::AesDecrypt192CbcNative => {
                crate::impl_encryption::aes_decrypt_192_cbc_native_fn_meta()
            }
            Self::AesDecrypt256CbcNative => {
                crate::impl_encryption::aes_decrypt_256_cbc_native_fn_meta()
            }
            Self::AesEncrypt128OfbNative => {
                crate::impl_encryption::aes_encrypt_128_ofb_native_fn_meta()
            }
            Self::AesEncrypt192OfbNative => {
                crate::impl_encryption::aes_encrypt_192_ofb_native_fn_meta()
            }
            Self::AesEncrypt256OfbNative => {
                crate::impl_encryption::aes_encrypt_256_ofb_native_fn_meta()
            }
            Self::AesDecrypt128OfbNative => {
                crate::impl_encryption::aes_decrypt_128_ofb_native_fn_meta()
            }
            Self::AesDecrypt192OfbNative => {
                crate::impl_encryption::aes_decrypt_192_ofb_native_fn_meta()
            }
            Self::AesDecrypt256OfbNative => {
                crate::impl_encryption::aes_decrypt_256_ofb_native_fn_meta()
            }
            Self::AesEncrypt128CfbNative => {
                crate::impl_encryption::aes_encrypt_128_cfb_native_fn_meta()
            }
            Self::AesEncrypt192CfbNative => {
                crate::impl_encryption::aes_encrypt_192_cfb_native_fn_meta()
            }
            Self::AesEncrypt256CfbNative => {
                crate::impl_encryption::aes_encrypt_256_cfb_native_fn_meta()
            }
            Self::AesDecrypt128CfbNative => {
                crate::impl_encryption::aes_decrypt_128_cfb_native_fn_meta()
            }
            Self::AesDecrypt192CfbNative => {
                crate::impl_encryption::aes_decrypt_192_cfb_native_fn_meta()
            }
            Self::AesDecrypt256CfbNative => {
                crate::impl_encryption::aes_decrypt_256_cfb_native_fn_meta()
            }
            Self::AesNullNative => crate::impl_encryption::aes_null_native_fn_meta(),
            Self::CompareIntSsNative(op) => crate::impl_compare::compare_int_ss_native_fn_meta(op),
            Self::CompareIntSuNative(op) => crate::impl_compare::compare_int_su_native_fn_meta(op),
            Self::CompareIntUsNative(op) => crate::impl_compare::compare_int_us_native_fn_meta(op),
            Self::CompareIntUuNative(op) => crate::impl_compare::compare_int_uu_native_fn_meta(op),
            Self::CompareInt128Legacy(op) => crate::impl_compare::compare_int128_legacy_fn_meta(op),
            Self::CompareRealNative(op) => crate::impl_compare::compare_real_native_fn_meta(op),
            Self::CompareRealLegacy(op) => crate::impl_compare::compare_real_legacy_fn_meta(op),
            Self::CompareDecimalNative(op) => {
                crate::impl_compare::compare_decimal_native_fn_meta(op)
            }
            Self::CompareBytesNative(op) => crate::impl_compare::compare_bytes_native_fn_meta(op),
            Self::CompareVectorNative(op) => crate::impl_compare::compare_vector_native_fn_meta(op),
            Self::CompareTimeCoreNative(op) => {
                crate::impl_compare::compare_time_core_native_fn_meta(op)
            }
            Self::CompareDurationNative(op) => {
                crate::impl_compare::compare_duration_native_fn_meta(op)
            }
            Self::CompareJsonNative(op) => crate::impl_compare::compare_json_native_fn_meta(op),
            Self::CompareNullNative => crate::impl_compare::compare_null_native_fn_meta(),
            Self::CompareMissingLegacy => crate::impl_compare::compare_missing_legacy_fn_meta(),
            Self::GroupingBitAndNative => {
                crate::impl_miscellaneous::grouping_bit_and_native_fn_meta()
            }
            Self::GroupingNumericCmpNative => {
                crate::impl_miscellaneous::grouping_numeric_cmp_native_fn_meta()
            }
            Self::GroupingNumericSetNative => {
                crate::impl_miscellaneous::grouping_numeric_set_native_fn_meta()
            }
            Self::GroupingNullNative => crate::impl_miscellaneous::grouping_null_native_fn_meta(),
            Self::JsonContainsSerdeNative => crate::impl_json::json_contains_serde_native_fn_meta(),
            Self::JsonContainsPathSerdeNative => {
                crate::impl_json::json_contains_path_serde_native_fn_meta()
            }
            Self::JsonOverlapsSerdeNative => crate::impl_json::json_overlaps_serde_native_fn_meta(),
            Self::JsonMemberOfSerdeNative => {
                crate::impl_json::json_member_of_serde_native_fn_meta()
            }
            Self::JsonLengthSerdeNative => crate::impl_json::json_length_serde_native_fn_meta(),
            Self::JsonLengthPathSerdeNative => {
                crate::impl_json::json_length_path_serde_native_fn_meta()
            }
            Self::JsonPathExistsSerdeNative => {
                crate::impl_json::json_path_exists_serde_native_fn_meta()
            }
            Self::JsonMemberOfBinaryLegacy => {
                crate::impl_json::json_member_of_binary_legacy_fn_meta()
            }
            Self::JsonPredicateNullNative => crate::impl_json::json_predicate_null_native_fn_meta(),
            Self::JsonPredicateMissingLegacy => {
                crate::impl_json::json_predicate_missing_legacy_fn_meta()
            }
            Self::JsonArraySerdeNative => crate::impl_json::json_array_serde_native_fn_meta(),
            Self::JsonObjectSerdeNative => crate::impl_json::json_object_serde_native_fn_meta(),
            Self::JsonKeysSerdeNative => crate::impl_json::json_keys_serde_native_fn_meta(),
            Self::JsonKeysPathSerdeNative => {
                crate::impl_json::json_keys_path_serde_native_fn_meta()
            }
            Self::JsonPrettySerdeNative => crate::impl_json::json_pretty_serde_native_fn_meta(),
            Self::JsonOutputNullNative => crate::impl_json::json_output_null_native_fn_meta(),
            Self::JsonSearchSerdeNative => crate::impl_json::json_search_native_fn_meta(),
            Self::JsonExtractSerdeNative => crate::impl_json::json_extract_serde_native_fn_meta(),
            Self::JsonInsertSerdeNative => crate::impl_json::json_insert_serde_native_fn_meta(),
            Self::JsonSetSerdeNative => crate::impl_json::json_set_serde_native_fn_meta(),
            Self::JsonReplaceSerdeNative => crate::impl_json::json_replace_serde_native_fn_meta(),
            Self::JsonRemoveSerdeNative => crate::impl_json::json_remove_serde_native_fn_meta(),
            Self::JsonArrayAppendSerdeNative => {
                crate::impl_json::json_array_append_serde_native_fn_meta()
            }
            Self::JsonArrayInsertSerdeNative => {
                crate::impl_json::json_array_insert_serde_native_fn_meta()
            }
            Self::JsonReplaceRawLegacy => crate::impl_json::json_replace_raw_legacy_fn_meta(),
            Self::JsonArrayAppendRawLegacy => {
                crate::impl_json::json_array_append_raw_legacy_fn_meta()
            }
            Self::JsonArrayAppendEmptyLegacy => {
                crate::impl_json::json_array_append_empty_legacy_fn_meta()
            }
            Self::JsonValueAbsentLegacy => crate::impl_json::json_value_absent_legacy_fn_meta(),
            Self::UtcDateNative => crate::impl_time::utc_date_native_fn_meta(),
            Self::UtcTimestampNative => crate::impl_time::utc_timestamp_native_fn_meta(),
            Self::CurrentTimeWithoutFspNative => {
                crate::impl_time::current_time_without_fsp_native_fn_meta()
            }
            Self::CurrentTimeWithFspNative => {
                crate::impl_time::current_time_with_fsp_native_fn_meta()
            }
            Self::UtcTimeWithoutFspNative => {
                crate::impl_time::utc_time_without_fsp_native_fn_meta()
            }
            Self::UtcTimeWithFspNative => crate::impl_time::utc_time_with_fsp_native_fn_meta(),
            Self::UtcTimeNullNative => crate::impl_time::utc_time_null_native_fn_meta(),
            Self::TimeNative => crate::impl_time::time_native_fn_meta(),
            Self::MicrosecondNative => crate::impl_time::microsecond_native_fn_meta(),
            Self::MicrosecondLegacy => crate::impl_time::microsecond_legacy_fn_meta(),
            Self::TimestampAddNative => crate::impl_time::timestamp_add_native_fn_meta(),
            Self::TimestampAddPrefixNullNative => {
                crate::impl_time::timestamp_add_prefix_null_native_fn_meta()
            }
            Self::AddTimeNative => crate::impl_time::add_time_native_fn_meta(),
            Self::SubTimeNative => crate::impl_time::sub_time_native_fn_meta(),
            Self::TimeAddRightDatetimeNative => {
                crate::impl_time::time_add_right_datetime_native_fn_meta()
            }
            Self::TidbParseTsoNative => crate::impl_time::tidb_parse_tso_native_fn_meta(),
            Self::TimeDiffTextNative => crate::impl_time::time_diff_text_native_fn_meta(),
            Self::AnyValueNative | Self::NameConstNative => {
                crate::impl_miscellaneous::any_value_bytes_fn_meta()
            }
            Self::WeightStringNative => crate::impl_string::weight_string_native_fn_meta(),
            Self::WeightStringCharNative => crate::impl_string::weight_string_char_native_fn_meta(),
            Self::WeightStringBinaryNative => {
                crate::impl_string::weight_string_binary_native_fn_meta()
            }
            Self::WeightStringNumericNative => {
                crate::impl_string::weight_string_numeric_native_fn_meta()
            }
            Self::FormatLocaleNative => crate::impl_string::format_locale_native_fn_meta(),
            Self::DateCoreNative => crate::impl_time::date_core_native_fn_meta(),
            Self::DateCorePredicateLegacy => crate::impl_time::date_core_predicate_legacy_fn_meta(),
            Self::NowNative => crate::impl_time::now_native_fn_meta(),
            Self::CurrentDateNative => crate::impl_time::current_date_native_fn_meta(),
            Self::SysdateNative => crate::impl_time::sysdate_native_fn_meta(),
            Self::JsonMergeSerdeNative => crate::impl_json::json_merge_serde_native_fn_meta(),
            Self::JsonMergePatchSerdeNative => {
                crate::impl_json::json_merge_patch_serde_native_fn_meta()
            }
            Self::JsonMergePatchRawLegacy => {
                crate::impl_json::json_merge_patch_raw_legacy_fn_meta()
            }
            Self::JsonUnquoteTextNative => crate::impl_json::json_unquote_text_native_fn_meta(),
            Self::JsonUnquoteBinaryNative => crate::impl_json::json_unquote_binary_native_fn_meta(),
            Self::UnaryPlusIntNative => crate::impl_op::unary_plus_int_native_fn_meta(),
            Self::UnaryPlusBitsNative => crate::impl_op::unary_plus_bits_native_fn_meta(),
            Self::UnaryPlusDecimalNative => crate::impl_op::unary_plus_decimal_native_fn_meta(),
            Self::UnaryPlusBytesNative => crate::impl_op::unary_plus_bytes_native_fn_meta(),
            Self::UnaryMinusIntNative => crate::impl_op::unary_minus_int_native_fn_meta(),
            Self::UnaryMinusUIntNative => crate::impl_op::unary_minus_uint_native_fn_meta(),
            Self::UnaryMinusIntConstantNative => {
                crate::impl_op::unary_minus_int_constant_native_fn_meta()
            }
            Self::UnaryMinusUIntConstantNative => {
                crate::impl_op::unary_minus_uint_constant_native_fn_meta()
            }
            Self::UnaryMinusBitsNative => crate::impl_op::unary_minus_bits_native_fn_meta(),
            Self::UnaryMinusDecimalNative => crate::impl_op::unary_minus_decimal_native_fn_meta(),
            Self::UnaryNullNative => crate::impl_op::unary_null_native_fn_meta(),
            Self::VecAsTextNative => crate::impl_vec::get_native_vec_as_text_fn_meta(),
            Self::VecDimsNative => crate::impl_vec::get_native_vec_dims_fn_meta(),
            Self::VecL1DistanceNative => crate::impl_vec::get_native_vec_l1_distance_fn_meta(),
            Self::VecL2DistanceNative => crate::impl_vec::get_native_vec_l2_distance_fn_meta(),
            Self::VecNegativeInnerProductNative => {
                crate::impl_vec::get_native_vec_negative_inner_product_fn_meta()
            }
            Self::VecCosineDistanceNative => {
                crate::impl_vec::get_native_vec_cosine_distance_fn_meta()
            }
            Self::VecL2NormNative => crate::impl_vec::get_native_vec_l2_norm_fn_meta(),
            Self::VecFromTextNative => crate::impl_vec::get_native_vec_from_text_fn_meta(),
            Self::VecRealNullNative => crate::impl_vec::get_native_vec_real_null_fn_meta(),
            Self::LikeNative => crate::impl_like::like_native_fn_meta(),
            Self::IlikeNative => crate::impl_like::ilike_native_fn_meta(),
            Self::LikeLegacyNative => crate::impl_like::like_legacy_native_fn_meta(),
            Self::LikeNullIntNative => crate::impl_like::like_null_int_native_fn_meta(),
            Self::LikeMissingLegacyNative => crate::impl_like::like_missing_legacy_native_fn_meta(),
            Self::RegexpLikeNative => crate::impl_regexp::get_regexp_like_native_fn_meta(),
            Self::RegexpSubstrNative => crate::impl_regexp::get_regexp_substr_native_fn_meta(),
            Self::RegexpInstrNative => crate::impl_regexp::get_regexp_instr_native_fn_meta(),
            Self::RegexpReplaceNative => crate::impl_regexp::get_regexp_replace_native_fn_meta(),
            Self::RegexpLikeLegacyCiNative => {
                crate::impl_regexp::get_regexp_like_legacy_ci_native_fn_meta()
            }
            Self::RegexpLikeLegacyBinNative => {
                crate::impl_regexp::get_regexp_like_legacy_bin_native_fn_meta()
            }
            Self::RegexpNullIntNative => crate::impl_regexp::get_regexp_null_int_native_fn_meta(),
            Self::RegexpNullBytesNative => {
                crate::impl_regexp::get_regexp_null_bytes_native_fn_meta()
            }
            Self::RegexpMissingLegacyNative => {
                crate::impl_regexp::get_regexp_missing_legacy_native_fn_meta()
            }
            Self::Ascii => crate::impl_string::ascii_fn_meta(),
            Self::Length => crate::impl_string::length_fn_meta(),
            Self::BitLength => crate::impl_string::bit_length_fn_meta(),
            Self::LTrim => crate::impl_string::ltrim_fn_meta(),
            Self::RTrim => crate::impl_string::rtrim_fn_meta(),
            Self::UnHex => crate::impl_string::unhex_fn_meta(),
            Self::Crc32 => crate::impl_math::crc32_fn_meta(),
            Self::Reverse => crate::impl_string::reverse_fn_meta(),
            Self::ReverseUtf8 => crate::impl_string::reverse_utf8_fn_meta(),
            Self::CharLength => crate::impl_string::char_length_fn_meta(),
            Self::CharLengthUtf8 => crate::impl_string::char_length_utf8_fn_meta(),
            Self::Quote => crate::impl_string::quote_fn_meta(),
            Self::HexInt => crate::impl_string::hex_int_arg_fn_meta(),
            Self::HexStr => crate::impl_string::hex_str_arg_fn_meta(),
            Self::Bin => crate::impl_string::bin_fn_meta(),
            Self::OctInt => crate::impl_string::oct_int_fn_meta(),
            Self::OctStringNative => crate::impl_string::oct_string_native_fn_meta(),
            Self::ConcatNative => crate::impl_string::concat_native_fn_meta(),
            Self::ConcatWsNative => crate::impl_string::concat_ws_native_fn_meta(),
            Self::EltNative => crate::impl_string::elt_native_fn_meta(),
            Self::FieldBytesNative => crate::impl_string::field_bytes_native_fn_meta(),
            Self::FieldIntNative => crate::impl_string::field_int_native_fn_meta(),
            Self::FieldRealNative => crate::impl_string::field_real_native_fn_meta(),
            Self::MakeSetNative => crate::impl_string::make_set_native_fn_meta(),
            Self::ExportSetNative => crate::impl_string::export_set_native_fn_meta(),
            Self::AbsIntNative => crate::impl_math::abs_int_native_fn_meta(),
            Self::AbsUIntNative => crate::impl_math::abs_uint_native_fn_meta(),
            Self::AbsRealNative => crate::impl_math::abs_real_native_fn_meta(),
            Self::AbsDecimalNative => crate::impl_math::abs_decimal_native_fn_meta(),
            Self::CeilIntNative => crate::impl_math::ceil_int_native_fn_meta(),
            Self::FloorIntNative => crate::impl_math::floor_int_native_fn_meta(),
            Self::CeilRealNative => crate::impl_math::ceil_real_native_fn_meta(),
            Self::FloorRealNative => crate::impl_math::floor_real_native_fn_meta(),
            Self::CeilDecimalNative => crate::impl_math::ceil_decimal_native_fn_meta(),
            Self::FloorDecimalNative => crate::impl_math::floor_decimal_native_fn_meta(),
            Self::RoundIntNative => crate::impl_math::round_int_native_fn_meta(),
            Self::RoundIntWithScaleNative => {
                crate::impl_math::round_int_with_scale_native_fn_meta()
            }
            Self::RoundRealNative => crate::impl_math::round_real_native_fn_meta(),
            Self::RoundDecimalNative => crate::impl_math::round_decimal_native_fn_meta(),
            Self::TruncateIntNative => crate::impl_math::truncate_int_native_fn_meta(),
            Self::TruncateUIntNative => crate::impl_math::truncate_uint_native_fn_meta(),
            Self::TruncateIntUnsignedScaleNative => {
                crate::impl_math::truncate_int_unsigned_scale_native_fn_meta()
            }
            Self::TruncateRealNative => crate::impl_math::truncate_real_native_fn_meta(),
            Self::TruncateDecimalNative => crate::impl_math::truncate_decimal_native_fn_meta(),
            Self::RoundInt128Legacy => crate::impl_math::round_int128_legacy_fn_meta(),
            Self::RoundRealLegacy => crate::impl_math::round_real_legacy_fn_meta(),
            Self::RoundDecimalLegacy => crate::impl_math::round_decimal_legacy_fn_meta(),
            Self::MathNullWitnessNative => crate::impl_math::math_null_witness_native_fn_meta(),
            Self::CharNative => crate::impl_string::char_native_fn_meta(),
            Self::ConvNative => crate::impl_math::conv_native_fn_meta(),
            Self::ConvBinaryLiteralNative => crate::impl_math::conv_binary_literal_native_fn_meta(),
            Self::ConvLegacy => crate::impl_math::conv_legacy_fn_meta(),
            Self::SinGoNative => crate::impl_math::sin_go_native_fn_meta(),
            Self::CosGoNative => crate::impl_math::cos_go_native_fn_meta(),
            Self::TanGoNative => crate::impl_math::tan_go_native_fn_meta(),
            Self::CotGoNative => crate::impl_math::cot_go_native_fn_meta(),
            Self::AtanGoNative => crate::impl_math::atan_go_native_fn_meta(),
            Self::Atan2GoNative => crate::impl_math::atan2_go_native_fn_meta(),
            Self::SinLibmLegacy => crate::impl_math::sin_libm_legacy_fn_meta(),
            Self::CosLibmLegacy => crate::impl_math::cos_libm_legacy_fn_meta(),
            Self::CotLibmLegacy => crate::impl_math::cot_libm_legacy_fn_meta(),
            Self::AtanLibmLegacy => crate::impl_math::atan_libm_legacy_fn_meta(),
            Self::Atan2LibmLegacy => crate::impl_math::atan2_libm_legacy_fn_meta(),
            Self::ExpGoNative => crate::impl_math::exp_go_native_fn_meta(),
            Self::Log10GoNative => crate::impl_math::log10_go_native_fn_meta(),
            Self::CompressGoNative => crate::impl_encryption::compress_go_native_fn_meta(),
            Self::UncompressNative => crate::impl_encryption::uncompress_native_fn_meta(),
            Self::JsonValidTextNative => crate::impl_json::json_valid_text_native_fn_meta(),
            Self::JsonValidBinaryNative => crate::impl_json::json_valid_binary_native_fn_meta(),
            Self::JsonValidOtherNative => crate::impl_json::json_valid_other_native_fn_meta(),
            Self::JsonTypeTextNative => crate::impl_json::json_type_text_native_fn_meta(),
            Self::JsonTypeBinaryNative => crate::impl_json::json_type_binary_native_fn_meta(),
            Self::JsonDepthNative => crate::impl_json::json_depth_native_fn_meta(),
            Self::JsonStorageFreeNative => crate::impl_json::json_storage_free_native_fn_meta(),
            Self::JsonStorageSizeNative => crate::impl_json::json_storage_size_native_fn_meta(),
            Self::JsonQuoteNative => crate::impl_json::json_quote_native_fn_meta(),
            Self::YearCoreNative => crate::impl_time::year_core_native_fn_meta(),
            Self::MonthCoreNative => crate::impl_time::month_core_native_fn_meta(),
            Self::DayOfMonthCoreNative => crate::impl_time::day_of_month_core_native_fn_meta(),
            Self::QuarterCoreNative => crate::impl_time::quarter_core_native_fn_meta(),
            Self::HourTextNative => crate::impl_time::hour_text_native_fn_meta(),
            Self::MinuteTextNative => crate::impl_time::minute_text_native_fn_meta(),
            Self::SecondTextNative => crate::impl_time::second_text_native_fn_meta(),
            Self::HourNanosNative => crate::impl_time::hour_nanos_native_fn_meta(),
            Self::MinuteNanosNative => crate::impl_time::minute_nanos_native_fn_meta(),
            Self::SecondNanosNative => crate::impl_time::second_nanos_native_fn_meta(),
            Self::MonthNameTextNative => crate::impl_time::month_name_text_native_fn_meta(),
            Self::TimeToSecTextNative => crate::impl_time::time_to_sec_text_native_fn_meta(),
            Self::PeriodAddNative => crate::impl_time::period_add_native_fn_meta(),
            Self::PeriodDiffNative => crate::impl_time::period_diff_native_fn_meta(),
            Self::GetFormatNative => crate::impl_time::get_format_native_fn_meta(),
            Self::GetFormatNullNative => crate::impl_time::get_format_null_native_fn_meta(),
            Self::DayOfWeekTextNative => crate::impl_time::day_of_week_text_native_fn_meta(),
            Self::WeekdayTextNative => crate::impl_time::weekday_text_native_fn_meta(),
            Self::DayOfYearTextNative => crate::impl_time::day_of_year_text_native_fn_meta(),
            Self::DayNameTextNative => crate::impl_time::day_name_text_native_fn_meta(),
            Self::DateDiffTextNative => crate::impl_time::date_diff_text_native_fn_meta(),
            Self::DateDiffNullNative => crate::impl_time::date_diff_null_native_fn_meta(),
            Self::DateDiffCoreNative => crate::impl_time::date_diff_core_native_fn_meta(),
            Self::ToDaysTextNative => crate::impl_time::to_days_text_native_fn_meta(),
            Self::ToSecondsTextNative => crate::impl_time::to_seconds_text_native_fn_meta(),
            Self::TsoLogicalNative => crate::impl_time::tso_logical_native_fn_meta(),
            Self::WeekDateTextNative => crate::impl_time::week_date_text_native_fn_meta(),
            Self::WeekTextNative => crate::impl_time::week_text_native_fn_meta(),
            Self::YearWeekTextNative => crate::impl_time::year_week_text_native_fn_meta(),
            Self::WeekOfYearTextNative => crate::impl_time::week_of_year_text_native_fn_meta(),
            Self::WeekNullNative => crate::impl_time::week_null_native_fn_meta(),
            Self::WeekCoreNative => crate::impl_time::week_core_native_fn_meta(),
            Self::PasswordNative => crate::impl_encryption::password_native_fn_meta(),
            Self::Sm3Native => crate::impl_encryption::sm3_native_fn_meta(),
            Self::MakeDateNative => crate::impl_time::make_date_native_fn_meta(),
            Self::FromDaysNative => crate::impl_time::from_days_native_fn_meta(),
            Self::MakeTimePartsNative => crate::impl_time::make_time_parts_native_fn_meta(),
            Self::SecToTimeNative => crate::impl_time::sec_to_time_native_fn_meta(),
            Self::DateFormatTextNative => crate::impl_time::date_format_text_native_fn_meta(),
            Self::DateFormatCoreNative => crate::impl_time::date_format_core_native_fn_meta(),
            Self::DateFormatNullNative => crate::impl_time::date_format_null_native_fn_meta(),
            Self::DateFormatMissingNative => crate::impl_time::date_format_missing_native_fn_meta(),
            Self::DurationTextProbeNative => crate::impl_time::duration_text_probe_native_fn_meta(),
            Self::TimeFormatTextNative => crate::impl_time::time_format_text_native_fn_meta(),
            Self::LastDayTextNative => crate::impl_time::last_day_text_native_fn_meta(),
            Self::IsUuidNative => crate::impl_miscellaneous::get_native_is_uuid_fn_meta(),
            Self::UuidVersionNative => crate::impl_miscellaneous::get_native_uuid_version_fn_meta(),
            Self::UuidTimestampNative => {
                crate::impl_miscellaneous::get_native_uuid_timestamp_fn_meta()
            }
            Self::UuidToBinParseNative => {
                crate::impl_miscellaneous::get_native_uuid_to_bin_parse_fn_meta()
            }
            Self::UuidToBinSwapNative => {
                crate::impl_miscellaneous::get_native_uuid_to_bin_swap_fn_meta()
            }
            Self::BinToUuidNative => crate::impl_miscellaneous::get_native_bin_to_uuid_fn_meta(),
            Self::TranslateUtf8Native => crate::impl_string::get_native_translate_utf8_fn_meta(),
            Self::TranslateBinaryNative => {
                crate::impl_string::get_native_translate_binary_fn_meta()
            }
            Self::TranslateNullNative => crate::impl_string::get_native_translate_null_fn_meta(),
            Self::SqlEncodeNative => crate::impl_encryption::get_native_sql_encode_fn_meta(),
            Self::SqlDecodeNative => crate::impl_encryption::get_native_sql_decode_fn_meta(),
            Self::SqlCryptNullNative => crate::impl_encryption::get_native_sql_crypt_null_fn_meta(),
            Self::TidbShardNative => crate::impl_miscellaneous::get_native_tidb_shard_fn_meta(),
            Self::VitessHashNative => crate::impl_miscellaneous::get_native_vitess_hash_fn_meta(),
            Self::FormatBytesNative => crate::impl_miscellaneous::get_native_format_bytes_fn_meta(),
            Self::FormatNanoTimeNative => {
                crate::impl_miscellaneous::get_native_format_nano_time_fn_meta()
            }
            Self::Left => crate::impl_string::left_fn_meta(),
            Self::LeftUtf8 => crate::impl_string::left_utf8_fn_meta(),
            Self::Right => crate::impl_string::right_fn_meta(),
            Self::RightUtf8 => crate::impl_string::right_utf8_fn_meta(),
            Self::Replace => crate::impl_string::replace_fn_meta(),
            Self::BitCount => crate::impl_other::bit_count_fn_meta(),
            Self::BitNeg => crate::impl_op::bit_neg_fn_meta(),
            Self::BitAnd => crate::impl_op::bit_and_fn_meta(),
            Self::BitOr => crate::impl_op::bit_or_fn_meta(),
            Self::BitXor => crate::impl_op::bit_xor_fn_meta(),
            Self::LeftShift => crate::impl_op::left_shift_fn_meta(),
            Self::RightShift => crate::impl_op::right_shift_fn_meta(),
            Self::UnaryNot | Self::IsNotNull | Self::IsNotTrue | Self::IsNotFalse => {
                crate::impl_op::unary_not_int_fn_meta()
            }
            Self::IsNull => {
                crate::impl_op::is_null_fn_meta::<tidb_query_datatype::codec::data_type::Int>()
            }
            Self::IsTrue => crate::impl_op::int_is_true_fn_meta::<crate::impl_op::KeepNullOff>(),
            Self::IsFalse => crate::impl_op::int_is_false_fn_meta::<crate::impl_op::KeepNullOff>(),
            Self::IsTrueWithNull => {
                crate::impl_op::int_is_true_fn_meta::<crate::impl_op::KeepNullOn>()
            }
            Self::Md5 => crate::impl_encryption::md5_fn_meta(),
            Self::Sha1 => crate::impl_encryption::sha1_fn_meta(),
            Self::LogicalAnd => crate::impl_op::logical_and_fn_meta(),
            Self::LogicalOr => crate::impl_op::logical_or_fn_meta(),
            Self::LogicalXor => crate::impl_op::logical_xor_fn_meta(),
            Self::InetAton => crate::impl_miscellaneous::inet_aton_fn_meta(),
            Self::InetNtoa => crate::impl_miscellaneous::inet_ntoa_fn_meta(),
            Self::Inet6Aton => crate::impl_miscellaneous::inet6_aton_fn_meta(),
            Self::Inet6Ntoa => crate::impl_miscellaneous::inet6_ntoa_fn_meta(),
            Self::AsinRaw => crate::impl_math::asin_raw_fn_meta(),
            Self::AcosRaw => crate::impl_math::acos_raw_fn_meta(),
            Self::SqrtRaw => crate::impl_math::sqrt_raw_fn_meta(),
            Self::SignRaw => crate::impl_math::sign_raw_fn_meta(),
            Self::RadiansRaw => crate::impl_math::radians_raw_fn_meta(),
            Self::DegreesRaw => crate::impl_math::degrees_raw_fn_meta(),
            Self::PiRaw => crate::impl_math::pi_raw_fn_meta(),
            Self::IsIpv4Nullable => crate::impl_miscellaneous::is_ipv4_nullable_fn_meta(),
            Self::IsIpv6Nullable => crate::impl_miscellaneous::is_ipv6_nullable_fn_meta(),
            Self::IsIpv4CompatNullable => {
                crate::impl_miscellaneous::is_ipv4_compat_nullable_fn_meta()
            }
            Self::IsIpv4MappedNullable => {
                crate::impl_miscellaneous::is_ipv4_mapped_nullable_fn_meta()
            }
            Self::SpaceNative => crate::impl_string::space_native_fn_meta(),
            Self::RepeatNative => crate::impl_string::repeat_native_fn_meta(),
            Self::ToBase64Native => crate::impl_string::to_base64_native_fn_meta(),
            Self::FromBase64Native => crate::impl_string::from_base64_native_fn_meta(),
            Self::FromBase64ValueNative => crate::impl_string::from_base64_value_native_fn_meta(),
            Self::Lower => crate::impl_string::lower_fn_meta(),
            Self::Upper => crate::impl_string::upper_fn_meta(),
            Self::LowerUtf8Ready => crate::impl_string::lower_utf8_fn_meta::<
                tidb_query_datatype::codec::collation::encoding::EncodingUtf8Mb4,
            >(),
            Self::UpperUtf8Ready => crate::impl_string::upper_utf8_fn_meta::<
                tidb_query_datatype::codec::collation::encoding::EncodingUtf8Mb4,
            >(),
            Self::Sha2Native => crate::impl_encryption::sha2_native_fn_meta(),
            Self::OrdNative => crate::impl_string::ord_native_fn_meta(),
            Self::TrimBothNative => crate::impl_string::trim_both_native_fn_meta(),
            Self::TrimLeadingNative => crate::impl_string::trim_leading_native_fn_meta(),
            Self::TrimTrailingNative => crate::impl_string::trim_trailing_native_fn_meta(),
            Self::SubstringIndexSignedNative => {
                crate::impl_string::substring_index_signed_native_fn_meta()
            }
            Self::SubstringIndexUnsignedNative => {
                crate::impl_string::substring_index_unsigned_native_fn_meta()
            }
            Self::LpadBytesNative => crate::impl_string::lpad_bytes_native_fn_meta(),
            Self::RpadBytesNative => crate::impl_string::rpad_bytes_native_fn_meta(),
            Self::LpadUtf8Native => crate::impl_string::lpad_utf8_native_fn_meta(),
            Self::RpadUtf8Native => crate::impl_string::rpad_utf8_native_fn_meta(),
            Self::LnNative => crate::impl_math::ln_native_fn_meta(),
            Self::LogNative => crate::impl_math::log_native_fn_meta(),
            Self::Log2Native => crate::impl_math::log2_native_fn_meta(),
            Self::PowNative => crate::impl_math::pow_native_fn_meta(),
            Self::UncompressedLengthNative => {
                crate::impl_encryption::uncompressed_length_native_fn_meta()
            }
            Self::Insert => crate::impl_string::insert_fn_meta(),
            Self::InsertUtf8Native => crate::impl_string::insert_utf8_native_fn_meta(),
            Self::LowerAsciiNative => crate::impl_string::lower_ascii_native_fn_meta(),
            Self::UpperAsciiNative => crate::impl_string::upper_ascii_native_fn_meta(),
            Self::Substring2BytesNative => crate::impl_string::substring_2_bytes_native_fn_meta(),
            Self::Substring3BytesNative => crate::impl_string::substring_3_bytes_native_fn_meta(),
            Self::Substring2Utf8Native => crate::impl_string::substring_2_utf8_native_fn_meta(),
            Self::Substring3Utf8Native => crate::impl_string::substring_3_utf8_native_fn_meta(),
            Self::Substring2BytesLegacy => crate::impl_string::substring_2_bytes_legacy_fn_meta(),
            Self::Substring3BytesLegacy => crate::impl_string::substring_3_bytes_legacy_fn_meta(),
            Self::Substring2Utf8Legacy => crate::impl_string::substring_2_utf8_legacy_fn_meta(),
            Self::Substring3Utf8Legacy => crate::impl_string::substring_3_utf8_legacy_fn_meta(),
            Self::StrcmpNative => crate::impl_string::strcmp_native_fn_meta(),
            Self::Locate2Native => crate::impl_string::locate_2_native_fn_meta(),
            Self::Locate3Native => crate::impl_string::locate_3_native_fn_meta(),
            Self::Locate3BytesExtNative => crate::impl_string::locate_3_bytes_ext_native_fn_meta(),
            Self::Locate3Utf8ExtNative => crate::impl_string::locate_3_utf8_ext_native_fn_meta(),
            Self::FindInSetNative => crate::impl_string::find_in_set_native_fn_meta(),
            Self::FindInSetPreparedNative => {
                crate::impl_string::find_in_set_prepared_native_fn_meta()
            }
        }
    }

    pub(crate) fn eval_type(self) -> EvalType {
        match self {
            Self::ConvertTzNative => EvalType::Bytes,
            Self::DateLiteralNative | Self::TimestampLiteralNative => EvalType::Bytes,
            Self::JsonSearchSerdeNative => EvalType::Bytes,
            Self::TimestampAddNative | Self::TimestampAddPrefixNullNative => EvalType::Bytes,
            Self::AddTimeNative | Self::SubTimeNative | Self::TimeAddRightDatetimeNative => {
                EvalType::Bytes
            }
            Self::TimeNative => EvalType::Bytes,
            Self::MicrosecondNative | Self::MicrosecondLegacy => EvalType::Int,
            Self::IntDivDecimalSignedNative | Self::IntDivDecimalUnsignedNative => EvalType::Bytes,
            Self::IntDivDecimalLegacy => EvalType::Int,
            Self::IntDivIntSsNative
            | Self::IntDivIntUsNative
            | Self::IntDivIntSuNative
            | Self::IntDivIntUuNative => EvalType::Int,
            Self::IntDivInt128Legacy => EvalType::Bytes,
            Self::TidbParseTsoNative | Self::TimeDiffTextNative => EvalType::Bytes,
            Self::AnyValueNative | Self::NameConstNative => EvalType::Bytes,
            Self::WeightStringNative
            | Self::WeightStringCharNative
            | Self::WeightStringBinaryNative
            | Self::WeightStringNumericNative
            | Self::FormatLocaleNative => EvalType::Bytes,
            Self::DateCoreNative | Self::DateCorePredicateLegacy => EvalType::Int,
            Self::NowNative | Self::CurrentDateNative | Self::SysdateNative => EvalType::Bytes,
            Self::JsonMergeSerdeNative
            | Self::JsonMergePatchSerdeNative
            | Self::JsonMergePatchRawLegacy => EvalType::Bytes,
            Self::UtcDateNative
            | Self::UtcTimestampNative
            | Self::CurrentTimeWithoutFspNative
            | Self::CurrentTimeWithFspNative
            | Self::UtcTimeWithoutFspNative
            | Self::UtcTimeWithFspNative
            | Self::UtcTimeNullNative => EvalType::Bytes,
            Self::JsonArraySerdeNative
            | Self::JsonObjectSerdeNative
            | Self::JsonKeysSerdeNative
            | Self::JsonKeysPathSerdeNative
            | Self::JsonPrettySerdeNative
            | Self::JsonOutputNullNative
            | Self::JsonExtractSerdeNative
            | Self::JsonInsertSerdeNative
            | Self::JsonSetSerdeNative
            | Self::JsonReplaceSerdeNative
            | Self::JsonRemoveSerdeNative
            | Self::JsonArrayAppendSerdeNative
            | Self::JsonArrayInsertSerdeNative
            | Self::JsonReplaceRawLegacy
            | Self::JsonArrayAppendRawLegacy
            | Self::JsonArrayAppendEmptyLegacy
            | Self::JsonValueAbsentLegacy
            | Self::JsonUnquoteTextNative
            | Self::JsonUnquoteBinaryNative => EvalType::Bytes,
            Self::CompareIntSsNative(_)
            | Self::CompareIntSuNative(_)
            | Self::CompareIntUsNative(_)
            | Self::CompareIntUuNative(_)
            | Self::CompareInt128Legacy(_)
            | Self::CompareRealNative(_)
            | Self::CompareRealLegacy(_)
            | Self::CompareDecimalNative(_)
            | Self::CompareBytesNative(_)
            | Self::CompareVectorNative(_)
            | Self::CompareTimeCoreNative(_)
            | Self::CompareDurationNative(_)
            | Self::CompareJsonNative(_)
            | Self::CompareNullNative
            | Self::CompareMissingLegacy
            | Self::GroupingBitAndNative
            | Self::GroupingNumericCmpNative
            | Self::GroupingNumericSetNative
            | Self::GroupingNullNative
            | Self::JsonContainsSerdeNative
            | Self::JsonContainsPathSerdeNative
            | Self::JsonOverlapsSerdeNative
            | Self::JsonMemberOfSerdeNative
            | Self::JsonLengthSerdeNative
            | Self::JsonLengthPathSerdeNative
            | Self::JsonPathExistsSerdeNative
            | Self::JsonMemberOfBinaryLegacy
            | Self::JsonPredicateNullNative
            | Self::JsonPredicateMissingLegacy => EvalType::Int,
            Self::AesEncrypt128EcbNative
            | Self::AesEncrypt192EcbNative
            | Self::AesEncrypt256EcbNative
            | Self::AesDecrypt128EcbNative
            | Self::AesDecrypt192EcbNative
            | Self::AesDecrypt256EcbNative
            | Self::AesEncrypt128CbcNative
            | Self::AesEncrypt192CbcNative
            | Self::AesEncrypt256CbcNative
            | Self::AesDecrypt128CbcNative
            | Self::AesDecrypt192CbcNative
            | Self::AesDecrypt256CbcNative
            | Self::AesEncrypt128OfbNative
            | Self::AesEncrypt192OfbNative
            | Self::AesEncrypt256OfbNative
            | Self::AesDecrypt128OfbNative
            | Self::AesDecrypt192OfbNative
            | Self::AesDecrypt256OfbNative
            | Self::AesEncrypt128CfbNative
            | Self::AesEncrypt192CfbNative
            | Self::AesEncrypt256CfbNative
            | Self::AesDecrypt128CfbNative
            | Self::AesDecrypt192CfbNative
            | Self::AesDecrypt256CfbNative
            | Self::AesNullNative => EvalType::Bytes,
            Self::DivDecimalNative | Self::DivDecimalLegacy => EvalType::Decimal,
            Self::AddIntSsNative
            | Self::AddIntSuNative
            | Self::AddIntUsNative
            | Self::AddIntUuNative
            | Self::SubIntSsNative
            | Self::SubIntSuNative
            | Self::SubIntUsNative
            | Self::SubIntUuNative
            | Self::SubIntSuForcedNative
            | Self::SubIntUsForcedNative
            | Self::SubIntUuForcedNative
            | Self::MulIntSignedNative
            | Self::ModIntSsNative
            | Self::ModIntSuNative
            | Self::ModIntUsNative
            | Self::ModIntUuNative
            | Self::MulIntUnsignedNative
            | Self::BinaryArithmeticNullNative
            | Self::BinaryArithmeticMissingLegacy => EvalType::Int,
            Self::AddDecimalNative
            | Self::SubDecimalNative
            | Self::MulDecimalNative
            | Self::ModDecimalNative
            | Self::AddDecimalLegacy
            | Self::SubDecimalLegacy
            | Self::MulDecimalLegacy => EvalType::Decimal,
            Self::DivRealNative
            | Self::DivRealLegacy
            | Self::AddRealNative
            | Self::SubRealNative
            | Self::MulRealNative
            | Self::ModRealNative
            | Self::ModRealLegacy
            | Self::AddRealLegacy
            | Self::SubRealLegacy
            | Self::MulRealLegacy
            | Self::AddVectorNative
            | Self::SubVectorNative
            | Self::MulVectorNative
            | Self::AddInt128SignedLegacy
            | Self::AddInt128UnsignedLegacy
            | Self::AddInt128RejectLeftLegacy
            | Self::AddInt128RejectRightLegacy
            | Self::SubInt128SignedLegacy
            | Self::SubInt128UnsignedLegacy
            | Self::SubInt128RejectLeftLegacy
            | Self::SubInt128RejectRightLegacy
            | Self::MulInt128SignedLegacy
            | Self::ModInt128Legacy
            | Self::MulInt128UnsignedLegacy
            | Self::AddDecimalFastNative
            | Self::SubDecimalFastNative
            | Self::MulDecimalFastNative => EvalType::Bytes,
            Self::UnaryPlusIntNative
            | Self::UnaryMinusIntNative
            | Self::UnaryMinusUIntNative
            | Self::UnaryNullNative => EvalType::Int,
            Self::UnaryPlusDecimalNative
            | Self::UnaryMinusDecimalNative
            | Self::UnaryMinusIntConstantNative
            | Self::UnaryMinusUIntConstantNative => EvalType::Decimal,
            Self::UnaryPlusBitsNative | Self::UnaryMinusBitsNative | Self::UnaryPlusBytesNative => {
                EvalType::Bytes
            }
            Self::Ascii
            | Self::Length
            | Self::BitLength
            | Self::Crc32
            | Self::CharLength
            | Self::CharLengthUtf8
            | Self::BitCount
            | Self::BitNeg
            | Self::BitAnd
            | Self::BitOr
            | Self::BitXor
            | Self::LeftShift
            | Self::RightShift
            | Self::UnaryNot
            | Self::IsNull
            | Self::IsTrue
            | Self::IsFalse
            | Self::IsTrueWithNull
            | Self::IsNotNull
            | Self::IsNotTrue
            | Self::IsNotFalse
            | Self::LogicalAnd
            | Self::LogicalOr
            | Self::LogicalXor
            | Self::InetAton
            | Self::SignRaw
            | Self::IsIpv4Nullable
            | Self::IsIpv6Nullable
            | Self::IsIpv4CompatNullable
            | Self::IsIpv4MappedNullable
            | Self::OrdNative
            | Self::UncompressedLengthNative
            | Self::StrcmpNative
            | Self::Locate2Native
            | Self::Locate3Native
            | Self::Locate3BytesExtNative
            | Self::Locate3Utf8ExtNative
            | Self::FindInSetNative
            | Self::FindInSetPreparedNative
            | Self::FieldBytesNative
            | Self::FieldIntNative
            | Self::FieldRealNative
            | Self::AbsIntNative
            | Self::AbsUIntNative
            | Self::CeilIntNative
            | Self::FloorIntNative
            | Self::RoundIntNative
            | Self::RoundIntWithScaleNative
            | Self::TruncateIntNative
            | Self::TruncateUIntNative
            | Self::TruncateIntUnsignedScaleNative
            | Self::MathNullWitnessNative
            | Self::JsonValidTextNative
            | Self::JsonValidBinaryNative
            | Self::JsonValidOtherNative
            | Self::YearCoreNative
            | Self::MonthCoreNative
            | Self::DayOfMonthCoreNative
            | Self::QuarterCoreNative
            | Self::HourTextNative
            | Self::MinuteTextNative
            | Self::SecondTextNative
            | Self::HourNanosNative
            | Self::MinuteNanosNative
            | Self::SecondNanosNative
            | Self::TimeToSecTextNative
            | Self::PeriodAddNative
            | Self::PeriodDiffNative
            | Self::DayOfWeekTextNative
            | Self::WeekdayTextNative
            | Self::DayOfYearTextNative
            | Self::DateDiffTextNative
            | Self::DateDiffNullNative
            | Self::DateDiffCoreNative
            | Self::ToDaysTextNative
            | Self::ToSecondsTextNative
            | Self::TsoLogicalNative
            | Self::WeekTextNative
            | Self::YearWeekTextNative
            | Self::WeekOfYearTextNative
            | Self::WeekNullNative
            | Self::WeekCoreNative
            | Self::DateFormatMissingNative
            | Self::IsUuidNative
            | Self::UuidVersionNative
            | Self::TidbShardNative
            | Self::VitessHashNative
            | Self::VecDimsNative
            | Self::RegexpLikeNative
            | Self::RegexpInstrNative
            | Self::RegexpLikeLegacyCiNative
            | Self::RegexpLikeLegacyBinNative
            | Self::RegexpNullIntNative
            | Self::RegexpMissingLegacyNative
            | Self::LikeNative
            | Self::IlikeNative
            | Self::LikeLegacyNative
            | Self::LikeNullIntNative
            | Self::LikeMissingLegacyNative => EvalType::Int,
            Self::AbsDecimalNative
            | Self::CeilDecimalNative
            | Self::FloorDecimalNative
            | Self::RoundDecimalNative
            | Self::TruncateDecimalNative
            | Self::UuidTimestampNative => EvalType::Decimal,
            Self::LTrim
            | Self::RTrim
            | Self::UnHex
            | Self::Reverse
            | Self::ReverseUtf8
            | Self::Quote
            | Self::HexInt
            | Self::HexStr
            | Self::Bin
            | Self::Left
            | Self::LeftUtf8
            | Self::Right
            | Self::RightUtf8
            | Self::Replace
            | Self::Md5
            | Self::Sha1
            | Self::InetNtoa
            | Self::Inet6Aton
            | Self::Inet6Ntoa
            | Self::AsinRaw
            | Self::AcosRaw
            | Self::SqrtRaw
            | Self::RadiansRaw
            | Self::DegreesRaw
            | Self::PiRaw
            | Self::SpaceNative
            | Self::RepeatNative
            | Self::ToBase64Native
            | Self::FromBase64Native
            | Self::FromBase64ValueNative
            | Self::Lower
            | Self::Upper
            | Self::LowerUtf8Ready
            | Self::UpperUtf8Ready
            | Self::Sha2Native
            | Self::TrimBothNative
            | Self::TrimLeadingNative
            | Self::TrimTrailingNative
            | Self::SubstringIndexSignedNative
            | Self::SubstringIndexUnsignedNative
            | Self::LpadBytesNative
            | Self::RpadBytesNative
            | Self::LpadUtf8Native
            | Self::RpadUtf8Native
            | Self::LnNative
            | Self::LogNative
            | Self::Log2Native
            | Self::PowNative
            | Self::Insert
            | Self::InsertUtf8Native
            | Self::LowerAsciiNative
            | Self::UpperAsciiNative
            | Self::Substring2BytesNative
            | Self::Substring3BytesNative
            | Self::Substring2Utf8Native
            | Self::Substring3Utf8Native
            | Self::Substring2BytesLegacy
            | Self::Substring3BytesLegacy
            | Self::Substring2Utf8Legacy
            | Self::Substring3Utf8Legacy
            | Self::OctInt
            | Self::OctStringNative
            | Self::ConcatNative
            | Self::ConcatWsNative
            | Self::EltNative
            | Self::MakeSetNative
            | Self::ExportSetNative
            | Self::AbsRealNative
            | Self::CeilRealNative
            | Self::FloorRealNative
            | Self::RoundRealNative
            | Self::TruncateRealNative
            | Self::RoundRealLegacy
            | Self::RoundDecimalLegacy
            | Self::RoundInt128Legacy
            | Self::CharNative
            | Self::ConvNative
            | Self::ConvBinaryLiteralNative
            | Self::ConvLegacy
            | Self::SinGoNative
            | Self::CosGoNative
            | Self::TanGoNative
            | Self::CotGoNative
            | Self::AtanGoNative
            | Self::Atan2GoNative
            | Self::SinLibmLegacy
            | Self::CosLibmLegacy
            | Self::CotLibmLegacy
            | Self::AtanLibmLegacy
            | Self::Atan2LibmLegacy
            | Self::ExpGoNative
            | Self::Log10GoNative
            | Self::CompressGoNative
            | Self::UncompressNative
            | Self::JsonTypeTextNative
            | Self::JsonTypeBinaryNative
            | Self::JsonDepthNative
            | Self::JsonStorageFreeNative
            | Self::JsonStorageSizeNative
            | Self::JsonQuoteNative
            | Self::MonthNameTextNative
            | Self::GetFormatNative
            | Self::GetFormatNullNative
            | Self::DayNameTextNative
            | Self::WeekDateTextNative
            | Self::PasswordNative
            | Self::Sm3Native
            | Self::MakeDateNative
            | Self::FromDaysNative
            | Self::MakeTimePartsNative
            | Self::SecToTimeNative
            | Self::DateFormatTextNative
            | Self::DateFormatCoreNative
            | Self::DateFormatNullNative
            | Self::DurationTextProbeNative
            | Self::TimeFormatTextNative
            | Self::LastDayTextNative
            | Self::UuidToBinParseNative
            | Self::UuidToBinSwapNative
            | Self::BinToUuidNative
            | Self::TranslateUtf8Native
            | Self::TranslateBinaryNative
            | Self::TranslateNullNative
            | Self::SqlEncodeNative
            | Self::SqlDecodeNative
            | Self::SqlCryptNullNative
            | Self::FormatBytesNative
            | Self::FormatNanoTimeNative
            | Self::VecAsTextNative
            | Self::VecL1DistanceNative
            | Self::VecL2DistanceNative
            | Self::VecNegativeInnerProductNative
            | Self::VecCosineDistanceNative
            | Self::VecL2NormNative
            | Self::VecFromTextNative
            | Self::VecRealNullNative
            | Self::RegexpSubstrNative
            | Self::RegexpReplaceNative
            | Self::RegexpNullBytesNative => EvalType::Bytes,
        }
    }

    pub(crate) fn return_type(self) -> tipb::FieldType {
        match self.eval_type() {
            EvalType::Int => evaluated_ascii_int_type(),
            EvalType::Bytes => evaluated_ascii_bytes_type(),
            EvalType::Decimal => evaluated_ascii_decimal_type(),
            _ => unreachable!("the operation has a closed Int/Bytes/Decimal result"),
        }
    }

    pub(crate) fn input_types(self) -> &'static [EvalType] {
        match self {
            Self::ConvertTzNative => &[EvalType::Bytes, EvalType::Bytes, EvalType::Bytes],
            Self::DateLiteralNative | Self::TimestampLiteralNative => {
                &[EvalType::Bytes, EvalType::Int]
            }
            Self::JsonSearchSerdeNative => &[EvalType::Bytes, EvalType::Bytes, EvalType::Bytes],
            Self::TimestampAddNative => &[EvalType::Bytes, EvalType::Bytes, EvalType::Int],
            Self::TimestampAddPrefixNullNative => &[EvalType::Bytes, EvalType::Int],
            Self::AddTimeNative | Self::SubTimeNative => {
                &[EvalType::Bytes, EvalType::Bytes, EvalType::Int]
            }
            Self::TimeAddRightDatetimeNative => &[EvalType::Int],
            Self::TimeNative | Self::MicrosecondNative => &[EvalType::Bytes],
            Self::MicrosecondLegacy => &[EvalType::Int],
            Self::IntDivDecimalSignedNative | Self::IntDivDecimalUnsignedNative => &[
                EvalType::Bytes,
                EvalType::Int,
                EvalType::Int,
                EvalType::Bytes,
                EvalType::Int,
            ],
            Self::IntDivDecimalLegacy => &[EvalType::Bytes, EvalType::Bytes, EvalType::Int],
            Self::IntDivIntSsNative
            | Self::IntDivIntUsNative
            | Self::IntDivIntSuNative
            | Self::IntDivIntUuNative => &[EvalType::Int, EvalType::Int],
            Self::IntDivInt128Legacy => &[EvalType::Bytes, EvalType::Bytes],
            Self::TidbParseTsoNative => &[EvalType::Int, EvalType::Int],
            Self::TimeDiffTextNative => &[EvalType::Bytes, EvalType::Bytes],
            Self::AnyValueNative | Self::NameConstNative => &[EvalType::Bytes],
            Self::WeightStringNative
            | Self::WeightStringCharNative
            | Self::WeightStringBinaryNative => &[EvalType::Bytes, EvalType::Bytes],
            Self::WeightStringNumericNative => &[EvalType::Int],
            Self::FormatLocaleNative => &[EvalType::Bytes, EvalType::Bytes, EvalType::Int],
            Self::DateCoreNative => &[EvalType::Bytes, EvalType::Int],
            Self::DateCorePredicateLegacy => &[EvalType::Bytes],
            Self::NowNative | Self::SysdateNative => &[EvalType::Bytes, EvalType::Int],
            Self::CurrentDateNative => &[EvalType::Bytes],
            Self::JsonMergeSerdeNative
            | Self::JsonMergePatchSerdeNative
            | Self::JsonMergePatchRawLegacy => &[EvalType::Bytes],
            Self::UtcDateNative
            | Self::CurrentTimeWithoutFspNative
            | Self::UtcTimeWithoutFspNative => &[EvalType::Bytes],
            Self::UtcTimestampNative
            | Self::CurrentTimeWithFspNative
            | Self::UtcTimeWithFspNative => &[EvalType::Bytes, EvalType::Int],
            Self::UtcTimeNullNative => &[EvalType::Int],
            Self::JsonReplaceRawLegacy | Self::JsonArrayAppendRawLegacy => {
                &[EvalType::Bytes, EvalType::Bytes, EvalType::Bytes]
            }
            Self::JsonArrayAppendEmptyLegacy
            | Self::JsonUnquoteTextNative
            | Self::JsonUnquoteBinaryNative => &[EvalType::Bytes],
            Self::JsonValueAbsentLegacy => &[],
            Self::JsonArraySerdeNative
            | Self::JsonObjectSerdeNative
            | Self::JsonKeysSerdeNative
            | Self::JsonPrettySerdeNative => &[EvalType::Bytes],
            Self::JsonKeysPathSerdeNative
            | Self::JsonExtractSerdeNative
            | Self::JsonRemoveSerdeNative => &[EvalType::Bytes, EvalType::Bytes],
            Self::JsonInsertSerdeNative
            | Self::JsonSetSerdeNative
            | Self::JsonReplaceSerdeNative
            | Self::JsonArrayAppendSerdeNative
            | Self::JsonArrayInsertSerdeNative => {
                &[EvalType::Bytes, EvalType::Bytes, EvalType::Bytes]
            }
            Self::JsonOutputNullNative => &[EvalType::Int],
            Self::CompareIntSsNative(_)
            | Self::CompareIntSuNative(_)
            | Self::CompareIntUsNative(_)
            | Self::CompareIntUuNative(_)
            | Self::CompareDurationNative(_) => &[EvalType::Int, EvalType::Int],
            Self::CompareInt128Legacy(_)
            | Self::CompareRealNative(_)
            | Self::CompareRealLegacy(_)
            | Self::CompareTimeCoreNative(_)
            | Self::CompareJsonNative(_) => &[EvalType::Bytes, EvalType::Bytes],
            Self::CompareDecimalNative(_) => &[EvalType::Decimal, EvalType::Decimal, EvalType::Int],
            Self::CompareBytesNative(_) => &[EvalType::Bytes, EvalType::Bytes, EvalType::Int],
            Self::CompareVectorNative(_) => &[EvalType::VectorFloat32, EvalType::VectorFloat32],
            Self::CompareNullNative | Self::GroupingNullNative | Self::JsonPredicateNullNative => {
                &[EvalType::Int]
            }
            Self::CompareMissingLegacy | Self::JsonPredicateMissingLegacy => &[],
            Self::JsonContainsSerdeNative
            | Self::JsonOverlapsSerdeNative
            | Self::JsonMemberOfSerdeNative
            | Self::JsonLengthPathSerdeNative
            | Self::JsonPathExistsSerdeNative
            | Self::JsonMemberOfBinaryLegacy => &[EvalType::Bytes, EvalType::Bytes],
            Self::JsonContainsPathSerdeNative => {
                &[EvalType::Bytes, EvalType::Bytes, EvalType::Bytes]
            }
            Self::JsonLengthSerdeNative => &[EvalType::Bytes],
            Self::GroupingBitAndNative
            | Self::GroupingNumericCmpNative
            | Self::GroupingNumericSetNative => &[EvalType::Bytes, EvalType::Bytes],
            Self::AesEncrypt128EcbNative
            | Self::AesEncrypt192EcbNative
            | Self::AesEncrypt256EcbNative
            | Self::AesDecrypt128EcbNative
            | Self::AesDecrypt192EcbNative
            | Self::AesDecrypt256EcbNative => &[EvalType::Bytes, EvalType::Bytes],
            Self::AesEncrypt128CbcNative
            | Self::AesEncrypt192CbcNative
            | Self::AesEncrypt256CbcNative
            | Self::AesDecrypt128CbcNative
            | Self::AesDecrypt192CbcNative
            | Self::AesDecrypt256CbcNative
            | Self::AesEncrypt128OfbNative
            | Self::AesEncrypt192OfbNative
            | Self::AesEncrypt256OfbNative
            | Self::AesDecrypt128OfbNative
            | Self::AesDecrypt192OfbNative
            | Self::AesDecrypt256OfbNative
            | Self::AesEncrypt128CfbNative
            | Self::AesEncrypt192CfbNative
            | Self::AesEncrypt256CfbNative
            | Self::AesDecrypt128CfbNative
            | Self::AesDecrypt192CfbNative
            | Self::AesDecrypt256CfbNative => &[EvalType::Bytes, EvalType::Bytes, EvalType::Bytes],
            Self::AesNullNative => &[EvalType::Int],
            Self::DivDecimalNative | Self::DivDecimalLegacy => {
                &[EvalType::Decimal, EvalType::Decimal, EvalType::Int]
            }
            Self::AddIntSsNative
            | Self::AddIntSuNative
            | Self::AddIntUsNative
            | Self::AddIntUuNative
            | Self::SubIntSsNative
            | Self::SubIntSuNative
            | Self::SubIntUsNative
            | Self::SubIntUuNative
            | Self::SubIntSuForcedNative
            | Self::SubIntUsForcedNative
            | Self::SubIntUuForcedNative
            | Self::MulIntSignedNative
            | Self::ModIntSsNative
            | Self::ModIntSuNative
            | Self::ModIntUsNative
            | Self::ModIntUuNative
            | Self::MulIntUnsignedNative => &[EvalType::Int, EvalType::Int],
            Self::DivRealNative
            | Self::DivRealLegacy
            | Self::AddRealNative
            | Self::SubRealNative
            | Self::MulRealNative
            | Self::ModRealNative
            | Self::ModRealLegacy
            | Self::AddRealLegacy
            | Self::SubRealLegacy
            | Self::MulRealLegacy
            | Self::AddInt128SignedLegacy
            | Self::AddInt128UnsignedLegacy
            | Self::AddInt128RejectLeftLegacy
            | Self::AddInt128RejectRightLegacy
            | Self::SubInt128SignedLegacy
            | Self::SubInt128UnsignedLegacy
            | Self::SubInt128RejectLeftLegacy
            | Self::SubInt128RejectRightLegacy
            | Self::MulInt128SignedLegacy
            | Self::ModInt128Legacy
            | Self::MulInt128UnsignedLegacy => &[EvalType::Bytes, EvalType::Bytes],
            Self::AddDecimalNative
            | Self::SubDecimalNative
            | Self::MulDecimalNative
            | Self::ModDecimalNative
            | Self::AddDecimalLegacy
            | Self::SubDecimalLegacy
            | Self::MulDecimalLegacy
            | Self::AddDecimalFastNative
            | Self::SubDecimalFastNative
            | Self::MulDecimalFastNative => &[EvalType::Decimal, EvalType::Decimal, EvalType::Int],
            Self::AddVectorNative | Self::SubVectorNative | Self::MulVectorNative => {
                &[EvalType::VectorFloat32, EvalType::VectorFloat32]
            }
            Self::BinaryArithmeticNullNative => &[EvalType::Int],
            Self::BinaryArithmeticMissingLegacy => &[],
            Self::UnaryPlusIntNative
            | Self::UnaryMinusIntNative
            | Self::UnaryMinusUIntNative
            | Self::UnaryMinusIntConstantNative
            | Self::UnaryMinusUIntConstantNative
            | Self::UnaryNullNative => &[EvalType::Int],
            Self::UnaryPlusBitsNative | Self::UnaryMinusBitsNative | Self::UnaryPlusBytesNative => {
                &[EvalType::Bytes]
            }
            Self::UnaryPlusDecimalNative | Self::UnaryMinusDecimalNative => {
                &[EvalType::Decimal, EvalType::Int]
            }
            Self::LikeNative | Self::IlikeNative | Self::LikeLegacyNative => {
                &[EvalType::Bytes, EvalType::Bytes, EvalType::Int]
            }
            Self::LikeNullIntNative => &[EvalType::Int],
            Self::LikeMissingLegacyNative => &[],
            Self::RegexpLikeNative => &[EvalType::Bytes, EvalType::Bytes, EvalType::Bytes],
            Self::RegexpSubstrNative => &[
                EvalType::Bytes,
                EvalType::Bytes,
                EvalType::Int,
                EvalType::Int,
                EvalType::Bytes,
            ],
            Self::RegexpInstrNative => &[
                EvalType::Bytes,
                EvalType::Bytes,
                EvalType::Int,
                EvalType::Int,
                EvalType::Int,
                EvalType::Bytes,
            ],
            Self::RegexpReplaceNative => &[
                EvalType::Bytes,
                EvalType::Bytes,
                EvalType::Bytes,
                EvalType::Int,
                EvalType::Int,
                EvalType::Bytes,
            ],
            Self::RegexpLikeLegacyCiNative | Self::RegexpLikeLegacyBinNative => {
                &[EvalType::Bytes, EvalType::Bytes]
            }
            Self::RegexpNullIntNative | Self::RegexpNullBytesNative => &[EvalType::Int],
            Self::VecAsTextNative | Self::VecDimsNative | Self::VecL2NormNative => {
                &[EvalType::VectorFloat32]
            }
            Self::VecL1DistanceNative
            | Self::VecL2DistanceNative
            | Self::VecNegativeInnerProductNative
            | Self::VecCosineDistanceNative => &[EvalType::VectorFloat32, EvalType::VectorFloat32],
            Self::VecFromTextNative => &[EvalType::Bytes],
            Self::VecRealNullNative => &[EvalType::Int],
            Self::SqlEncodeNative | Self::SqlDecodeNative => &[EvalType::Bytes, EvalType::Bytes],
            Self::SqlCryptNullNative | Self::TidbShardNative | Self::VitessHashNative => {
                &[EvalType::Int]
            }
            Self::FormatBytesNative | Self::FormatNanoTimeNative => &[EvalType::Bytes],
            Self::IsUuidNative
            | Self::UuidVersionNative
            | Self::UuidTimestampNative
            | Self::UuidToBinParseNative => &[EvalType::Bytes],
            Self::UuidToBinSwapNative | Self::BinToUuidNative => &[EvalType::Bytes, EvalType::Int],
            Self::TranslateUtf8Native | Self::TranslateBinaryNative => {
                &[EvalType::Bytes, EvalType::Bytes, EvalType::Bytes]
            }
            Self::TranslateNullNative => &[EvalType::Int],
            Self::PiRaw
            | Self::JsonValidOtherNative
            | Self::DateFormatMissingNative
            | Self::RegexpMissingLegacyNative => &[],
            Self::DateFormatTextNative
            | Self::DateFormatCoreNative
            | Self::TimeFormatTextNative => &[EvalType::Bytes, EvalType::Bytes],
            Self::DateFormatNullNative => &[EvalType::Int],
            Self::DurationTextProbeNative | Self::LastDayTextNative => &[EvalType::Bytes],
            Self::MakeDateNative => &[EvalType::Int, EvalType::Int],
            Self::FromDaysNative => &[EvalType::Int],
            Self::MakeTimePartsNative => &[EvalType::Bytes, EvalType::Int, EvalType::Bytes],
            Self::SecToTimeNative => &[EvalType::Bytes, EvalType::Int],
            Self::DateDiffNullNative | Self::TsoLogicalNative | Self::WeekNullNative => {
                &[EvalType::Int]
            }
            Self::WeekTextNative | Self::YearWeekTextNative => &[EvalType::Bytes, EvalType::Int],
            Self::PeriodAddNative | Self::PeriodDiffNative => &[EvalType::Int, EvalType::Int],
            Self::GetFormatNative | Self::DateDiffTextNative | Self::DateDiffCoreNative => {
                &[EvalType::Bytes, EvalType::Bytes]
            }
            Self::HourNanosNative | Self::MinuteNanosNative | Self::SecondNanosNative => {
                &[EvalType::Int]
            }
            Self::OctInt => &[EvalType::Int],
            Self::OctStringNative | Self::ConcatNative | Self::ConcatWsNative => &[EvalType::Bytes],
            Self::EltNative => &[EvalType::Int, EvalType::Int, EvalType::Bytes],
            Self::FieldBytesNative
            | Self::FieldIntNative
            | Self::FieldRealNative
            | Self::MakeSetNative => &[EvalType::Bytes],
            Self::ExportSetNative => &[EvalType::Bytes, EvalType::Int, EvalType::Int],
            Self::CharNative => &[EvalType::Bytes],
            Self::ConvNative | Self::ConvBinaryLiteralNative => {
                &[EvalType::Bytes, EvalType::Int, EvalType::Int]
            }
            Self::ConvLegacy => &[EvalType::Bytes, EvalType::Bytes, EvalType::Bytes],
            Self::SinGoNative
            | Self::CosGoNative
            | Self::TanGoNative
            | Self::CotGoNative
            | Self::AtanGoNative
            | Self::SinLibmLegacy
            | Self::CosLibmLegacy
            | Self::CotLibmLegacy
            | Self::AtanLibmLegacy
            | Self::ExpGoNative
            | Self::Log10GoNative
            | Self::CompressGoNative
            | Self::UncompressNative
            | Self::JsonValidTextNative
            | Self::JsonValidBinaryNative
            | Self::JsonTypeTextNative
            | Self::JsonTypeBinaryNative
            | Self::JsonDepthNative
            | Self::JsonStorageFreeNative
            | Self::JsonStorageSizeNative
            | Self::JsonQuoteNative
            | Self::YearCoreNative
            | Self::MonthCoreNative
            | Self::DayOfMonthCoreNative
            | Self::QuarterCoreNative
            | Self::HourTextNative
            | Self::MinuteTextNative
            | Self::SecondTextNative
            | Self::MonthNameTextNative
            | Self::TimeToSecTextNative
            | Self::GetFormatNullNative
            | Self::DayOfWeekTextNative
            | Self::WeekdayTextNative
            | Self::DayOfYearTextNative
            | Self::DayNameTextNative
            | Self::ToDaysTextNative
            | Self::ToSecondsTextNative
            | Self::WeekDateTextNative
            | Self::WeekOfYearTextNative
            | Self::WeekCoreNative
            | Self::PasswordNative
            | Self::Sm3Native => &[EvalType::Bytes],
            Self::Atan2GoNative | Self::Atan2LibmLegacy => &[EvalType::Bytes, EvalType::Bytes],
            Self::AbsIntNative
            | Self::AbsUIntNative
            | Self::CeilIntNative
            | Self::FloorIntNative
            | Self::RoundIntNative
            | Self::MathNullWitnessNative => &[EvalType::Int],
            Self::RoundIntWithScaleNative
            | Self::TruncateIntNative
            | Self::TruncateUIntNative
            | Self::TruncateIntUnsignedScaleNative => &[EvalType::Int, EvalType::Int],
            Self::AbsRealNative
            | Self::CeilRealNative
            | Self::FloorRealNative
            | Self::RoundRealLegacy
            | Self::RoundInt128Legacy => &[EvalType::Bytes],
            Self::RoundRealNative | Self::TruncateRealNative => &[EvalType::Bytes, EvalType::Int],
            Self::AbsDecimalNative
            | Self::CeilDecimalNative
            | Self::FloorDecimalNative
            | Self::RoundDecimalLegacy => &[EvalType::Decimal, EvalType::Int],
            Self::RoundDecimalNative | Self::TruncateDecimalNative => {
                &[EvalType::Decimal, EvalType::Int, EvalType::Int]
            }
            Self::StrcmpNative
            | Self::Locate2Native
            | Self::Locate3BytesExtNative
            | Self::Locate3Utf8ExtNative
            | Self::FindInSetNative
            | Self::FindInSetPreparedNative => &[EvalType::Bytes, EvalType::Bytes, EvalType::Int],
            Self::Locate3Native => &[
                EvalType::Bytes,
                EvalType::Bytes,
                EvalType::Int,
                EvalType::Int,
            ],
            Self::Substring2BytesNative | Self::Substring2Utf8Native => {
                &[EvalType::Bytes, EvalType::Int]
            }
            Self::Substring3BytesNative | Self::Substring3Utf8Native => {
                &[EvalType::Bytes, EvalType::Int, EvalType::Int]
            }
            Self::Substring2BytesLegacy | Self::Substring2Utf8Legacy => {
                &[EvalType::Bytes, EvalType::Bytes]
            }
            Self::Substring3BytesLegacy | Self::Substring3Utf8Legacy => {
                &[EvalType::Bytes, EvalType::Bytes, EvalType::Bytes]
            }
            Self::HexInt
            | Self::Bin
            | Self::BitCount
            | Self::BitNeg
            | Self::UnaryNot
            | Self::IsNull
            | Self::IsTrue
            | Self::IsFalse
            | Self::IsTrueWithNull
            | Self::IsNotNull
            | Self::IsNotTrue
            | Self::IsNotFalse
            | Self::InetNtoa => &[EvalType::Int],
            Self::BitAnd
            | Self::BitOr
            | Self::BitXor
            | Self::LeftShift
            | Self::RightShift
            | Self::LogicalAnd
            | Self::LogicalOr
            | Self::LogicalXor
            | Self::SpaceNative => &[EvalType::Int, EvalType::Int],
            Self::Left
            | Self::LeftUtf8
            | Self::Right
            | Self::RightUtf8
            | Self::ToBase64Native
            | Self::FromBase64Native
            | Self::Sha2Native => &[EvalType::Bytes, EvalType::Int],
            Self::RepeatNative => &[EvalType::Bytes, EvalType::Int, EvalType::Int],
            Self::TrimBothNative
            | Self::TrimLeadingNative
            | Self::TrimTrailingNative
            | Self::LogNative
            | Self::PowNative => &[EvalType::Bytes, EvalType::Bytes],
            Self::Insert | Self::InsertUtf8Native => &[
                EvalType::Bytes,
                EvalType::Int,
                EvalType::Int,
                EvalType::Bytes,
            ],
            Self::SubstringIndexSignedNative | Self::SubstringIndexUnsignedNative => {
                &[EvalType::Bytes, EvalType::Bytes, EvalType::Int]
            }
            Self::LpadBytesNative
            | Self::RpadBytesNative
            | Self::LpadUtf8Native
            | Self::RpadUtf8Native => &[
                EvalType::Bytes,
                EvalType::Int,
                EvalType::Bytes,
                EvalType::Int,
            ],
            Self::Replace => &[EvalType::Bytes, EvalType::Bytes, EvalType::Bytes],
            Self::Ascii
            | Self::Length
            | Self::BitLength
            | Self::LTrim
            | Self::RTrim
            | Self::UnHex
            | Self::Crc32
            | Self::Reverse
            | Self::ReverseUtf8
            | Self::CharLength
            | Self::CharLengthUtf8
            | Self::Quote
            | Self::HexStr
            | Self::Md5
            | Self::Sha1
            | Self::InetAton
            | Self::Inet6Aton
            | Self::Inet6Ntoa
            | Self::AsinRaw
            | Self::AcosRaw
            | Self::SqrtRaw
            | Self::SignRaw
            | Self::RadiansRaw
            | Self::DegreesRaw
            | Self::IsIpv4Nullable
            | Self::IsIpv6Nullable
            | Self::IsIpv4CompatNullable
            | Self::IsIpv4MappedNullable
            | Self::FromBase64ValueNative
            | Self::Lower
            | Self::Upper
            | Self::LowerUtf8Ready
            | Self::UpperUtf8Ready
            | Self::OrdNative
            | Self::LnNative
            | Self::Log2Native
            | Self::UncompressedLengthNative
            | Self::LowerAsciiNative
            | Self::UpperAsciiNative => &[EvalType::Bytes],
        }
    }

    /// The only composite recipes are NOT(IS NULL/TRUE/FALSE(input)). These
    /// stages are fixed identities, not a caller-supplied list or expression.
    pub(crate) fn call_count(self) -> usize {
        match self {
            Self::IsNotNull | Self::IsNotTrue | Self::IsNotFalse => 2,
            _ => 1,
        }
    }

    /// Primitive kernel identities in postfix order. function_ref()/fn_meta()
    /// on a composite denote its root; each stage must instead use this
    /// selector.
    pub(crate) fn call_operation(self, index: usize) -> Option<Self> {
        match (self, index) {
            (Self::IsNotNull, 0) => Some(Self::IsNull),
            (Self::IsNotTrue, 0) => Some(Self::IsTrue),
            (Self::IsNotFalse, 0) => Some(Self::IsFalse),
            (Self::IsNotNull | Self::IsNotTrue | Self::IsNotFalse, 1) => Some(Self::UnaryNot),
            (operation, 0) => Some(operation),
            _ => None,
        }
    }

    pub(crate) fn input_field_type(self, slot: usize) -> Option<tipb::FieldType> {
        self.input_types().get(slot).map(|kind| match kind {
            EvalType::Int => evaluated_ascii_int_type(),
            EvalType::Bytes => evaluated_ascii_bytes_type(),
            EvalType::Decimal => evaluated_ascii_decimal_type(),
            EvalType::VectorFloat32 => evaluated_native_vector_type(),
            _ => unreachable!("the operation has only Int/Bytes/Decimal/VectorFloat32 inputs"),
        })
    }

    fn entry(self) -> ProgramEntry {
        if self == Self::Ascii {
            ProgramEntry::EvaluatedAscii
        } else {
            ProgramEntry::EvaluatedBytes
        }
    }
}

/// A native frontend's packet-policy decision, separate from SQL NULL inputs.
/// The frontend owns warning 1301 (or its error); the selected private wrapper
/// owns the suppressed NULL result after a real generated-wrapper dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputDisposition {
    Allow,
    SuppressByPacket,
}

impl OutputDisposition {
    fn flag(self) -> i64 {
        match self {
            Self::Allow => 0,
            Self::SuppressByPacket => 1,
        }
    }
}

/// Integer-argument demand is independent of SQL nullability. The facade
/// checks each recipe's undemanded-input condition before supplying an
/// irrelevant representative; Undemanded never claims an evaluated SQL NULL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadyIntArg {
    Value(Option<i64>),
    Undemanded,
}

/// Legacy CONV retains the complete folded integer domain. A base outside
/// i64 is a non-NULL value; only that validated from-base exit, or an earlier
/// actual NULL, authorizes skipping a later base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadyConvBaseArg {
    Value(Option<i128>),
    Undemanded,
}

/// A Decimal demand marker. Undemanded requires an actual NULL scale;
/// it never substitutes for an evaluated SQL NULL numeric operand.
#[derive(Debug)]
pub enum ReadyDecimalArg {
    Value(Option<Decimal>),
    Undemanded,
}

/// Preserve a native Decimal bridge's concrete cause without classifying it
/// as a SQL overflow or erasing it through the legacy boxed-error conversion.
pub fn native_decimal_bridge_error(error: NativeDecimalError) -> LocalError {
    LocalError::Evaluation(EvaluateError::Caused(Box::new(error)).into())
}

/// PAD's two string arguments are either both evaluated values or both
/// explicitly undemanded. The latter requires an earlier length/packet exit;
/// it is not a claim that either original argument was SQL NULL. Native regexp
/// INSTR also permits undemanded flags, only after an invalid return_option.
#[derive(Debug)]
pub enum ReadyBytesArg {
    Value(Option<Vec<u8>>),
    Undemanded,
}

/// Raw floating-point demand is separate from SQL NULL. Only PowNative may
/// have one undemanded operand, and only when the other is Value(None).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadyIeee754Arg {
    Value(Option<u64>),
    Undemanded,
}

/// Only legacy SUBSTRING transports this full integer domain. An i128 which
/// cannot become i64 remains a non-NULL input; its result policy is in the
/// kernel. Undemanded is admitted only by that recipe's demand conditions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadySubstringI128 {
    Value(Option<i128>),
    Undemanded,
}

/// Search units are explicit: a binary collation can still compare UTF8
/// character windows (POSITION), independently of byte-oriented LOCATE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeSearchPolicy {
    Bytes,
    Utf8(NativeCollation),
}

impl NativeSearchPolicy {
    pub(crate) fn tag(self) -> i64 {
        match self {
            Self::Bytes => 0,
            Self::Utf8(collation) => 1 + collation.tag(),
        }
    }

    pub(crate) fn from_tag(tag: i64) -> Option<Self> {
        if tag == 0 {
            Some(Self::Bytes)
        } else {
            tag.checked_sub(1)
                .and_then(NativeCollation::from_tag)
                .map(Self::Utf8)
        }
    }
}

/// Packs the actual grouping id and immutable ascending mark sets. This only
/// builds the existing two-column carrier: mode selection and result evaluation
/// remain outside this helper. Both owners and the complete envelope extent use
/// fallible allocation, with no serialized opcode or precomputed result bits.
pub fn prepare_grouping_args(
    gid: u64,
    metadata: &crate::GroupingMetadata,
) -> LocalResult<EvaluatedArgs> {
    let groups = metadata.grouping_marks();
    let group_count =
        u64::try_from(groups.len()).map_err(|_| evaluated_ascii_storage_overflow())?;
    let words = groups.iter().try_fold(1usize, |words, marks| {
        words
            .checked_add(1)
            .and_then(|words| words.checked_add(marks.len()))
            .ok_or_else(evaluated_ascii_storage_overflow)
    })?;
    let bytes = words
        .checked_mul(8)
        .ok_or_else(evaluated_ascii_storage_overflow)?;
    let mut gid_bytes = Vec::new();
    gid_bytes
        .try_reserve_exact(8)
        .map_err(|_| LocalError::ResourceLimit("GROUPING id input allocation failed".into()))?;
    gid_bytes.extend_from_slice(&gid.to_le_bytes());
    let mut marks_bytes = Vec::new();
    marks_bytes
        .try_reserve_exact(bytes)
        .map_err(|_| LocalError::ResourceLimit("GROUPING marks input allocation failed".into()))?;
    marks_bytes.extend_from_slice(&group_count.to_le_bytes());
    for marks in groups {
        let count = u64::try_from(marks.len()).map_err(|_| evaluated_ascii_storage_overflow())?;
        marks_bytes.extend_from_slice(&count.to_le_bytes());
        for mark in marks {
            marks_bytes.extend_from_slice(&mark.to_le_bytes());
        }
    }
    Ok(EvaluatedArgs::Bytes2(Some(gid_bytes), Some(marks_bytes)))
}

/// Serializes actual frontend-prepared JSON values and preserves original path
/// text. This only changes representation; SQL parsing, path policy and the
/// predicate result are not computed here. Serializer allocation retains the
/// existing serde policy, while copying the path is explicitly fallible.
pub fn prepare_json_serde_args(
    first: &serde_json::Value,
    second: Option<&serde_json::Value>,
    path: Option<&str>,
) -> LocalResult<EvaluatedArgs> {
    let serialize = |value: &serde_json::Value| {
        serde_json::to_vec(value).map_err(|error| {
            LocalError::Evaluation(other_err!("JSON ready serialization failed: {}", error))
        })
    };
    let first = serialize(first)?;
    let second = second.map(serialize).transpose()?;
    let path = path
        .map(|path| -> LocalResult<Vec<u8>> {
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(path.len()).map_err(|_| {
                LocalError::ResourceLimit("JSON path input allocation failed".into())
            })?;
            bytes.extend_from_slice(path.as_bytes());
            Ok(bytes)
        })
        .transpose()?;
    Ok(match (second, path) {
        (None, None) => EvaluatedArgs::Bytes(Some(first)),
        (Some(second), None) => EvaluatedArgs::Bytes2(Some(first), Some(second)),
        (None, Some(path)) => EvaluatedArgs::Bytes2(Some(first), Some(path)),
        (Some(second), Some(path)) => {
            EvaluatedArgs::Bytes3([Some(first), Some(second), Some(path)])
        }
    })
}

/// Packs two original binary JSON type/payload pairs without decoding,
/// canonicalization, predicate evaluation, or choosing a worker operation.
pub fn prepare_json_binary_pair_args(
    first_type: u8,
    first_raw: &[u8],
    second_type: u8,
    second_raw: &[u8],
) -> LocalResult<EvaluatedArgs> {
    let first_len = first_raw
        .len()
        .checked_add(1)
        .ok_or_else(evaluated_ascii_storage_overflow)?;
    let second_len = second_raw
        .len()
        .checked_add(1)
        .ok_or_else(evaluated_ascii_storage_overflow)?;
    first_len
        .checked_add(second_len)
        .ok_or_else(evaluated_ascii_storage_overflow)?;
    let packet = |type_code: u8, raw: &[u8], extent: usize| -> LocalResult<Vec<u8>> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(extent)
            .map_err(|_| LocalError::ResourceLimit("binary JSON input allocation failed".into()))?;
        bytes.push(type_code);
        bytes.extend_from_slice(raw);
        Ok(bytes)
    };
    Ok(EvaluatedArgs::Bytes2(
        Some(packet(first_type, first_raw, first_len)?),
        Some(packet(second_type, second_raw, second_len)?),
    ))
}

fn json_operand_list_buffer(count: usize) -> LocalResult<Vec<u8>> {
    let count = u64::try_from(count).map_err(|_| evaluated_ascii_storage_overflow())?;
    let mut encoded = Vec::new();
    encoded.try_reserve_exact(8).map_err(|_| {
        LocalError::ResourceLimit("JSON operand list header allocation failed".into())
    })?;
    encoded.extend_from_slice(&count.to_le_bytes());
    Ok(encoded)
}

fn push_json_operand_bytes(encoded: &mut Vec<u8>, value: &[u8]) -> LocalResult<()> {
    let length = u64::try_from(value.len()).map_err(|_| evaluated_ascii_storage_overflow())?;
    let additional = value
        .len()
        .checked_add(8)
        .ok_or_else(evaluated_ascii_storage_overflow)?;
    encoded
        .len()
        .checked_add(additional)
        .ok_or_else(evaluated_ascii_storage_overflow)?;
    // Amortized growth avoids re-allocating the complete prefix for every value.
    encoded
        .try_reserve(additional)
        .map_err(|_| LocalError::ResourceLimit("JSON operand list allocation failed".into()))?;
    encoded.extend_from_slice(&length.to_le_bytes());
    encoded.extend_from_slice(value);
    Ok(())
}

/// Packs each actual ready JSON argument independently, including JSON null.
/// Count zero is the real empty operand list, never a preconstructed `[]`
/// result.
pub fn prepare_json_array_args(values: &[serde_json::Value]) -> LocalResult<EvaluatedArgs> {
    let mut encoded = json_operand_list_buffer(values.len())?;
    for value in values {
        let value = serde_json::to_vec(value).map_err(|error| {
            LocalError::Evaluation(other_err!("JSON ready serialization failed: {}", error))
        })?;
        push_json_operand_bytes(&mut encoded, &value)?;
    }
    Ok(EvaluatedArgs::Bytes(Some(encoded)))
}

/// Packs ordered actual key/value pairs without constructing a JSON object.
/// Duplicate keys remain separate records so only the worker applies overwrite
/// semantics. Keys are already coerced non-NULL strings; zero pairs is valid.
pub fn prepare_json_object_args(
    pairs: &[(String, serde_json::Value)],
) -> LocalResult<EvaluatedArgs> {
    let mut encoded = json_operand_list_buffer(pairs.len())?;
    for (key, value) in pairs {
        push_json_operand_bytes(&mut encoded, key.as_bytes())?;
        let value = serde_json::to_vec(value).map_err(|error| {
            LocalError::Evaluation(other_err!("JSON ready serialization failed: {}", error))
        })?;
        push_json_operand_bytes(&mut encoded, &value)?;
    }
    Ok(EvaluatedArgs::Bytes(Some(encoded)))
}

fn encode_json_paths(paths: &[crate::NativeJsonPath]) -> LocalResult<Vec<u8>> {
    use crate::{NativeJsonArraySelection as Array, NativeJsonPathLeg as Leg};
    let count = u64::try_from(paths.len()).map_err(|_| evaluated_ascii_storage_overflow())?;
    let mut extent = 8usize;
    for path in paths {
        u64::try_from(path.legs.len()).map_err(|_| evaluated_ascii_storage_overflow())?;
        extent = extent
            .checked_add(9)
            .ok_or_else(evaluated_ascii_storage_overflow)?;
        for leg in &path.legs {
            let additional = match leg {
                Leg::Key(key) => {
                    u64::try_from(key.len()).map_err(|_| evaluated_ascii_storage_overflow())?;
                    key.len()
                        .checked_add(9)
                        .ok_or_else(evaluated_ascii_storage_overflow)?
                }
                Leg::Array(Array::Index(_)) => 9,
                Leg::Array(Array::Range(..)) => 17,
                Leg::KeyWildcard | Leg::Array(Array::All) | Leg::Recursive => 1,
            };
            extent = extent
                .checked_add(additional)
                .ok_or_else(evaluated_ascii_storage_overflow)?;
        }
    }
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(extent)
        .map_err(|_| LocalError::ResourceLimit("JSON path list allocation failed".into()))?;
    encoded.extend_from_slice(&count.to_le_bytes());
    for path in paths {
        // Preserve the actual cached flag, including its distinction from the
        // selector legs. Never reconstruct text or infer it from wildcard tags.
        encoded.push(u8::from(path.could_match_multiple));
        let count =
            u64::try_from(path.legs.len()).map_err(|_| evaluated_ascii_storage_overflow())?;
        encoded.extend_from_slice(&count.to_le_bytes());
        for leg in &path.legs {
            match leg {
                Leg::Key(key) => {
                    encoded.push(0);
                    let length =
                        u64::try_from(key.len()).map_err(|_| evaluated_ascii_storage_overflow())?;
                    encoded.extend_from_slice(&length.to_le_bytes());
                    encoded.extend_from_slice(key.as_bytes());
                }
                Leg::KeyWildcard => encoded.push(1),
                Leg::Array(Array::All) => encoded.push(2),
                Leg::Array(Array::Index(index)) => {
                    encoded.push(3);
                    encoded.extend_from_slice(&index.to_le_bytes());
                }
                Leg::Array(Array::Range(start, end)) => {
                    encoded.push(4);
                    encoded.extend_from_slice(&start.to_le_bytes());
                    encoded.extend_from_slice(&end.to_le_bytes());
                }
                Leg::Recursive => encoded.push(5),
            }
        }
    }
    Ok(encoded)
}

/// Packs the actual document and borrowed selector ASTs from the existing path
/// cache. Zero paths is real data; this neither changes SQL arity admission nor
/// reparses selectors, chooses an operation, or evaluates a path.
pub fn prepare_json_paths_args(
    document: &serde_json::Value,
    paths: &[crate::NativeJsonPath],
) -> LocalResult<EvaluatedArgs> {
    let document = serde_json::to_vec(document).map_err(|error| {
        LocalError::Evaluation(other_err!("JSON ready serialization failed: {}", error))
    })?;
    Ok(EvaluatedArgs::Bytes2(
        Some(document),
        Some(encode_json_paths(paths)?),
    ))
}

/// Packs actual JSON_SEARCH operands without traversing the document or paths.
pub fn prepare_json_search_args(
    document: &serde_json::Value,
    paths: &[crate::NativeJsonPath],
    one: bool,
    pattern: &str,
    escape: char,
) -> LocalResult<EvaluatedArgs> {
    let EvaluatedArgs::Bytes2(Some(document), Some(paths)) =
        prepare_json_paths_args(document, paths)?
    else {
        return Err(LocalError::InvalidSpec(
            "JSON path operand packing changed its fixed shape".into(),
        ));
    };
    let spec = crate::native_json_search::encode_native_json_search_spec(one, pattern, escape)
        .ok_or_else(|| {
            LocalError::ResourceLimit(
                "JSON search specification allocation or extent failed".into(),
            )
        })?;
    Ok(EvaluatedArgs::Bytes3([
        Some(document),
        Some(paths),
        Some(spec),
    ]))
}

/// Adds actual ordered value operands using the existing ARRAY operand framing,
/// not an ARRAY kernel or a constructed array result. Frontends preserve their
/// original zip policy by supplying the effective matching prefix here.
pub fn prepare_json_path_values_args(
    document: &serde_json::Value,
    paths: &[crate::NativeJsonPath],
    values: &[serde_json::Value],
) -> LocalResult<EvaluatedArgs> {
    if paths.len() != values.len() {
        return Err(LocalError::InvalidBatch(
            "JSON path/value operand counts differ".into(),
        ));
    }
    match (
        prepare_json_paths_args(document, paths)?,
        prepare_json_array_args(values)?,
    ) {
        (
            EvaluatedArgs::Bytes2(Some(document), Some(paths)),
            EvaluatedArgs::Bytes(Some(values)),
        ) => Ok(EvaluatedArgs::Bytes3([
            Some(document),
            Some(paths),
            Some(values),
        ])),
        _ => Err(LocalError::InvalidSpec(
            "JSON operand packing changed its fixed shape".into(),
        )),
    }
}

fn reserve_json_raw_operand(encoded: &mut Vec<u8>, additional: usize) -> LocalResult<()> {
    encoded
        .len()
        .checked_add(additional)
        .ok_or_else(evaluated_ascii_storage_overflow)?;
    encoded
        .try_reserve(additional)
        .map_err(|_| LocalError::ResourceLimit("raw JSON operand packet allocation failed".into()))
}

fn encode_json_raw_scalar((kind, raw): (u8, &[u8])) -> LocalResult<Vec<u8>> {
    let extent = raw
        .len()
        .checked_add(1)
        .ok_or_else(evaluated_ascii_storage_overflow)?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(extent)
        .map_err(|_| LocalError::ResourceLimit("raw JSON document allocation failed".into()))?;
    encoded.push(kind);
    encoded.extend_from_slice(raw);
    Ok(encoded)
}

/// Transports an actual legacy binary value without decoding, including an
/// unknown tag or empty/malformed payload. Absence has a separate NoArgs
/// recipe.
pub fn prepare_json_raw_identity_args(document: (u8, &[u8])) -> LocalResult<EvaluatedArgs> {
    Ok(EvaluatedArgs::Bytes(Some(encode_json_raw_scalar(
        document,
    )?)))
}

/// Packs only actual borrowed SDK legs, original multiple flags, and binary
/// operands. Counts are checked against actual iterator consumption; this does
/// not parse paths, derive flags, validate binary JSON, or select an operation.
pub fn prepare_json_raw_paths_values_args<'p, 'v>(
    document: (u8, &[u8]),
    paths: impl ExactSizeIterator<
        Item = (
            &'p [tidb_query_datatype::codec::mysql::json::NativeBinaryJsonPathLeg],
            bool,
        ),
    >,
    values: impl ExactSizeIterator<Item = (u8, &'v [u8])>,
) -> LocalResult<EvaluatedArgs> {
    use tidb_query_datatype::codec::mysql::json::{
        NativeBinaryJsonArraySelection as Array, NativeBinaryJsonPathLeg as Leg,
    };
    let count = paths.len();
    if count != values.len() {
        return Err(LocalError::InvalidBatch(
            "raw JSON path/value operand counts differ".into(),
        ));
    }
    let mut encoded_paths = json_operand_list_buffer(count)?;
    let mut observed = 0usize;
    for (legs, multiple) in paths {
        if observed == count {
            return Err(LocalError::InvalidBatch(
                "raw JSON path iterator exceeds its count".into(),
            ));
        }
        observed += 1;
        let leg_count =
            u64::try_from(legs.len()).map_err(|_| evaluated_ascii_storage_overflow())?;
        let mut extent = 9usize;
        for leg in legs {
            let additional = match leg {
                Leg::Key(key) => {
                    u64::try_from(key.len()).map_err(|_| evaluated_ascii_storage_overflow())?;
                    key.len()
                        .checked_add(9)
                        .ok_or_else(evaluated_ascii_storage_overflow)?
                }
                Leg::Array(Array::Index(_)) => 9,
                Leg::Array(Array::Range { .. }) => 17,
                Leg::Array(Array::Asterisk) | Leg::DoubleAsterisk => 1,
            };
            extent = extent
                .checked_add(additional)
                .ok_or_else(evaluated_ascii_storage_overflow)?;
        }
        reserve_json_raw_operand(&mut encoded_paths, extent)?;
        encoded_paths.push(u8::from(multiple));
        encoded_paths.extend_from_slice(&leg_count.to_le_bytes());
        for leg in legs {
            match leg {
                Leg::Key(key) => {
                    encoded_paths.push(0);
                    let length =
                        u64::try_from(key.len()).map_err(|_| evaluated_ascii_storage_overflow())?;
                    encoded_paths.extend_from_slice(&length.to_le_bytes());
                    encoded_paths.extend_from_slice(key.as_bytes());
                }
                Leg::Array(Array::Asterisk) => encoded_paths.push(1),
                Leg::Array(Array::Index(index)) => {
                    encoded_paths.push(2);
                    encoded_paths.extend_from_slice(&index.to_le_bytes());
                }
                Leg::Array(Array::Range { start, end }) => {
                    encoded_paths.push(3);
                    encoded_paths.extend_from_slice(&start.to_le_bytes());
                    encoded_paths.extend_from_slice(&end.to_le_bytes());
                }
                Leg::DoubleAsterisk => encoded_paths.push(4),
            }
        }
    }
    if observed != count {
        return Err(LocalError::InvalidBatch(
            "raw JSON path iterator ended before its count".into(),
        ));
    }
    let mut encoded_values = json_operand_list_buffer(count)?;
    observed = 0;
    for (kind, raw) in values {
        if observed == count {
            return Err(LocalError::InvalidBatch(
                "raw JSON value iterator exceeds its count".into(),
            ));
        }
        observed += 1;
        let length = raw
            .len()
            .checked_add(1)
            .ok_or_else(evaluated_ascii_storage_overflow)?;
        let word = u64::try_from(length).map_err(|_| evaluated_ascii_storage_overflow())?;
        let additional = length
            .checked_add(8)
            .ok_or_else(evaluated_ascii_storage_overflow)?;
        reserve_json_raw_operand(&mut encoded_values, additional)?;
        encoded_values.extend_from_slice(&word.to_le_bytes());
        encoded_values.push(kind);
        encoded_values.extend_from_slice(raw);
    }
    if observed != count {
        return Err(LocalError::InvalidBatch(
            "raw JSON value iterator ended before its count".into(),
        ));
    }
    Ok(EvaluatedArgs::Bytes3([
        Some(encode_json_raw_scalar(document)?),
        Some(encoded_paths),
        Some(encoded_values),
    ]))
}

/// Packs actual nullable JSON operands in order. The presence byte
/// distinguishes SQL NULL from Some(JSON null); no merge or NULL-revival policy
/// runs here.
pub fn prepare_json_nullable_values_args(
    values: &[Option<serde_json::Value>],
) -> LocalResult<EvaluatedArgs> {
    let mut encoded = json_operand_list_buffer(values.len())?;
    for value in values {
        reserve_json_raw_operand(&mut encoded, 1)?;
        encoded.push(u8::from(value.is_some()));
        if let Some(value) = value {
            let value = serde_json::to_vec(value).map_err(|error| {
                LocalError::Evaluation(other_err!("JSON ready serialization failed: {}", error))
            })?;
            push_json_operand_bytes(&mut encoded, &value)?;
        }
    }
    Ok(EvaluatedArgs::Bytes(Some(encoded)))
}

/// Packs ordered actual raw scalars, preserving every type and payload byte.
/// Empty lists are real data, and semantic raw decoding belongs to the worker.
pub fn prepare_json_raw_values_args(values: &[(u8, &[u8])]) -> LocalResult<EvaluatedArgs> {
    let mut encoded = json_operand_list_buffer(values.len())?;
    for &value in values {
        let scalar = encode_json_raw_scalar(value)?;
        push_json_operand_bytes(&mut encoded, &scalar)?;
    }
    Ok(EvaluatedArgs::Bytes(Some(encoded)))
}

/// Owned ready arguments and explicit demand markers for closed recipes. Int
/// carries the original 64-bit pattern: callers may pass a u64 as i64 without
/// numeric narrowing. Coercion, diagnostics, argument demand and text
/// normalization belong to the original frontend. For logical AND(false, _)
/// or OR(true, _), only its validated undemanded-RHS marker may authorize an
/// irrelevant representative; this boundary does not claim RHS evaluated to
/// NULL.
#[derive(Debug)]
pub enum EvaluatedArgs {
    TemporalText {
        value: Vec<u8>,
        modes: i64,
        zone: NativeSessionTimeZone,
    },
    /// Actual value operands plus an invocation-bound precision increment.
    /// NULL/missing uses the existing terminal recipes, not this carrier.
    DecimalDivision {
        left: Option<Decimal>,
        right: Option<Decimal>,
        frac_increment: u32,
    },
    /// Two actual Decimal owners; the facade derives their finite shared
    /// budget.
    Decimal2 {
        left: Option<Decimal>,
        right: Option<Decimal>,
    },
    /// Actual signed i128 operands, not narrowed native integer values.
    Int1282(Option<i128>, Option<i128>),
    /// Three actual, non-NULL operands with invocation-scoped semantic context.
    /// NULL short-circuit uses NullWitness instead; escape is normalized to u8.
    Like {
        invocation: NativeLikeInvocation,
        text: Option<Bytes>,
        pattern: Option<Bytes>,
        escape: Option<i64>,
    },
    RegexpLike {
        invocation: NativeRegexpInvocation,
        text: Vec<u8>,
        pattern: Vec<u8>,
        match_type: Vec<u8>,
    },
    RegexpSubstr {
        invocation: NativeRegexpInvocation,
        text: Vec<u8>,
        pattern: Vec<u8>,
        pos: i64,
        occurrence: i64,
        match_type: Vec<u8>,
    },
    RegexpInstr {
        invocation: NativeRegexpInvocation,
        text: Vec<u8>,
        pattern: Vec<u8>,
        pos: i64,
        occurrence: i64,
        return_option: i64,
        match_type: ReadyBytesArg,
    },
    RegexpReplace {
        invocation: NativeRegexpInvocation,
        text: Vec<u8>,
        pattern: Vec<u8>,
        replacement: Vec<u8>,
        pos: i64,
        occurrence: i64,
        match_type: Vec<u8>,
    },
    /// Actual aligned vectors, never ordinary Bytes or serialized controls.
    NativeVector(Option<NativeVectorFloat32>),
    NativeVector2(Option<NativeVectorFloat32>, Option<NativeVectorFloat32>),
    /// A genuine zero-operand invocation, not a nullable dummy argument.
    NoArgs,
    Decimal(Option<Decimal>),
    DecimalIntReady {
        value: ReadyDecimalArg,
        scale: ReadyIntArg,
    },
    Ieee754BitsInt {
        value: Option<u64>,
        scale: Option<i64>,
    },
    /// Actually evaluated MAKETIME operands. The hour retains its integer bits
    /// and actual signedness; seconds retain IEEE754 bits, not a computed date.
    MakeTimeParts {
        hour: Option<(i64, bool)>,
        minute: Option<i64>,
        second: Option<u64>,
    },
    Int128(Option<i128>),
    /// Witness of some actually observed SQL NULL, not a claimed numeric value.
    NullWitness(Option<i64>),
    CharReady(PreparedCharArgs),
    ConvReady {
        number: ReadyBytesArg,
        from_base: ReadyIntArg,
        to_base: ReadyIntArg,
    },
    ConvLegacyReady {
        number: ReadyBytesArg,
        from_base: ReadyConvBaseArg,
        to_base: ReadyConvBaseArg,
    },
    Bytes(Option<Vec<u8>>),
    Bytes2(Option<Vec<u8>>, Option<Vec<u8>>),
    Int(Option<i64>),
    BytesInt(Option<Vec<u8>>, Option<i64>),
    BytesIntIntBytes(Option<Vec<u8>>, Option<i64>, Option<i64>, Option<Vec<u8>>),
    /// Ready values with an explicit integer-demand marker, not packet policy.
    BytesIntReady {
        bytes: Option<Vec<u8>>,
        count: ReadyIntArg,
    },
    /// SUBSTRING_INDEX checks a genuinely NULL count before empty delimiter.
    /// Only a non-NULL count may be undemanded due to an empty delimiter.
    BytesBytesIntReady {
        bytes: Option<Vec<u8>>,
        delimiter: Option<Vec<u8>>,
        count: ReadyIntArg,
    },
    Bytes3([Option<Vec<u8>>; 3]),
    Int2(Option<i64>, Option<i64>),
    /// Original ready value plus an explicit packet-policy decision.
    PacketInt {
        value: Option<i64>,
        disposition: OutputDisposition,
    },
    PacketBytes {
        value: Option<Vec<u8>>,
        disposition: OutputDisposition,
    },
    PacketBytesInt {
        bytes: Option<Vec<u8>>,
        count: ReadyIntArg,
        disposition: OutputDisposition,
    },
    PacketBytesIntBytes {
        bytes: ReadyBytesArg,
        count: Option<i64>,
        pad: ReadyBytesArg,
        disposition: OutputDisposition,
    },
    /// Nullable IEEE754 binary64 bits, not a SQL integer or ordinary Bytes.
    /// All bit patterns are admitted; only None represents an absent input.
    Ieee754Bits(Option<u64>),
    /// Raw calendar core-time data, not packed time, IEEE754, or ordinary
    /// Bytes. All bits are retained; no Datetime or precomputed field is
    /// constructed.
    TimeCoreBits(Option<u64>),
    /// Two actually evaluated raw core-time values, in independent nullable
    /// LE8 columns. No second operand, date field, or clock policy is inferred.
    TimeCoreBits2(Option<u64>, Option<u64>),
    /// A non-NULL raw core and an actually evaluated nullable layout. A NULL
    /// core uses the dedicated NULL-witness recipe instead of inventing layout.
    TimeCoreBitsBytes {
        core: u64,
        bytes: Option<Vec<u8>>,
    },
    Ieee754Bits2 {
        left: ReadyIeee754Arg,
        right: ReadyIeee754Arg,
    },
    Substring2Ready {
        bytes: ReadyBytesArg,
        pos: ReadyIntArg,
    },
    Substring3Ready {
        bytes: ReadyBytesArg,
        pos: ReadyIntArg,
        len: ReadyIntArg,
    },
    LegacySubstring2Ready {
        bytes: Option<Vec<u8>>,
        pos: ReadySubstringI128,
    },
    LegacySubstring3Ready {
        bytes: Option<Vec<u8>>,
        pos: ReadySubstringI128,
        len: ReadySubstringI128,
    },
    CollatedBytes2 {
        left: Option<Vec<u8>>,
        right: Option<Vec<u8>>,
        collation: NativeCollation,
    },
    SearchBytes2 {
        needle: Option<Vec<u8>>,
        haystack: Option<Vec<u8>>,
        policy: NativeSearchPolicy,
    },
    SearchBytes2IntReady {
        needle: Option<Vec<u8>>,
        haystack: Option<Vec<u8>>,
        pos: ReadyIntArg,
        policy: NativeSearchPolicy,
    },
    BytesBytesInt(Option<Vec<u8>>, Option<Vec<u8>>, Option<i64>),
    FindInSetPreparedReady {
        needle: ReadyBytesArg,
        keys: PreparedFindInSetKeys,
        collation: NativeCollation,
    },
    ConcatReady(PreparedConcatArgs),
    FieldReady(PreparedFieldArgs),
    MakeSetReady(PreparedMakeSetArgs),
    ExportSetReady(PreparedExportSetArgs),
    EltReady {
        index: Option<i64>,
        total_sql_arity: usize,
        selected: ReadyBytesArg,
    },
}

impl EvaluatedArgs {
    fn role(&self) -> EvaluatedArgsRole {
        match self {
            Self::TemporalText { .. } => EvaluatedArgsRole::TemporalText,
            Self::Decimal2 { .. } => EvaluatedArgsRole::DecimalBinary,
            Self::DecimalDivision { .. } => EvaluatedArgsRole::DecimalDivision,
            Self::Int1282(..) => EvaluatedArgsRole::Int1282,
            Self::Decimal(_) => EvaluatedArgsRole::DecimalUnary,
            Self::DecimalIntReady { .. } => EvaluatedArgsRole::DecimalInt,
            Self::Ieee754BitsInt { .. } => EvaluatedArgsRole::Ieee754Int,
            Self::MakeTimeParts { .. } => EvaluatedArgsRole::MakeTimeParts,
            Self::Int128(_) => EvaluatedArgsRole::Int128,
            Self::NullWitness(_) => EvaluatedArgsRole::NullWitness,
            Self::CharReady(_) => EvaluatedArgsRole::CharReady,
            Self::ConvReady { .. } => EvaluatedArgsRole::ConvNative,
            Self::ConvLegacyReady { .. } => EvaluatedArgsRole::ConvLegacy,
            Self::ConcatReady(_) => EvaluatedArgsRole::ConcatPacked,
            Self::FieldReady(_) => EvaluatedArgsRole::FieldPacked,
            Self::MakeSetReady(_) => EvaluatedArgsRole::MakeSetPacked,
            Self::ExportSetReady(_) => EvaluatedArgsRole::ExportSetPacked,
            Self::EltReady { .. } => EvaluatedArgsRole::EltReady,
            Self::CollatedBytes2 { .. } => EvaluatedArgsRole::CollatedBytes2,
            Self::SearchBytes2 { .. } | Self::SearchBytes2IntReady { .. } => {
                EvaluatedArgsRole::NativeSearch
            }
            Self::FindInSetPreparedReady { .. } => EvaluatedArgsRole::FindInSetPrepared,
            Self::NoArgs => EvaluatedArgsRole::NoArgs,
            Self::Like { .. } => EvaluatedArgsRole::Like,
            Self::RegexpLike { .. } => EvaluatedArgsRole::NativeRegexpLike,
            Self::RegexpSubstr { .. } => EvaluatedArgsRole::NativeRegexpSubstr,
            Self::RegexpInstr { .. } => EvaluatedArgsRole::NativeRegexpInstr,
            Self::RegexpReplace { .. } => EvaluatedArgsRole::NativeRegexpReplace,
            Self::NativeVector(_) => EvaluatedArgsRole::NativeVector,
            Self::NativeVector2(..) => EvaluatedArgsRole::NativeVector2,
            Self::Ieee754Bits(_) => EvaluatedArgsRole::Ieee754Bits,
            Self::TimeCoreBits(_) => EvaluatedArgsRole::TimeCoreBits,
            Self::TimeCoreBits2(..) => EvaluatedArgsRole::TimeCoreBits2,
            Self::TimeCoreBitsBytes { .. } => EvaluatedArgsRole::TimeCoreBitsBytes,
            Self::Ieee754Bits2 { .. } => EvaluatedArgsRole::Ieee754Bits2,
            Self::BytesIntReady { .. } => EvaluatedArgsRole::ReadyBytesInt,
            Self::BytesBytesIntReady { .. } => EvaluatedArgsRole::ReadyBytesBytesInt,
            Self::PacketBytesIntBytes { .. } => EvaluatedArgsRole::PadPacket,
            Self::Substring2Ready { .. } | Self::Substring3Ready { .. } => {
                EvaluatedArgsRole::SubstringNative
            }
            Self::LegacySubstring2Ready { .. } | Self::LegacySubstring3Ready { .. } => {
                EvaluatedArgsRole::SubstringLegacy
            }
            Self::PacketInt { .. } | Self::PacketBytes { .. } | Self::PacketBytesInt { .. } => {
                EvaluatedArgsRole::Packet
            }
            _ => EvaluatedArgsRole::Values,
        }
    }

    fn input_types(&self) -> &'static [EvalType] {
        match self {
            Self::TemporalText { .. } => &[EvalType::Bytes, EvalType::Int],
            Self::Like { .. } => EvaluatedBytesOp::LikeNative.input_types(),
            Self::RegexpLike { .. } => EvaluatedBytesOp::RegexpLikeNative.input_types(),
            Self::RegexpSubstr { .. } => EvaluatedBytesOp::RegexpSubstrNative.input_types(),
            Self::RegexpInstr { .. } => EvaluatedBytesOp::RegexpInstrNative.input_types(),
            Self::RegexpReplace { .. } => EvaluatedBytesOp::RegexpReplaceNative.input_types(),
            Self::NativeVector(_) => &[EvalType::VectorFloat32],
            Self::NativeVector2(..) => &[EvalType::VectorFloat32, EvalType::VectorFloat32],
            Self::NoArgs => &[],
            Self::Decimal2 { .. } | Self::DecimalDivision { .. } => {
                &[EvalType::Decimal, EvalType::Decimal, EvalType::Int]
            }
            Self::Int1282(..) => &[EvalType::Bytes, EvalType::Bytes],
            Self::Decimal(_) => &[EvalType::Decimal, EvalType::Int],
            Self::DecimalIntReady { .. } => &[EvalType::Decimal, EvalType::Int, EvalType::Int],
            Self::Ieee754BitsInt { .. } => &[EvalType::Bytes, EvalType::Int],
            Self::MakeTimeParts { .. } => &[EvalType::Bytes, EvalType::Int, EvalType::Bytes],
            Self::Int128(_) => &[EvalType::Bytes],
            Self::NullWitness(_) => &[EvalType::Int],
            Self::CharReady(_) => &[EvalType::Bytes],
            Self::ConvReady { .. } => &[EvalType::Bytes, EvalType::Int, EvalType::Int],
            Self::ConvLegacyReady { .. } => &[EvalType::Bytes, EvalType::Bytes, EvalType::Bytes],
            Self::ConcatReady(_) | Self::FieldReady(_) | Self::MakeSetReady(_) => {
                &[EvalType::Bytes]
            }
            Self::ExportSetReady(_) => &[EvalType::Bytes, EvalType::Int, EvalType::Int],
            Self::EltReady { .. } => &[EvalType::Int, EvalType::Int, EvalType::Bytes],
            Self::CollatedBytes2 { .. }
            | Self::SearchBytes2 { .. }
            | Self::FindInSetPreparedReady { .. }
            | Self::BytesBytesInt(..) => &[EvalType::Bytes, EvalType::Bytes, EvalType::Int],
            Self::SearchBytes2IntReady { .. } => &[
                EvalType::Bytes,
                EvalType::Bytes,
                EvalType::Int,
                EvalType::Int,
            ],
            Self::Bytes(_) | Self::Ieee754Bits(_) | Self::TimeCoreBits(_) => &[EvalType::Bytes],
            Self::Bytes2(..)
            | Self::Ieee754Bits2 { .. }
            | Self::TimeCoreBits2(..)
            | Self::TimeCoreBitsBytes { .. } => &[EvalType::Bytes, EvalType::Bytes],
            Self::BytesIntIntBytes(..) => &[
                EvalType::Bytes,
                EvalType::Int,
                EvalType::Int,
                EvalType::Bytes,
            ],
            Self::BytesBytesIntReady { .. } => &[EvalType::Bytes, EvalType::Bytes, EvalType::Int],
            Self::Substring2Ready { .. } => &[EvalType::Bytes, EvalType::Int],
            Self::Substring3Ready { .. } => &[EvalType::Bytes, EvalType::Int, EvalType::Int],
            Self::LegacySubstring2Ready { .. } => &[EvalType::Bytes, EvalType::Bytes],
            Self::LegacySubstring3Ready { .. } => {
                &[EvalType::Bytes, EvalType::Bytes, EvalType::Bytes]
            }
            Self::PacketBytesIntBytes { .. } => &[
                EvalType::Bytes,
                EvalType::Int,
                EvalType::Bytes,
                EvalType::Int,
            ],
            Self::Int(_) => &[EvalType::Int],
            Self::BytesInt(..) | Self::BytesIntReady { .. } | Self::PacketBytes { .. } => {
                &[EvalType::Bytes, EvalType::Int]
            }
            Self::Bytes3(_) => &[EvalType::Bytes, EvalType::Bytes, EvalType::Bytes],
            Self::Int2(..) | Self::PacketInt { .. } => &[EvalType::Int, EvalType::Int],
            Self::PacketBytesInt { .. } => &[EvalType::Bytes, EvalType::Int, EvalType::Int],
        }
    }

    fn admission_matches(&self, operation: EvaluatedBytesOp) -> bool {
        if operation == EvaluatedBytesOp::ConvertTzNative {
            return match self {
                Self::Bytes3([datetime, from, to]) => crate::convert_tz_native_args_valid(
                    datetime.as_deref(),
                    from.as_deref(),
                    to.as_deref(),
                ),
                _ => false,
            };
        }
        if operation.is_temporal_literal() {
            return match self {
                Self::TemporalText { value, modes, .. } => {
                    crate::temporal_literal_native_args_valid(Some(value), Some(*modes))
                }
                _ => false,
            };
        }
        if operation == EvaluatedBytesOp::TimestampAddNative {
            return match self {
                Self::BytesBytesInt(unit, date, amount) => crate::native_timestamp_add_args_valid(
                    unit.as_deref(),
                    date.as_deref(),
                    *amount,
                ),
                _ => false,
            };
        }
        if operation == EvaluatedBytesOp::TimestampAddPrefixNullNative {
            return match self {
                Self::BytesInt(unit, amount) => {
                    crate::native_timestamp_add_prefix_null_args_valid(unit.as_deref(), *amount)
                }
                _ => false,
            };
        }
        if matches!(
            operation,
            EvaluatedBytesOp::AddTimeNative | EvaluatedBytesOp::SubTimeNative
        ) {
            return match self {
                Self::BytesBytesInt(left, right, metadata) => {
                    crate::native_time_add_args_valid(left.as_deref(), right.as_deref(), *metadata)
                }
                _ => false,
            };
        }
        if operation == EvaluatedBytesOp::TimeAddRightDatetimeNative {
            return matches!(self, Self::Int(metadata) if crate::native_time_add_null_metadata_valid(*metadata));
        }
        if matches!(
            operation,
            EvaluatedBytesOp::TimeNative | EvaluatedBytesOp::MicrosecondNative
        ) {
            return matches!(self, Self::Bytes(value) if value.as_ref().is_none_or(|value| std::str::from_utf8(value).is_ok()));
        }
        if operation.is_native_decimal_int_div() {
            return match self {
                Self::BytesIntIntBytes(left, probe, fallback, right) => {
                    crate::native_intdiv_args_valid(
                        left.as_deref(),
                        *probe,
                        *fallback,
                        right.as_deref(),
                    )
                }
                _ => false,
            };
        }
        if operation == EvaluatedBytesOp::IntDivDecimalLegacy {
            return match self {
                Self::Bytes2(left, right) => {
                    crate::native_intdiv_legacy_args_valid(left.as_deref(), right.as_deref())
                }
                _ => false,
            };
        }
        if operation == EvaluatedBytesOp::TidbParseTsoNative {
            return match self {
                Self::Int2(tso, offset) => crate::native_tso_args_valid(*tso, *offset),
                _ => false,
            };
        }
        if operation == EvaluatedBytesOp::TimeDiffTextNative {
            return match self {
                Self::Bytes2(left, right) => {
                    crate::native_time_diff_args_valid(left.as_deref(), right.as_deref())
                }
                _ => false,
            };
        }
        if matches!(
            operation,
            EvaluatedBytesOp::AnyValueNative | EvaluatedBytesOp::NameConstNative
        ) {
            return match self {
                Self::Bytes(value) => crate::native_identity_args_valid(value.as_deref()),
                _ => false,
            };
        }
        if operation.is_weight_or_format_native() {
            return match self {
                Self::Bytes2(first, second) => {
                    operation.weight_or_format_args_valid(first.as_deref(), second.as_deref(), None)
                }
                Self::Int(kind) => operation.weight_or_format_args_valid(None, None, *kind),
                Self::BytesBytesInt(first, second, number) => operation
                    .weight_or_format_args_valid(first.as_deref(), second.as_deref(), *number),
                _ => false,
            };
        }
        if operation == EvaluatedBytesOp::DateCoreNative {
            return match self {
                Self::BytesInt(core, modes) => {
                    crate::impl_time::date_core_native_args_valid(core.as_deref(), *modes)
                }
                _ => false,
            };
        }
        if operation.is_clock_value() {
            return match self {
                Self::Bytes(clock) => operation.clock_args_valid(clock.as_deref(), None),
                Self::BytesInt(clock, fsp) => operation.clock_args_valid(clock.as_deref(), *fsp),
                _ => false,
            };
        }
        if operation.is_json_output_value() {
            return match self {
                Self::Bytes(first) => operation.json_output_args_valid(&[first.as_deref()]),
                Self::Bytes2(first, second) => {
                    operation.json_output_args_valid(&[first.as_deref(), second.as_deref()])
                }
                Self::Bytes3([first, second, third]) => operation.json_output_args_valid(&[
                    first.as_deref(),
                    second.as_deref(),
                    third.as_deref(),
                ]),
                _ => false,
            };
        }
        if operation.is_json_predicate_value() {
            return match self {
                Self::Bytes(first) => operation.json_predicate_args_valid(&[first.as_deref()]),
                Self::Bytes2(first, second) => {
                    operation.json_predicate_args_valid(&[first.as_deref(), second.as_deref()])
                }
                Self::Bytes3([first, second, third]) => operation.json_predicate_args_valid(&[
                    first.as_deref(),
                    second.as_deref(),
                    third.as_deref(),
                ]),
                _ => false,
            };
        }
        if let Some(mode) = operation.grouping_mode() {
            return matches!(self, Self::Bytes2(Some(gid), Some(marks))
                if crate::impl_miscellaneous::grouping_native_args_valid(gid, marks, mode));
        }
        if operation.comparison_op().is_some() {
            return matches!(
                self,
                Self::Int2(Some(_), Some(_))
                    | Self::Int1282(Some(_), Some(_))
                    | Self::Bytes2(Some(_), Some(_))
                    | Self::TimeCoreBits2(Some(_), Some(_))
                    | Self::NativeVector2(Some(_), Some(_))
                    | Self::Ieee754Bits2 {
                        left: ReadyIeee754Arg::Value(Some(_)),
                        right: ReadyIeee754Arg::Value(Some(_)),
                    }
                    | Self::Decimal2 {
                        left: Some(_),
                        right: Some(_)
                    }
                    | Self::CollatedBytes2 {
                        left: Some(_),
                        right: Some(_),
                        ..
                    }
            );
        }
        if operation.aes_profile().is_some() {
            return matches!(
                self,
                Self::Bytes2(Some(_), Some(_)) | Self::Bytes3([Some(_), Some(_), Some(_)])
            );
        }
        if operation.is_division_value() {
            return matches!(
                self,
                Self::DecimalDivision {
                    left: Some(_),
                    right: Some(_),
                    ..
                } | Self::Ieee754Bits2 {
                    left: ReadyIeee754Arg::Value(Some(_)),
                    right: ReadyIeee754Arg::Value(Some(_)),
                }
            );
        }
        if operation.is_integer_division_value() {
            return matches!(
                self,
                Self::Int2(Some(_), Some(_)) | Self::Int1282(Some(_), Some(_))
            );
        }
        if operation.is_modulo_value() {
            return match self {
                Self::Int2(Some(_), Some(_)) | Self::Int1282(Some(_), Some(_)) => true,
                Self::Ieee754Bits2 {
                    left: ReadyIeee754Arg::Value(Some(_)),
                    right: ReadyIeee754Arg::Value(Some(_)),
                } => true,
                Self::Decimal2 {
                    left: Some(_),
                    right: Some(_),
                } => true,
                _ => false,
            };
        }
        match self {
            Self::Like {
                invocation,
                text,
                pattern,
                escape,
            } => {
                operation.like_kind() == Some(invocation.kind())
                    && text.is_some()
                    && pattern.is_some()
                    && escape.is_some_and(|value| u8::try_from(value).is_ok())
            }
            Self::RegexpInstr {
                return_option,
                match_type,
                ..
            } => {
                operation == EvaluatedBytesOp::RegexpInstrNative
                    && match match_type {
                        ReadyBytesArg::Value(Some(_)) => true,
                        ReadyBytesArg::Undemanded => !matches!(return_option, 0 | 1),
                        ReadyBytesArg::Value(None) => false,
                    }
            }
            Self::BytesInt(bytes, flag) if operation == EvaluatedBytesOp::UuidToBinSwapNative => {
                // Only the parsed UUID stage has a nonnullable, exact-width input.
                // Other BytesInt operations retain their existing NULL policies.
                matches!((bytes, flag), (Some(bytes), Some(_)) if bytes.len() == 16)
            }
            Self::Ieee754BitsInt { value, scale }
                if operation == EvaluatedBytesOp::SecToTimeNative =>
            {
                // Only this recipe treats absent FSP as undemanded after a real
                // NULL seconds value, not as a claimed second SQL NULL.
                match (value, scale) {
                    (None, None) => true,
                    (Some(_), Some(scale)) => *scale >= 0 && usize::try_from(*scale).is_ok(),
                    _ => false,
                }
            }
            Self::NullWitness(value) => {
                matches!(
                    operation,
                    EvaluatedBytesOp::MathNullWitnessNative
                        | EvaluatedBytesOp::DateDiffNullNative
                        | EvaluatedBytesOp::WeekNullNative
                        | EvaluatedBytesOp::DateFormatNullNative
                        | EvaluatedBytesOp::TranslateNullNative
                        | EvaluatedBytesOp::SqlCryptNullNative
                        | EvaluatedBytesOp::AesNullNative
                        | EvaluatedBytesOp::VecRealNullNative
                        | EvaluatedBytesOp::LikeNullIntNative
                        | EvaluatedBytesOp::RegexpNullIntNative
                        | EvaluatedBytesOp::RegexpNullBytesNative
                        | EvaluatedBytesOp::UnaryNullNative
                        | EvaluatedBytesOp::BinaryArithmeticNullNative
                        | EvaluatedBytesOp::CompareNullNative
                        | EvaluatedBytesOp::GroupingNullNative
                        | EvaluatedBytesOp::JsonPredicateNullNative
                        | EvaluatedBytesOp::JsonOutputNullNative
                        | EvaluatedBytesOp::UtcTimeNullNative
                ) && value.is_none()
            }
            Self::ConvReady {
                number,
                from_base,
                to_base,
            } => {
                // Native CONV checks base NULLs before coercing its number.
                // A NULL number does not authorize skipping either base.
                let base_null = matches!(from_base, ReadyIntArg::Value(None))
                    || matches!(to_base, ReadyIntArg::Value(None));
                let demanded = base_null
                    || (matches!(number, ReadyBytesArg::Value(_))
                        && matches!(from_base, ReadyIntArg::Value(_))
                        && matches!(to_base, ReadyIntArg::Value(_)));
                let text_valid = operation != EvaluatedBytesOp::ConvNative
                    || match number {
                        ReadyBytesArg::Value(Some(bytes)) => std::str::from_utf8(bytes).is_ok(),
                        _ => true,
                    };
                demanded && text_valid
            }
            Self::ConvLegacyReady {
                number,
                from_base,
                to_base,
            } => {
                // Retain the actual demand prefix, not a fabricated NULL base.
                // Only representability is examined here, never the radix or
                // unchecked source arithmetic (including i64::MIN negation).
                match number {
                    ReadyBytesArg::Undemanded => false,
                    ReadyBytesArg::Value(None) => {
                        matches!(from_base, ReadyConvBaseArg::Undemanded)
                            && matches!(to_base, ReadyConvBaseArg::Undemanded)
                    }
                    ReadyBytesArg::Value(Some(_)) => match from_base {
                        ReadyConvBaseArg::Undemanded => false,
                        ReadyConvBaseArg::Value(None) => {
                            matches!(to_base, ReadyConvBaseArg::Undemanded)
                        }
                        ReadyConvBaseArg::Value(Some(from)) if i64::try_from(*from).is_err() => {
                            matches!(to_base, ReadyConvBaseArg::Undemanded)
                        }
                        ReadyConvBaseArg::Value(Some(_)) => {
                            matches!(to_base, ReadyConvBaseArg::Value(_))
                        }
                    },
                }
            }
            Self::DecimalIntReady { value, scale } => {
                (!matches!(value, ReadyDecimalArg::Undemanded)
                    || matches!(scale, ReadyIntArg::Value(None)))
                    && (!matches!(scale, ReadyIntArg::Undemanded)
                        || matches!(value, ReadyDecimalArg::Value(None)))
                    && match scale {
                        ReadyIntArg::Value(Some(scale)) => i32::try_from(*scale).is_ok(),
                        _ => true,
                    }
            }
            Self::Bytes(bytes) => operation.ready_bytes_match(bytes.as_deref()),
            Self::ConcatReady(args) => operation.concat_kind() == Some(args.kind()),
            Self::FieldReady(args) => operation.field_kind() == Some(args.kind()),
            Self::MakeSetReady(_) => operation == EvaluatedBytesOp::MakeSetNative,
            Self::ExportSetReady(_) => operation == EvaluatedBytesOp::ExportSetNative,
            Self::EltReady {
                index,
                total_sql_arity,
                selected,
            } => {
                operation == EvaluatedBytesOp::EltNative
                    && *total_sql_arity >= 2
                    && if crate::impl_string::elt_selected_arg(*index, *total_sql_arity).is_some() {
                        matches!(selected, ReadyBytesArg::Value(_))
                    } else {
                        matches!(selected, ReadyBytesArg::Undemanded)
                    }
            }
            Self::SearchBytes2IntReady {
                needle,
                haystack,
                pos,
                ..
            } => {
                operation.is_locate3_native()
                    && (!matches!(pos, ReadyIntArg::Undemanded)
                        || needle.is_none()
                        || haystack.is_none())
            }
            Self::FindInSetPreparedReady { needle, keys, .. } => {
                operation == EvaluatedBytesOp::FindInSetPreparedNative
                    && (!matches!(needle, ReadyBytesArg::Undemanded) || keys.is_null())
            }
            Self::Substring2Ready { bytes, pos } => {
                operation.is_substring_native()
                    && ((matches!(bytes, ReadyBytesArg::Value(_))
                        && matches!(pos, ReadyIntArg::Value(_)))
                        || matches!(bytes, ReadyBytesArg::Value(None))
                        || matches!(pos, ReadyIntArg::Value(None)))
            }
            Self::Substring3Ready { bytes, pos, len } => {
                operation.is_substring_native()
                    && ((matches!(bytes, ReadyBytesArg::Value(_))
                        && matches!(pos, ReadyIntArg::Value(_))
                        && matches!(len, ReadyIntArg::Value(_)))
                        || matches!(bytes, ReadyBytesArg::Value(None))
                        || matches!(pos, ReadyIntArg::Value(None))
                        || matches!(len, ReadyIntArg::Value(None)))
            }
            Self::LegacySubstring2Ready { bytes, pos } => {
                operation.is_substring_legacy()
                    && (!matches!(pos, ReadySubstringI128::Undemanded) || bytes.is_none())
            }
            Self::LegacySubstring3Ready { bytes, pos, len } => {
                if !operation.is_substring_legacy()
                    || (matches!(pos, ReadySubstringI128::Undemanded) && bytes.is_some())
                {
                    false
                } else {
                    let needs_len = match (bytes.as_deref(), pos) {
                        (Some(source), ReadySubstringI128::Value(Some(position))) => {
                            crate::impl_string::legacy_substring_needs_len(
                                source,
                                *position,
                                operation.substring_is_utf8(),
                            )
                        }
                        _ => false,
                    };
                    if needs_len {
                        matches!(len, ReadySubstringI128::Value(_))
                    } else {
                        matches!(len, ReadySubstringI128::Undemanded)
                    }
                }
            }
            Self::Ieee754Bits2 { left, right } => match (left, right) {
                (ReadyIeee754Arg::Value(_), ReadyIeee754Arg::Value(_)) => true,
                (ReadyIeee754Arg::Undemanded, ReadyIeee754Arg::Value(None)) => {
                    operation == EvaluatedBytesOp::PowNative
                }
                (ReadyIeee754Arg::Value(None), ReadyIeee754Arg::Undemanded) => {
                    matches!(
                        operation,
                        EvaluatedBytesOp::PowNative | EvaluatedBytesOp::Atan2LibmLegacy
                    )
                }
                _ => false,
            },
            Self::BytesIntReady {
                bytes,
                count: ReadyIntArg::Undemanded,
            } => operation == EvaluatedBytesOp::Sha2Native && bytes.is_none(),
            Self::BytesBytesIntReady {
                bytes,
                delimiter,
                count: ReadyIntArg::Undemanded,
            } => {
                matches!(
                    operation,
                    EvaluatedBytesOp::SubstringIndexSignedNative
                        | EvaluatedBytesOp::SubstringIndexUnsignedNative
                ) && (bytes.is_none()
                    || delimiter.is_none()
                    || delimiter.as_ref().is_some_and(Vec::is_empty))
            }
            Self::PacketBytesIntBytes {
                bytes,
                count,
                pad,
                disposition,
            } => {
                operation.is_pad_native()
                    && match (bytes, pad) {
                        (ReadyBytesArg::Value(_), ReadyBytesArg::Value(_)) => true,
                        (ReadyBytesArg::Undemanded, ReadyBytesArg::Undemanded) => {
                            count.is_none()
                                || *disposition == OutputDisposition::SuppressByPacket
                                || count.is_some_and(|value| !(0..=16_777_216).contains(&value))
                        }
                        _ => false,
                    }
            }
            Self::PacketBytesInt {
                bytes,
                count: ReadyIntArg::Undemanded,
                disposition,
            } => {
                operation == EvaluatedBytesOp::RepeatNative
                    && bytes.is_none()
                    && *disposition == OutputDisposition::Allow
            }
            _ => true,
        }
    }

    fn decimal_materialization_budget(
        value: Option<&Decimal>,
        available: usize,
    ) -> LocalResult<i64> {
        let remaining = available
            .checked_sub(value.map_or(0, Decimal::spill_capacity_bytes))
            .filter(|remaining| *remaining != usize::MAX)
            .ok_or_else(|| {
                LocalError::ResourceLimit("Decimal math requires finite remaining storage".into())
            })?;
        let bits = u64::try_from(remaining).map_err(|_| evaluated_ascii_storage_overflow())?;
        Ok(bits as i64)
    }

    fn native_vector_values(
        values: [Option<NativeVectorFloat32>; 2],
        arity: usize,
        available: usize,
    ) -> LocalResult<([ScalarValue; 4], usize)> {
        let mut live = values
            .iter()
            .try_fold(0usize, |total, value| {
                total.checked_add(
                    value
                        .as_ref()
                        .map_or(0, NativeVectorFloat32::elements_capacity)
                        .checked_mul(mem::size_of::<f32>())?,
                )
            })
            .ok_or_else(evaluated_ascii_storage_overflow)?;
        let check = |bytes: usize| {
            if bytes > available {
                Err(LocalError::ResourceLimit(
                    "native vector input conversion exceeds retained storage".into(),
                ))
            } else {
                Ok(())
            }
        };
        check(live)?;
        let mut ready = [
            ScalarValue::Int(None),
            ScalarValue::Int(None),
            ScalarValue::Int(None),
            ScalarValue::Int(None),
        ];
        for (slot, value) in values.into_iter().take(arity).enumerate() {
            let value = match value {
                None => None,
                Some(source) => {
                    let source_bytes = source
                        .elements_capacity()
                        .checked_mul(mem::size_of::<f32>())
                        .ok_or_else(evaluated_ascii_storage_overflow)?;
                    let requested = source
                        .len()
                        .checked_mul(mem::size_of::<f32>())
                        .ok_or_else(evaluated_ascii_storage_overflow)?;
                    check(
                        live.checked_add(requested)
                            .ok_or_else(evaluated_ascii_storage_overflow)?,
                    )?;
                    // No wire constructor/decoder: preserve the native-endian bits,
                    // including mutable NaN/Inf and dimensions above the text cap.
                    let wire = source.into_wire_raw();
                    let overlap = live
                        .checked_add(wire.value.capacity())
                        .ok_or_else(evaluated_ascii_storage_overflow)?;
                    check(overlap)?;
                    live = overlap
                        .checked_sub(source_bytes)
                        .ok_or_else(evaluated_ascii_storage_overflow)?;
                    Some(wire)
                }
            };
            ready[slot] = ScalarValue::VectorFloat32(value);
        }
        Ok((ready, arity))
    }

    fn into_values_for_operation(
        self,
        operation: EvaluatedBytesOp,
        available: usize,
    ) -> LocalResult<([ScalarValue; 6], usize, Option<NativeRegexpInvocation>)> {
        if !operation.is_decimal_int_div_budgeted() {
            return self.into_values(available);
        }
        let (left, right) = match &self {
            Self::BytesIntIntBytes(Some(left), _, _, Some(right))
                if operation.is_native_decimal_int_div() =>
            {
                (left, right)
            }
            Self::Bytes2(Some(left), Some(right))
                if operation == EvaluatedBytesOp::IntDivDecimalLegacy =>
            {
                (left, right)
            }
            _ => {
                return Err(LocalError::InvalidBatch(
                    "Decimal DIV requires actual operand frames".into(),
                ));
            }
        };
        let remaining = available
            .checked_sub(left.capacity())
            .and_then(|remaining| remaining.checked_sub(right.capacity()))
            .ok_or_else(evaluated_ascii_storage_overflow)?;
        let limit = Self::decimal_materialization_budget(None, remaining)?;
        let (mut ready, arity, invocation) = self.into_values(available)?;
        debug_assert_eq!(arity + 1, operation.input_types().len());
        ready[arity] = ScalarValue::Int(Some(limit));
        Ok((ready, arity + 1, invocation))
    }

    fn into_values(
        self,
        available: usize,
    ) -> LocalResult<([ScalarValue; 6], usize, Option<NativeRegexpInvocation>)> {
        // Only the selected arity enters the common driver. Regex invocation
        // handles are real semantic context, never hidden ScalarValue operands.
        use ScalarValue::{Bytes, Int};
        let (ready, arity) = match self {
            Self::TemporalText { .. } => {
                return Err(LocalError::InvalidSpec(
                    "temporal operands require their guarded zone projection".into(),
                ));
            }
            Self::Like {
                text,
                pattern,
                escape,
                ..
            } => ([Bytes(text), Bytes(pattern), Int(escape), Int(None)], 3),
            Self::RegexpLike {
                invocation,
                text,
                pattern,
                match_type,
            } => {
                return Ok((
                    [
                        Bytes(Some(text)),
                        Bytes(Some(pattern)),
                        Bytes(Some(match_type)),
                        Int(None),
                        Int(None),
                        Int(None),
                    ],
                    3,
                    Some(invocation),
                ));
            }
            Self::RegexpSubstr {
                invocation,
                text,
                pattern,
                pos,
                occurrence,
                match_type,
            } => {
                return Ok((
                    [
                        Bytes(Some(text)),
                        Bytes(Some(pattern)),
                        Int(Some(pos)),
                        Int(Some(occurrence)),
                        Bytes(Some(match_type)),
                        Int(None),
                    ],
                    5,
                    Some(invocation),
                ));
            }
            Self::RegexpInstr {
                invocation,
                text,
                pattern,
                pos,
                occurrence,
                return_option,
                match_type,
            } => {
                return Ok((
                    [
                        Bytes(Some(text)),
                        Bytes(Some(pattern)),
                        Int(Some(pos)),
                        Int(Some(occurrence)),
                        Int(Some(return_option)),
                        Self::ready_bytes_value(match_type),
                    ],
                    6,
                    Some(invocation),
                ));
            }
            Self::RegexpReplace {
                invocation,
                text,
                pattern,
                replacement,
                pos,
                occurrence,
                match_type,
            } => {
                return Ok((
                    [
                        Bytes(Some(text)),
                        Bytes(Some(pattern)),
                        Bytes(Some(replacement)),
                        Int(Some(pos)),
                        Int(Some(occurrence)),
                        Bytes(Some(match_type)),
                    ],
                    6,
                    Some(invocation),
                ));
            }
            Self::NativeVector(value) => Self::native_vector_values([value, None], 1, available)?,
            Self::NativeVector2(left, right) => {
                Self::native_vector_values([left, right], 2, available)?
            }
            Self::NoArgs => ([Int(None), Int(None), Int(None), Int(None)], 0),
            Self::Decimal2 { left, right } | Self::DecimalDivision { left, right, .. } => {
                // Both actual owners remain live through conversion and kernel
                // execution. Subtract each spill before exposing one budget.
                let available = available
                    .checked_sub(right.as_ref().map_or(0, Decimal::spill_capacity_bytes))
                    .ok_or_else(evaluated_ascii_storage_overflow)?;
                let limit = Self::decimal_materialization_budget(left.as_ref(), available)?;
                (
                    [
                        ScalarValue::Decimal(left),
                        ScalarValue::Decimal(right),
                        Int(Some(limit)),
                        Int(None),
                    ],
                    3,
                )
            }
            Self::Int1282(left, right) => (
                [
                    Bytes(left.map(|value| value.to_le_bytes().to_vec())),
                    Bytes(right.map(|value| value.to_le_bytes().to_vec())),
                    Int(None),
                    Int(None),
                ],
                2,
            ),
            Self::Decimal(value) => {
                let limit = Self::decimal_materialization_budget(value.as_ref(), available)?;
                (
                    [
                        ScalarValue::Decimal(value),
                        Int(Some(limit)),
                        Int(None),
                        Int(None),
                    ],
                    2,
                )
            }
            Self::DecimalIntReady { value, scale } => {
                let value = match value {
                    ReadyDecimalArg::Value(value) => value,
                    ReadyDecimalArg::Undemanded => Some(Decimal::zero()),
                };
                let limit = Self::decimal_materialization_budget(value.as_ref(), available)?;
                (
                    [
                        ScalarValue::Decimal(value),
                        Self::ready_int_value(scale),
                        Int(Some(limit)),
                        Int(None),
                    ],
                    3,
                )
            }
            Self::Ieee754BitsInt { value, scale } => (
                [
                    Self::ieee754_value(value)?,
                    Int(scale),
                    Int(None),
                    Int(None),
                ],
                2,
            ),
            Self::MakeTimeParts {
                hour,
                minute,
                second,
            } => (
                [
                    Self::make_time_hour_value(hour)?,
                    Int(minute),
                    Self::ieee754_value(second)?,
                    Int(None),
                ],
                3,
            ),
            Self::Int128(value) => (
                [
                    Self::substring_i128_value(ReadySubstringI128::Value(value))?,
                    Int(None),
                    Int(None),
                    Int(None),
                ],
                1,
            ),
            Self::NullWitness(value) => ([Int(value), Int(None), Int(None), Int(None)], 1),
            Self::CharReady(args) => (
                [
                    Bytes(Some(args.into_encoded())),
                    Int(None),
                    Int(None),
                    Int(None),
                ],
                1,
            ),
            Self::ConvReady {
                number,
                from_base,
                to_base,
            } => (
                [
                    Self::ready_bytes_value(number),
                    Self::ready_int_value(from_base),
                    Self::ready_int_value(to_base),
                    Int(None),
                ],
                3,
            ),
            Self::ConvLegacyReady {
                number,
                from_base,
                to_base,
            } => (
                [
                    Self::ready_bytes_value(number),
                    Self::conv_base_value(from_base)?,
                    Self::conv_base_value(to_base)?,
                    Int(None),
                ],
                3,
            ),
            Self::FieldReady(args) => (
                [
                    Bytes(Some(args.into_encoded())),
                    Int(None),
                    Int(None),
                    Int(None),
                ],
                1,
            ),
            Self::MakeSetReady(args) => (
                [
                    Bytes(Some(args.into_encoded())),
                    Int(None),
                    Int(None),
                    Int(None),
                ],
                1,
            ),
            Self::ExportSetReady(args) => {
                let (encoded, bits, count) = args.into_parts();
                ([Bytes(Some(encoded)), Int(bits), Int(count), Int(None)], 3)
            }
            Self::ConcatReady(args) => (
                [
                    Bytes(Some(args.into_encoded())),
                    Int(None),
                    Int(None),
                    Int(None),
                ],
                1,
            ),
            Self::EltReady {
                index,
                total_sql_arity,
                selected,
            } => {
                // This is an unsigned arity carrier, not a signed SQL operand.
                let arity = u64::try_from(total_sql_arity).map_err(|_| {
                    LocalError::ResourceLimit("ELT total arity exceeds u64 transport".into())
                })?;
                (
                    [
                        Int(index),
                        Int(Some(arity as i64)),
                        Self::ready_bytes_value(selected),
                        Int(None),
                    ],
                    3,
                )
            }
            Self::Bytes(value) => ([Bytes(value), Int(None), Int(None), Int(None)], 1),
            Self::Bytes2(a, b) => ([Bytes(a), Bytes(b), Int(None), Int(None)], 2),
            Self::CollatedBytes2 {
                left,
                right,
                collation,
            } => (
                [
                    Bytes(left),
                    Bytes(right),
                    Int(Some(collation.tag())),
                    Int(None),
                ],
                3,
            ),
            Self::SearchBytes2 {
                needle,
                haystack,
                policy,
            } => (
                [
                    Bytes(needle),
                    Bytes(haystack),
                    Int(Some(policy.tag())),
                    Int(None),
                ],
                3,
            ),
            Self::SearchBytes2IntReady {
                needle,
                haystack,
                pos,
                policy,
            } => (
                [
                    Bytes(needle),
                    Bytes(haystack),
                    Self::ready_int_value(pos),
                    Int(Some(policy.tag())),
                ],
                4,
            ),
            Self::BytesBytesInt(left, right, value) => {
                ([Bytes(left), Bytes(right), Int(value), Int(None)], 3)
            }
            Self::FindInSetPreparedReady {
                needle,
                keys,
                collation,
            } => (
                [
                    Self::ready_bytes_value(needle),
                    Bytes(keys.into_encoded()?),
                    Int(Some(collation.tag())),
                    Int(None),
                ],
                3,
            ),
            Self::Substring2Ready { bytes, pos } => (
                [
                    Self::ready_bytes_value(bytes),
                    Self::ready_int_value(pos),
                    Int(None),
                    Int(None),
                ],
                2,
            ),
            Self::Substring3Ready { bytes, pos, len } => (
                [
                    Self::ready_bytes_value(bytes),
                    Self::ready_int_value(pos),
                    Self::ready_int_value(len),
                    Int(None),
                ],
                3,
            ),
            Self::LegacySubstring2Ready { bytes, pos } => (
                [
                    Bytes(bytes),
                    Self::substring_i128_value(pos)?,
                    Int(None),
                    Int(None),
                ],
                2,
            ),
            Self::LegacySubstring3Ready { bytes, pos, len } => (
                [
                    Bytes(bytes),
                    Self::substring_i128_value(pos)?,
                    Self::substring_i128_value(len)?,
                    Int(None),
                ],
                3,
            ),
            Self::Int(value) => ([Int(value), Int(None), Int(None), Int(None)], 1),
            Self::BytesInt(bytes, int) => ([Bytes(bytes), Int(int), Int(None), Int(None)], 2),
            Self::BytesIntIntBytes(bytes, position, length, replacement) => (
                [Bytes(bytes), Int(position), Int(length), Bytes(replacement)],
                4,
            ),
            Self::BytesIntReady { bytes, count } => {
                let count = match count {
                    ReadyIntArg::Value(value) => value,
                    // Validated Sha2Native + NULL left: irrelevant, not SQL NULL.
                    ReadyIntArg::Undemanded => Some(0),
                };
                ([Bytes(bytes), Int(count), Int(None), Int(None)], 2)
            }
            Self::BytesBytesIntReady {
                bytes,
                delimiter,
                count,
            } => {
                let count = match count {
                    ReadyIntArg::Value(value) => value,
                    // Validated SUBSTRING_INDEX early exit; not a NULL count.
                    ReadyIntArg::Undemanded => Some(0),
                };
                ([Bytes(bytes), Bytes(delimiter), Int(count), Int(None)], 3)
            }
            Self::Bytes3([a, b, c]) => ([Bytes(a), Bytes(b), Bytes(c), Int(None)], 3),
            Self::Int2(lhs, rhs) => ([Int(lhs), Int(rhs), Int(None), Int(None)], 2),
            Self::PacketInt { value, disposition } => (
                [
                    Int(value),
                    Int(Some(disposition.flag())),
                    Int(None),
                    Int(None),
                ],
                2,
            ),
            Self::PacketBytes { value, disposition } => (
                [
                    Bytes(value),
                    Int(Some(disposition.flag())),
                    Int(None),
                    Int(None),
                ],
                2,
            ),
            Self::PacketBytesInt {
                bytes,
                count,
                disposition,
            } => {
                let count = match count {
                    ReadyIntArg::Value(value) => value,
                    // Validated RepeatNative + NULL left + Allow; not SQL NULL.
                    ReadyIntArg::Undemanded => Some(0),
                };
                (
                    [
                        Bytes(bytes),
                        Int(count),
                        Int(Some(disposition.flag())),
                        Int(None),
                    ],
                    3,
                )
            }
            Self::PacketBytesIntBytes {
                bytes,
                count,
                pad,
                disposition,
            } => {
                // Admission checked the pair and the earlier length/packet exit.
                // Empty representatives do not label original strings as NULL.
                let into_ready = |arg: ReadyBytesArg| match arg {
                    ReadyBytesArg::Value(value) => value,
                    ReadyBytesArg::Undemanded => Some(Vec::new()),
                };
                (
                    [
                        Bytes(into_ready(bytes)),
                        Int(count),
                        Bytes(into_ready(pad)),
                        Int(Some(disposition.flag())),
                    ],
                    4,
                )
            }
            Self::Ieee754Bits(value) => (
                [Self::ieee754_value(value)?, Int(None), Int(None), Int(None)],
                1,
            ),
            Self::TimeCoreBits(value) => (
                [
                    Self::raw_u64_value(value, "time core bits input allocation failed")?,
                    Int(None),
                    Int(None),
                    Int(None),
                ],
                1,
            ),
            Self::TimeCoreBits2(left, right) => (
                [
                    Self::raw_u64_value(left, "time core bits input allocation failed")?,
                    Self::raw_u64_value(right, "time core bits input allocation failed")?,
                    Int(None),
                    Int(None),
                ],
                2,
            ),
            Self::TimeCoreBitsBytes { core, bytes } => (
                [
                    Self::raw_u64_value(Some(core), "time core bits input allocation failed")?,
                    Bytes(bytes),
                    Int(None),
                    Int(None),
                ],
                2,
            ),
            Self::Ieee754Bits2 { left, right } => {
                // Admission allows POW with a truly NULL opposite operand,
                // or legacy ATAN2's undemanded right after an actual left NULL.
                // This irrelevant +0 representative is never a fake SQL NULL.
                let into_ready = |arg: ReadyIeee754Arg| match arg {
                    ReadyIeee754Arg::Value(value) => value,
                    ReadyIeee754Arg::Undemanded => Some(0),
                };
                (
                    [
                        Self::ieee754_value(into_ready(left))?,
                        Self::ieee754_value(into_ready(right))?,
                        Int(None),
                        Int(None),
                    ],
                    2,
                )
            }
        };
        let [first, second, third, fourth] = ready;
        Ok((
            [first, second, third, fourth, Int(None), Int(None)],
            arity,
            None,
        ))
    }

    fn ready_bytes_value(arg: ReadyBytesArg) -> ScalarValue {
        // Admission proved this operand irrelevant; the representative is not NULL.
        ScalarValue::Bytes(match arg {
            ReadyBytesArg::Value(value) => value,
            ReadyBytesArg::Undemanded => Some(Vec::new()),
        })
    }

    fn ready_int_value(arg: ReadyIntArg) -> ScalarValue {
        ScalarValue::Int(match arg {
            ReadyIntArg::Value(value) => value,
            ReadyIntArg::Undemanded => Some(0),
        })
    }

    fn conv_base_value(arg: ReadyConvBaseArg) -> LocalResult<ScalarValue> {
        // Share only the canonical LE16 transport, never SUBSTRING semantics.
        Self::substring_i128_value(match arg {
            ReadyConvBaseArg::Value(value) => ReadySubstringI128::Value(value),
            ReadyConvBaseArg::Undemanded => ReadySubstringI128::Undemanded,
        })
    }

    fn substring_i128_value(arg: ReadySubstringI128) -> LocalResult<ScalarValue> {
        let value = match arg {
            ReadySubstringI128::Value(value) => value,
            // Checked legacy demand, not an evaluated integer or SQL NULL.
            ReadySubstringI128::Undemanded => Some(0),
        };
        let value = value
            .map(|value| -> LocalResult<Vec<u8>> {
                let mut bytes = Vec::new();
                bytes.try_reserve_exact(16).map_err(|_| {
                    LocalError::ResourceLimit(
                        "legacy SUBSTRING i128 input allocation failed".into(),
                    )
                })?;
                bytes.extend_from_slice(&value.to_le_bytes());
                Ok(bytes)
            })
            .transpose()?;
        Ok(ScalarValue::Bytes(value))
    }

    fn make_time_hour_value(value: Option<(i64, bool)>) -> LocalResult<ScalarValue> {
        let value = value
            .map(|(hour, unsigned)| -> LocalResult<Vec<u8>> {
                let mut bytes = Vec::new();
                bytes.try_reserve_exact(9).map_err(|_| {
                    LocalError::ResourceLimit("MAKETIME hour input allocation failed".into())
                })?;
                bytes.extend_from_slice(&hour.to_le_bytes());
                bytes.push(u8::from(unsigned));
                Ok(bytes)
            })
            .transpose()?;
        Ok(ScalarValue::Bytes(value))
    }

    fn ieee754_value(value: Option<u64>) -> LocalResult<ScalarValue> {
        Self::raw_u64_value(value, "IEEE754 input allocation failed")
    }

    // Share only fallible physical LE8 allocation. Logical roles are checked
    // before materialization and remain distinct even for a nullable input.
    fn raw_u64_value(
        value: Option<u64>,
        allocation_failure: &'static str,
    ) -> LocalResult<ScalarValue> {
        let value = value
            .map(|bits| -> LocalResult<Vec<u8>> {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(8)
                    .map_err(|_| LocalError::ResourceLimit(allocation_failure.into()))?;
                bytes.extend_from_slice(&bits.to_le_bytes());
                Ok(bytes)
            })
            .transpose()?;
        Ok(ScalarValue::Bytes(value))
    }
}

/// The fixed computed result identity, including for a NULL result. It is not
/// the operand's metadata, a SQL return descriptor, or a control-lineage ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComputedIntMetadata {
    OwnSignedInt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComputedInt {
    value: Option<i64>,
}

impl ComputedInt {
    pub fn value(&self) -> Option<i64> {
        self.value
    }

    pub fn into_option(self) -> Option<i64> {
        self.value
    }

    pub fn metadata(&self) -> ComputedIntMetadata {
        ComputedIntMetadata::OwnSignedInt
    }
}

/// Own computed Bytes, not the operand's lineage or a SQL charset/collation.
/// The caller still owns native text/binary packing and return-type metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComputedBytesMetadata {
    OwnBytes,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ComputedBytes {
    value: Option<Vec<u8>>,
}

impl ComputedBytes {
    pub fn value(&self) -> Option<&[u8]> {
        self.value.as_deref()
    }

    pub fn into_option(self) -> Option<Vec<u8>> {
        self.value
    }

    pub fn metadata(&self) -> ComputedBytesMetadata {
        ComputedBytesMetadata::OwnBytes
    }
}

/// A completed UNCOMPRESS result. Corrupt and OutputLimit are decoder
/// outcomes, not runtime allocation errors or already-emitted SQL warnings.
#[derive(Debug, PartialEq, Eq)]
pub enum UncompressOutcome {
    Null,
    Value(Vec<u8>),
    Corrupt,
    OutputLimit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComputedUncompressMetadata {
    OwnUncompress,
}

/// Owns the result of the sealed UNCOMPRESS wrapper, never an input tag.
/// The frontend alone applies the corresponding statement warning policy.
#[derive(Debug, PartialEq, Eq)]
pub struct ComputedUncompress {
    outcome: UncompressOutcome,
}

impl ComputedUncompress {
    pub fn outcome(&self) -> &UncompressOutcome {
        &self.outcome
    }
    pub fn into_outcome(self) -> UncompressOutcome {
        self.outcome
    }
    pub fn metadata(&self) -> ComputedUncompressMetadata {
        ComputedUncompressMetadata::OwnUncompress
    }
}

// A short-lived view of the private kernel result, not a new RPN carrier or
// public constructor. Only the selected UNCOMPRESS extraction uses this frame.
#[derive(Debug, PartialEq, Eq)]
enum UncompressFrame<'a> {
    Null,
    Value(&'a [u8]),
    Corrupt,
    OutputLimit,
}

fn decode_uncompress_frame(encoded: Option<&[u8]>) -> LocalResult<UncompressFrame<'_>> {
    match encoded {
        None => Ok(UncompressFrame::Null),
        Some([0, payload @ ..]) => Ok(UncompressFrame::Value(payload)),
        Some([1]) => Ok(UncompressFrame::Corrupt),
        Some([2]) => Ok(UncompressFrame::OutputLimit),
        _ => Err(LocalError::InvalidBatch(
            "UNCOMPRESS result has an invalid canonical envelope".into(),
        )),
    }
}

/// A computed result of the closed JSON report recipes. Text parser
/// failures are explicit outcomes, not inferred SQL diagnostics or raw errors.
#[derive(Debug, PartialEq, Eq)]
pub enum JsonReportOutcome {
    Null,
    Bytes(Vec<u8>),
    Int(i64),
    EmptyText,
    InvalidText,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComputedJsonReportMetadata {
    OwnJsonReport,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ComputedJsonReport {
    outcome: JsonReportOutcome,
}

impl ComputedJsonReport {
    pub fn outcome(&self) -> &JsonReportOutcome {
        &self.outcome
    }
    pub fn into_outcome(self) -> JsonReportOutcome {
        self.outcome
    }
    pub fn metadata(&self) -> ComputedJsonReportMetadata {
        ComputedJsonReportMetadata::OwnJsonReport
    }
}

// An ephemeral result view, not an input JSON domain or a public constructor.
#[derive(Debug, PartialEq, Eq)]
enum JsonReportFrame<'a> {
    Null,
    Bytes(&'a [u8]),
    Int(i64),
    EmptyText,
    InvalidText,
}

fn decode_json_report_frame(
    operation: EvaluatedBytesOp,
    encoded: Option<&[u8]>,
) -> LocalResult<JsonReportFrame<'_>> {
    let invalid = || {
        LocalError::InvalidBatch("JSON report has an invalid canonical envelope or recipe".into())
    };
    if !operation.returns_json_report() {
        return Err(invalid());
    }
    match encoded {
        None => Ok(JsonReportFrame::Null),
        Some([0, payload @ ..])
            if matches!(
                operation,
                EvaluatedBytesOp::JsonDepthNative
                    | EvaluatedBytesOp::JsonStorageFreeNative
                    | EvaluatedBytesOp::JsonStorageSizeNative
            ) =>
        {
            let bytes = <[u8; 8]>::try_from(payload).map_err(|_| invalid())?;
            Ok(JsonReportFrame::Int(i64::from_le_bytes(bytes)))
        }
        Some([0, payload @ ..]) => Ok(JsonReportFrame::Bytes(payload)),
        Some([1])
            if matches!(
                operation,
                EvaluatedBytesOp::JsonTypeTextNative
                    | EvaluatedBytesOp::JsonDepthNative
                    | EvaluatedBytesOp::JsonStorageFreeNative
                    | EvaluatedBytesOp::JsonStorageSizeNative
            ) =>
        {
            Ok(JsonReportFrame::EmptyText)
        }
        Some([2]) => Ok(JsonReportFrame::InvalidText),
        _ => Err(invalid()),
    }
}

/// IEEE754 output identity, not a SQL integer, Bytes descriptor or input donor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComputedIeee754BitsMetadata {
    OwnIeee754Bits,
}

/// Owned nullable IEEE754 binary64 bits. NaN/Inf/signed zero remain values;
/// equality here is bitwise identity, not floating-point SQL comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComputedIeee754Bits {
    value: Option<u64>,
}

impl ComputedIeee754Bits {
    pub fn value(&self) -> Option<u64> {
        self.value
    }

    pub fn into_option(self) -> Option<u64> {
        self.value
    }

    pub fn metadata(&self) -> ComputedIeee754BitsMetadata {
        ComputedIeee754BitsMetadata::OwnIeee754Bits
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComputedDecimalMetadata {
    OwnDecimal,
}

/// Owns the exact shared Decimal, including wide storage and presentation
/// state.
#[derive(Debug, PartialEq, Eq)]
pub struct ComputedDecimal {
    value: Option<Decimal>,
    checked_i64_view: Option<i64>,
}

impl ComputedDecimal {
    pub fn value(&self) -> Option<&Decimal> {
        self.value.as_ref()
    }
    pub fn into_option(self) -> Option<Decimal> {
        self.value
    }
    pub fn metadata(&self) -> ComputedDecimalMetadata {
        ComputedDecimalMetadata::OwnDecimal
    }
    /// Available only for integral CEIL/FLOOR and constant integer-negation
    /// results that fit exactly in i64.
    /// None leaves the original Decimal intact, including out-of-range results.
    pub fn checked_i64_view(&self) -> Option<i64> {
        self.checked_i64_view
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComputedDecimalDivisionMetadata {
    OwnDecimalDivision,
}

/// The budgeted owned official Decimal and this call's actual division status.
#[derive(Debug, PartialEq, Eq)]
pub struct ComputedDecimalDivision {
    value: Option<Decimal>,
    disposition: NativeDecimalDivisionDisposition,
}

impl ComputedDecimalDivision {
    pub fn value(&self) -> Option<&Decimal> {
        self.value.as_ref()
    }

    pub fn disposition(&self) -> NativeDecimalDivisionDisposition {
        self.disposition
    }

    pub fn into_parts(self) -> (Option<Decimal>, NativeDecimalDivisionDisposition) {
        (self.value, self.disposition)
    }

    pub fn metadata(&self) -> ComputedDecimalDivisionMetadata {
        ComputedDecimalDivisionMetadata::OwnDecimalDivision
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComputedDecimalFastMetadata {
    OwnDecimalFast,
}

/// Owns the kernel's actual fast outcome, including distinct NULL/Unsupported.
/// Decoding is owned by impl_arithmetic; this wrapper performs no arithmetic.
#[derive(Debug, PartialEq, Eq)]
pub struct ComputedDecimalFast {
    outcome: NativeDecimalFastOutcome,
}

impl ComputedDecimalFast {
    pub fn outcome(&self) -> &NativeDecimalFastOutcome {
        &self.outcome
    }
    pub fn into_outcome(self) -> NativeDecimalFastOutcome {
        self.outcome
    }
    pub fn metadata(&self) -> ComputedDecimalFastMetadata {
        ComputedDecimalFastMetadata::OwnDecimalFast
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComputedInt128Metadata {
    OwnInt128,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ComputedInt128 {
    value: Option<i128>,
}

impl ComputedInt128 {
    pub fn value(&self) -> Option<i128> {
        self.value
    }
    pub fn into_option(self) -> Option<i128> {
        self.value
    }
    pub fn metadata(&self) -> ComputedInt128Metadata {
        ComputedInt128Metadata::OwnInt128
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComputedNativeVectorMetadata {
    OwnNativeVector,
}

/// Owns the aligned result. Equality is transport-bit equality, deliberately
/// separate from the shared native vector's unchanged floating-point PartialEq.
#[derive(Debug)]
pub struct ComputedNativeVector {
    value: Option<NativeVectorFloat32>,
}

impl ComputedNativeVector {
    pub fn value(&self) -> Option<&NativeVectorFloat32> {
        self.value.as_ref()
    }
    pub fn into_value(self) -> Option<NativeVectorFloat32> {
        self.value
    }
    pub fn metadata(&self) -> ComputedNativeVectorMetadata {
        ComputedNativeVectorMetadata::OwnNativeVector
    }
}

impl PartialEq for ComputedNativeVector {
    fn eq(&self, other: &Self) -> bool {
        self.metadata() == other.metadata()
            && match (&self.value, &other.value) {
                (None, None) => true,
                (Some(left), Some(right)) => {
                    left.len() == right.len()
                        && left
                            .elements()
                            .iter()
                            .zip(right.elements())
                            .all(|(left, right)| left.to_bits() == right.to_bits())
                }
                _ => false,
            }
    }
}
impl Eq for ComputedNativeVector {}

fn materialize_native_vector(
    source: &[u8],
    physical_bytes: usize,
    input_bytes: usize,
    budget: &EvalBudget,
) -> LocalResult<NativeVectorFloat32> {
    let invalid = |error: NativeVectorError| {
        LocalError::InvalidBatch(format!(
            "native vector result has invalid serialized layout: {error}"
        ))
    };
    let encoded = peek_native_vector_float32(source).map_err(invalid)?;
    if encoded != source.len() {
        return Err(LocalError::InvalidBatch(
            "native vector result has a trailing suffix".into(),
        ));
    }
    let requested = encoded
        .checked_sub(mem::size_of::<u32>())
        .ok_or_else(evaluated_ascii_storage_overflow)?;
    let overlap = physical_bytes
        .checked_add(requested)
        .ok_or_else(evaluated_ascii_storage_overflow)?;
    budget.check_output(overlap, input_bytes)?;
    // This is the actual computed vector's standard LE image. Decode layout
    // only; do not re-run the text parser, finite checks or dimension policy.
    let (owned, suffix) = deserialize_native_vector_float32(source).map_err(invalid)?;
    if !suffix.is_empty() {
        return Err(LocalError::InvalidBatch(
            "native vector result has a trailing suffix".into(),
        ));
    }
    let retained = owned
        .elements_capacity()
        .checked_mul(mem::size_of::<f32>())
        .ok_or_else(evaluated_ascii_storage_overflow)?;
    let overlap = physical_bytes
        .checked_add(retained)
        .ok_or_else(evaluated_ascii_storage_overflow)?;
    budget.check_output(overlap, input_bytes)?;
    Ok(owned)
}

/// Only a semantic cause returned by the matching sealed invocation can
/// authorize this view. Neither numeric error codes nor messages classify it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvaluatedSqlFailureKind {
    AbsSignedOverflow,
    ConvUnsignedOverflow,
    PeriodAddIncorrectArguments,
    PeriodDiffIncorrectArguments,
    UuidToBinWhitespace,
    UuidToBinInvalid,
    UuidVersionInvalid,
    UuidTimestampInvalid,
    BinToUuidInvalidLength,
    VectorNative,
    RegexpNative,
    UnaryMinusNative,
    BinaryArithmeticNative,
    BinaryArithmeticLegacy,
    AesNative,
}

/// Fresh owned failure-only observation for one ready-value invocation.
/// This is not retained in the worker, context, program, or pool.
#[derive(Debug)]
pub struct ReportedEvaluatedFailure {
    error: LocalError,
    operation: Option<EvaluatedBytesOp>,
    sql_failure: Option<EvaluatedSqlFailureKind>,
}

impl ReportedEvaluatedFailure {
    fn unreported(error: LocalError) -> Self {
        Self {
            error,
            operation: None,
            sql_failure: None,
        }
    }
    pub fn error(&self) -> &LocalError {
        &self.error
    }
    pub fn into_error(self) -> LocalError {
        self.error
    }
    pub fn operation(&self) -> Option<EvaluatedBytesOp> {
        self.operation
    }
    pub fn sql_failure(&self) -> Option<EvaluatedSqlFailureKind> {
        self.sql_failure
    }
    /// Borrow the actual native cause only for its exact operation/domain
    /// recipe.
    pub fn native_binary_arithmetic_error(&self) -> Option<&NativeBinaryArithmeticError> {
        if self.sql_failure != Some(EvaluatedSqlFailureKind::BinaryArithmeticNative) {
            return None;
        }
        let profile = self.operation?.native_binary_error_profile()?;
        match &self.error {
            LocalError::Evaluation(error) => match error.0.as_ref() {
                ErrorInner::Evaluate(EvaluateError::Caused(source)) => source
                    .downcast_ref::<NativeBinaryArithmeticError>()
                    .filter(|cause| (cause.operation, cause.kind) == profile),
                _ => None,
            },
            _ => None,
        }
    }

    /// Legacy real/Decimal warning values are not integer overflow receipts.
    pub fn legacy_binary_arithmetic_error(&self) -> Option<&LegacyBinaryArithmeticError> {
        if self.sql_failure != Some(EvaluatedSqlFailureKind::BinaryArithmeticLegacy) {
            return None;
        }
        let profile = self.operation?.legacy_binary_error_profile()?;
        match &self.error {
            LocalError::Evaluation(error) => match error.0.as_ref() {
                ErrorInner::Evaluate(EvaluateError::Caused(source)) => source
                    .downcast_ref::<LegacyBinaryArithmeticError>()
                    .filter(|cause| (cause.operation, cause.unsigned) == profile),
                _ => None,
            },
            _ => None,
        }
    }

    /// Only this invocation's exact IV-mode recipe authenticates a short IV.
    /// ECB, SQL NULL, cipher rejection, and infrastructure errors do not.
    pub fn native_aes_error(&self) -> Option<&NativeAesError> {
        if self.sql_failure != Some(EvaluatedSqlFailureKind::AesNative) {
            return None;
        }
        let profile = self.operation?.aes_error_profile()?;
        match &self.error {
            LocalError::Evaluation(error) => match error.0.as_ref() {
                ErrorInner::Evaluate(EvaluateError::Caused(source)) => source
                    .downcast_ref::<NativeAesError>()
                    .filter(|cause| (cause.operation(), cause.profile()) == profile),
                _ => None,
            },
            _ => None,
        }
    }

    /// Only dynamic signed/unsigned negation authenticates this typed source.
    /// A constant's Decimal widening and all scope failures are different
    /// paths.
    pub fn native_unary_minus_error(&self) -> Option<&NativeUnaryMinusError> {
        if self.sql_failure != Some(EvaluatedSqlFailureKind::UnaryMinusNative) {
            return None;
        }
        let unsigned = match self.operation {
            Some(EvaluatedBytesOp::UnaryMinusIntNative) => false,
            Some(EvaluatedBytesOp::UnaryMinusUIntNative) => true,
            _ => return None,
        };
        match &self.error {
            LocalError::Evaluation(error) => match error.0.as_ref() {
                ErrorInner::Evaluate(EvaluateError::Caused(source)) => source
                    .downcast_ref::<NativeUnaryMinusError>()
                    .filter(|cause| cause.unsigned == unsigned),
                _ => None,
            },
            _ => None,
        }
    }

    /// Only the four native regexp operations authenticate this actual cause.
    pub fn native_regexp_error(&self) -> Option<&NativeRegexpError> {
        if self.sql_failure != Some(EvaluatedSqlFailureKind::RegexpNative)
            || !self
                .operation
                .is_some_and(|operation| operation.regexp_kind().is_some())
        {
            return None;
        }
        match &self.error {
            LocalError::Evaluation(error) => match error.0.as_ref() {
                ErrorInner::Evaluate(EvaluateError::Caused(source)) => {
                    source.downcast_ref::<NativeRegexpError>()
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Borrows only the matching invocation's actual typed native vector cause.
    pub fn native_vector_error(&self) -> Option<&NativeVectorError> {
        if self.sql_failure != Some(EvaluatedSqlFailureKind::VectorNative)
            || !matches!(
                self.operation,
                Some(
                    EvaluatedBytesOp::VecFromTextNative
                        | EvaluatedBytesOp::VecL1DistanceNative
                        | EvaluatedBytesOp::VecL2DistanceNative
                        | EvaluatedBytesOp::VecNegativeInnerProductNative
                        | EvaluatedBytesOp::VecCosineDistanceNative
                        | EvaluatedBytesOp::AddVectorNative
                        | EvaluatedBytesOp::SubVectorNative
                        | EvaluatedBytesOp::MulVectorNative
                )
            )
        {
            return None;
        }
        match &self.error {
            LocalError::Evaluation(error) => match error.0.as_ref() {
                ErrorInner::Evaluate(EvaluateError::Caused(source)) => {
                    source.downcast_ref::<NativeVectorError>()
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Borrow only the authenticated BIN_TO_UUID cause's original bytes.
    /// Neither the caller's input nor an error message supplies this payload.
    pub fn bin_to_uuid_input(&self) -> Option<&[u8]> {
        if self.operation != Some(EvaluatedBytesOp::BinToUuidNative)
            || self.sql_failure != Some(EvaluatedSqlFailureKind::BinToUuidInvalidLength)
        {
            return None;
        }
        match &self.error {
            LocalError::Evaluation(error) => match error.0.as_ref() {
                ErrorInner::Evaluate(EvaluateError::BinToUuidInvalidLength { input }) => {
                    Some(input.as_slice())
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Borrow the actual failing conversion stage's sign-stripped digits.
    /// They live in the original typed cause; no input reparse or prediction
    /// is performed, and unreported or unrelated failures expose no payload.
    pub fn conv_overflow_digits(&self) -> Option<&str> {
        if self.sql_failure != Some(EvaluatedSqlFailureKind::ConvUnsignedOverflow) {
            return None;
        }
        match &self.error {
            LocalError::Evaluation(error) => match error.0.as_ref() {
                ErrorInner::Evaluate(EvaluateError::ConvUnsignedOverflow { digits, .. }) => {
                    Some(digits.as_str())
                }
                _ => None,
            },
            _ => None,
        }
    }
}

impl std::fmt::Display for ReportedEvaluatedFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, formatter)
    }
}

impl std::error::Error for ReportedEvaluatedFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// The complete result domain of the closed ready-value worker. Every carrier
/// owns its computed result, including NULL; none borrows the input/worker.
#[derive(Debug, PartialEq, Eq)]
pub enum ComputedValue {
    NativeVector(ComputedNativeVector),
    Int(ComputedInt),
    Bytes(ComputedBytes),
    Uncompress(ComputedUncompress),
    JsonReport(ComputedJsonReport),
    Ieee754Bits(ComputedIeee754Bits),
    Decimal(ComputedDecimal),
    DecimalDivision(ComputedDecimalDivision),
    DecimalFast(ComputedDecimalFast),
    Int128(ComputedInt128),
}

/// Checked retained storage for one worker. Inline bytes are separate so a
/// caller does not charge them twice inside an idle Vec's allocated slots.
/// Allocator rounding/bookkeeping and caller-owned pool allocations are not
/// included. This is an admitted-boundary observation, not an allocator peak.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerStorage {
    inline_bytes: usize,
    owned_heap_bytes: usize,
    total_bytes: usize,
}

impl WorkerStorage {
    fn new(inline_bytes: usize, owned_heap_bytes: usize) -> LocalResult<Self> {
        let total_bytes = inline_bytes
            .checked_add(owned_heap_bytes)
            .ok_or_else(evaluated_ascii_storage_overflow)?;
        Ok(Self {
            inline_bytes,
            owned_heap_bytes,
            total_bytes,
        })
    }

    pub fn inline_bytes(&self) -> usize {
        self.inline_bytes
    }

    pub fn owned_heap_bytes(&self) -> usize {
        self.owned_heap_bytes
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }
}

// ACCOUNTING ONLY: the approved allocation-request extent proxy. January's
// ArcInner source uses this representation; both supported pins require the
// parent's isolated allocation-request probe. This does NOT assert the other
// pin's private offsets or a portable Arc ABI. No pointer/offset access uses
// this proxy. Re-review on standard-library/EvalConfig changes; January's
// Atomic<usize> maps to AtomicUsize. No observed numeric byte count is
// hardcoded.
#[repr(C, align(2))]
struct EvaluatedAsciiConfigAllocation {
    _strong: AtomicUsize,
    _weak: AtomicUsize,
    _data: EvalConfig,
}

fn evaluated_ascii_storage_overflow() -> LocalError {
    LocalError::ResourceLimit("evaluated ASCII worker storage overflow".into())
}

fn evaluated_ascii_owned_heap_bytes(
    node_capacity: usize,
    schema_capacity: usize,
    metadata_bytes: usize,
    warning_capacity: usize,
) -> LocalResult<usize> {
    node_capacity
        .checked_mul(mem::size_of::<RpnExpressionNode>())
        .and_then(|bytes| {
            schema_capacity
                .checked_mul(mem::size_of::<tipb::FieldType>())
                .and_then(|schema| bytes.checked_add(schema))
        })
        .and_then(|bytes| bytes.checked_add(metadata_bytes))
        .and_then(|bytes| bytes.checked_add(mem::size_of::<EvaluatedAsciiConfigAllocation>()))
        .and_then(|bytes| {
            warning_capacity
                .checked_mul(mem::size_of::<tipb::Error>())
                .and_then(|warnings| bytes.checked_add(warnings))
        })
        .ok_or_else(evaluated_ascii_storage_overflow)
}

fn evaluated_ascii_context_is_sealed(ctx: &EvalContext) -> bool {
    let cfg = &ctx.cfg;
    // The only config Arc is moved into this private context at construction.
    // Its UTC enum and the remaining scalar config fields own no nested heap.
    Arc::strong_count(cfg) == 1
        && Arc::weak_count(cfg) == 0
        && matches!(&cfg.tz, Tz::Name(tz) if *tz == chrono_tz::UTC)
        && cfg.flag.is_empty()
        && cfg.sql_mode.is_empty()
        && cfg.max_warning_cnt == 0
        && cfg.paging_size.is_none()
        && cfg.max_keys_read.is_none()
        && cfg.div_precision_increment == DEFAULT_DIV_FRAC_INCR
        && !cfg.is_test
        && ctx.warnings.warning_cnt == 0
        && ctx.warnings.warnings.is_empty()
}

/// An exclusively owned, reusable runtime for one closed ready-value operation.
/// It is Send, not Sync, and exposes no program, context, services, native
/// graph or mutable configuration. A caller may cache workers by operation in
/// one synchronized owner; unhealthy or unwinding workers must not be recycled.
///
/// Per-call limits cover ready input and driver/result owners, NOT this
/// retained program/context/state. They are boundary accounting, not a bound on
/// temporary kernel allocations (including UNHEX's decoder/padding).
/// Worker/pool ownership remains separately charged. No operand, result or
/// invocation borrow is cached. Native regexp calls briefly bind shared handles
/// to statement-owned caches and charge their observed known
/// layouts/capacities; opaque regex engine/TLS allocations are excluded, not
/// claimed ExactRetained.
pub struct EvaluatedBytesWorker {
    operation: EvaluatedBytesOp,
    program: LocalProgram,
    state: LocalEvalState,
    ctx: EvalContext,
    witness: EvaluatedAsciiWitness,
    max_worker_retained_bytes: usize,
    accepted_storage: WorkerStorage,
    poisoned: bool,
}

impl std::fmt::Debug for EvaluatedBytesWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvaluatedBytesWorker")
            .field("operation", &self.operation)
            .field("kernel_invocations", &self.kernel_invocations())
            .field("healthy", &self.is_healthy())
            .finish_non_exhaustive()
    }
}

/// Prepare one fixed operation after the frontend has produced demanded ready
/// arguments, under the caller's creating-worker reservation. No kernel is
/// evaluated (not even a fake NULL), and no native descriptor or SQL context is
/// retained by the idle worker. Native regexp payloads are prepared unbound;
/// their explicit statement-cache handles are supplied only during evaluation.
/// The private UTC context disables warning storage and still checks that no
/// warning was raised. Prewarming inspects structure, never a fabricated input
/// or an assumption that NULL input must produce NULL output.
pub fn prepare_evaluated_bytes(
    operation: EvaluatedBytesOp,
    cx: LocalCompileContext,
    execution: ExecutionLimits,
    max_worker_retained_bytes: usize,
) -> LocalResult<EvaluatedBytesWorker> {
    let program = compile_evaluated_bytes(operation, cx)?;
    program.check_entry(operation.entry())?;
    // Fully warm the fixed program's owned metadata BEFORE publication. These
    // getters only inspect source structure; none dispatches an RPN function.
    let arity = operation.input_types().len();
    let nodes = arity + operation.call_count();
    if program.expression.node_count() != nodes
        || program.expression.work_count() != nodes
        || program.expression.column_ref_count() != arity
        || !program
            .expression
            .referenced_column_offsets()
            .iter()
            .copied()
            .eq(0..arity)
    {
        return Err(LocalError::InvalidSpec(
            "evaluated ASCII compiled metadata differs from its fixed recipe".into(),
        ));
    }
    let mut cfg = EvalConfig::new();
    cfg.set_max_warning_cnt(0);
    let mut runtime = EvaluatedBytesWorker {
        operation,
        program,
        state: LocalEvalState::with_limits(execution),
        ctx: EvalContext::new(Arc::new(cfg)),
        witness: EvaluatedAsciiWitness::default(),
        max_worker_retained_bytes,
        accepted_storage: WorkerStorage::new(0, 0)?,
        poisoned: false,
    };
    let storage = runtime.retained_storage()?;
    if storage.total_bytes() > max_worker_retained_bytes {
        return Err(LocalError::ResourceLimit(
            "evaluated ASCII worker retained storage exceeded".into(),
        ));
    }
    runtime.accepted_storage = storage;
    Ok(runtime)
}

/// Compatible ASCII-only facade over the shared closed ready-value worker.
/// Transparent representation keeps inline storage accounting identical to its
/// sole owned runtime; it exposes neither operation mutation nor a raw program.
#[repr(transparent)]
pub struct EvaluatedAsciiWorker {
    inner: EvaluatedBytesWorker,
}

impl std::fmt::Debug for EvaluatedAsciiWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvaluatedAsciiWorker")
            .field("kernel_invocations", &self.kernel_invocations())
            .field("healthy", &self.is_healthy())
            .finish_non_exhaustive()
    }
}

pub fn prepare_evaluated_ascii(
    cx: LocalCompileContext,
    execution: ExecutionLimits,
    max_worker_retained_bytes: usize,
) -> LocalResult<EvaluatedAsciiWorker> {
    prepare_evaluated_bytes(
        EvaluatedBytesOp::Ascii,
        cx,
        execution,
        max_worker_retained_bytes,
    )
    .map(|inner| EvaluatedAsciiWorker { inner })
}

impl EvaluatedAsciiWorker {
    pub fn kernel_invocations(&self) -> u64 {
        self.inner.kernel_invocations()
    }

    pub fn is_healthy(&self) -> bool {
        self.inner.is_healthy()
    }

    pub fn retained_storage(&self) -> LocalResult<WorkerStorage> {
        self.inner.retained_storage()
    }

    pub fn eval_one(&mut self, bytes: Option<Vec<u8>>) -> LocalResult<ComputedInt> {
        match self.inner.eval_one(bytes)? {
            ComputedValue::Int(value) => Ok(value),
            ComputedValue::Bytes(_)
            | ComputedValue::Uncompress(_)
            | ComputedValue::JsonReport(_)
            | ComputedValue::Ieee754Bits(_)
            | ComputedValue::Decimal(_)
            | ComputedValue::DecimalFast(_)
            | ComputedValue::DecimalDivision(_)
            | ComputedValue::Int128(_)
            | ComputedValue::NativeVector(_) => {
                self.inner.poisoned = true;
                Err(LocalError::InvalidBatch(
                    "evaluated ASCII requires an owned canonical Int result".into(),
                ))
            }
        }
    }
}

/// Borrows the worker, not one of its fields, so evaluation can keep using the
/// unchanged common driver. Drop precedes postflight on normal and error exits.
struct RegexpBindingGuard<'a> {
    worker: &'a mut EvaluatedBytesWorker,
}

impl Drop for RegexpBindingGuard<'_> {
    fn drop(&mut self) {
        if self
            .worker
            .regexp_metadata()
            .and_then(NativeRegexpCallMetadata::unbind)
            .is_err()
        {
            // A leftover binding/borrow also makes observe_storage refuse reuse.
            self.worker.poisoned = true;
        }
    }
}

struct TemporalBindingGuard<'a> {
    worker: &'a mut EvaluatedBytesWorker,
}

impl Drop for TemporalBindingGuard<'_> {
    fn drop(&mut self) {
        if self
            .worker
            .temporal_metadata()
            .and_then(NativeTemporalCallMetadata::unbind)
            .is_err()
        {
            self.worker.poisoned = true;
        }
    }
}

struct LikeBindingGuard<'a> {
    worker: &'a mut EvaluatedBytesWorker,
}

impl Drop for LikeBindingGuard<'_> {
    fn drop(&mut self) {
        if self
            .worker
            .like_metadata()
            .and_then(NativeLikeCallMetadata::unbind)
            .is_err()
        {
            self.worker.poisoned = true;
        }
    }
}

struct DecimalDivisionBindingGuard<'a> {
    worker: &'a mut EvaluatedBytesWorker,
}

impl Drop for DecimalDivisionBindingGuard<'_> {
    fn drop(&mut self) {
        if self
            .worker
            .decimal_division_metadata()
            .and_then(NativeDecimalDivisionCallMetadata::unbind)
            .is_err()
        {
            self.worker.poisoned = true;
        }
    }
}

impl EvaluatedBytesWorker {
    fn temporal_metadata(&self) -> LocalResult<&NativeTemporalCallMetadata> {
        if !self.operation.is_temporal_literal() {
            return Err(LocalError::InvalidSpec(
                "only temporal literals bind a session zone".into(),
            ));
        }
        let nodes: &[RpnExpressionNode] = self.program.expression.as_ref();
        match nodes.get(self.operation.input_types().len()) {
            Some(RpnExpressionNode::FnCall { metadata, .. }) => metadata
                .downcast_ref::<NativeTemporalCallMetadata>()
                .ok_or_else(|| LocalError::InvalidSpec("temporal call metadata changed".into())),
            _ => Err(LocalError::InvalidSpec(
                "temporal literal call is absent".into(),
            )),
        }
    }

    fn decimal_division_metadata(&self) -> LocalResult<&NativeDecimalDivisionCallMetadata> {
        let kind = self.operation.decimal_division_kind().ok_or_else(|| {
            LocalError::InvalidSpec("only Decimal division may bind its precision/report".into())
        })?;
        let nodes: &[RpnExpressionNode] = self.program.expression.as_ref();
        match nodes.get(self.operation.input_types().len()) {
            Some(RpnExpressionNode::FnCall { metadata, .. }) => metadata
                .downcast_ref::<NativeDecimalDivisionCallMetadata>()
                .filter(|payload| payload.kind == kind)
                .ok_or_else(|| {
                    LocalError::InvalidSpec("Decimal division metadata kind changed".into())
                }),
            _ => Err(LocalError::InvalidSpec(
                "Decimal division call is absent".into(),
            )),
        }
    }

    fn like_metadata(&self) -> LocalResult<&NativeLikeCallMetadata> {
        let kind = self.operation.like_kind().ok_or_else(|| {
            LocalError::InvalidSpec("only a LIKE recipe may bind LIKE metadata".into())
        })?;
        let nodes: &[RpnExpressionNode] = self.program.expression.as_ref();
        match nodes.get(self.operation.input_types().len()) {
            Some(RpnExpressionNode::FnCall { metadata, .. }) => metadata
                .downcast_ref::<NativeLikeCallMetadata>()
                .filter(|payload| payload.kind == kind)
                .ok_or_else(|| LocalError::InvalidSpec("LIKE metadata kind changed".into())),
            _ => Err(LocalError::InvalidSpec("LIKE call is absent".into())),
        }
    }

    fn regexp_metadata(&self) -> LocalResult<&NativeRegexpCallMetadata> {
        let kind = self.operation.regexp_kind().ok_or_else(|| {
            LocalError::InvalidSpec(
                "only a native regexp recipe may bind invocation metadata".into(),
            )
        })?;
        let nodes: &[RpnExpressionNode] = self.program.expression.as_ref();
        match nodes.get(self.operation.input_types().len()) {
            Some(RpnExpressionNode::FnCall { metadata, .. }) => metadata
                .downcast_ref::<NativeRegexpCallMetadata>()
                .filter(|payload| payload.kind == kind)
                .ok_or_else(|| LocalError::InvalidSpec("regexp metadata kind changed".into())),
            _ => Err(LocalError::InvalidSpec("regexp call is absent".into())),
        }
    }

    pub fn operation(&self) -> EvaluatedBytesOp {
        self.operation
    }

    pub fn kernel_invocations(&self) -> u64 {
        self.witness.invocations()
    }

    /// Includes sticky unwind/contract poison and unexpected retained-owner
    /// changes, even when the new allocation would fit the configured maximum.
    pub fn is_healthy(&self) -> bool {
        self.retained_storage().is_ok_and(|storage| {
            storage == self.accepted_storage
                && storage.total_bytes() <= self.max_worker_retained_bytes
        })
    }

    /// Observe ACTUAL current capacities without allocating a metadata cache or
    /// returning a cached size. Healthy but externally fault-injected spare
    /// capacity is observable; reuse still rejects any post-publication change.
    /// A dirty/poisoned context is refused, never reported as a partial count.
    pub fn retained_storage(&self) -> LocalResult<WorkerStorage> {
        if self.poisoned {
            return Err(LocalError::InvalidSpec(
                "evaluated ASCII worker is poisoned".into(),
            ));
        }
        self.observe_storage()
    }

    fn observe_storage(&self) -> LocalResult<WorkerStorage> {
        self.program.check_entry(self.operation.entry())?;
        if !evaluated_ascii_context_is_sealed(&self.ctx) {
            return Err(LocalError::InvalidSpec(
                "evaluated ASCII private context is not clean and sealed".into(),
            ));
        }
        let result_type = self.operation.return_type();
        let nodes: &[RpnExpressionNode] = self.program.expression.as_ref();
        if !matches!(nodes.last(), Some(RpnExpressionNode::FnCall { .. })) {
            return Err(LocalError::InvalidSpec(
                "evaluated ASCII worker no longer owns its fixed recipe".into(),
            ));
        }
        if self.program.host_catalog.is_some()
            || self.program.expression.checked_result_flow().is_some()
            || self.program.return_type() != &result_type
            || !evaluated_bytes_shape(self.operation, nodes, &self.program.schema)
        {
            return Err(LocalError::InvalidSpec(
                "evaluated ASCII worker ownership invariants changed".into(),
            ));
        }
        let metadata_bytes = self
            .program
            .expression
            .retained_metadata_heap_bytes()
            .ok_or_else(evaluated_ascii_storage_overflow)?;
        if metadata_bytes == 0 {
            return Err(LocalError::InvalidSpec(
                "evaluated ASCII worker metadata was not prewarmed".into(),
            ));
        }
        let payload_bytes = if self.operation.is_temporal_literal() {
            if !self.temporal_metadata()?.is_unbound() {
                return Err(LocalError::InvalidSpec(
                    "temporal worker retains a session zone".into(),
                ));
            }
            mem::size_of::<NativeTemporalCallMetadata>()
        } else if self.operation.regexp_kind().is_some() {
            let payload = self.regexp_metadata()?;
            if !payload.is_unbound() {
                return Err(LocalError::InvalidSpec(
                    "regexp worker retains invocation state".into(),
                ));
            }
            // The Box and its handle slots are fixed owned storage. Actual
            // statement cache use is separately recorded during an invocation;
            // opaque Regex/TLS storage is explicitly NOT measured as exact heap.
            mem::size_of::<NativeRegexpCallMetadata>()
        } else if self.operation.like_kind().is_some() {
            let payload = self.like_metadata()?;
            if !payload.is_unbound() {
                return Err(LocalError::InvalidSpec(
                    "LIKE worker retains invocation state".into(),
                ));
            }
            mem::size_of::<NativeLikeCallMetadata>()
        } else if self.operation.decimal_division_kind().is_some() {
            let payload = self.decimal_division_metadata()?;
            if !payload.is_unbound() {
                return Err(LocalError::InvalidSpec(
                    "Decimal division worker retains invocation state".into(),
                ));
            }
            mem::size_of::<NativeDecimalDivisionCallMetadata>()
        } else {
            0
        };
        // Canonical descriptors and unit Any remain heap-free by construction.
        let owned_heap_bytes = evaluated_ascii_owned_heap_bytes(
            self.program.expression.capacity(),
            self.program.schema.capacity(),
            metadata_bytes,
            self.ctx.warnings.warnings.capacity(),
        )?
        .checked_add(payload_bytes)
        .ok_or_else(evaluated_ascii_storage_overflow)?;
        WorkerStorage::new(mem::size_of::<Self>(), owned_heap_bytes)
    }

    fn check_owner_footprint(&self, storage: WorkerStorage) -> LocalResult<()> {
        if storage.total_bytes() > self.max_worker_retained_bytes {
            return Err(LocalError::ResourceLimit(
                "evaluated ASCII worker retained storage exceeded".into(),
            ));
        }
        if storage != self.accepted_storage {
            return Err(LocalError::InvalidSpec(
                "evaluated ASCII retained ownership changed after publication".into(),
            ));
        }
        Ok(())
    }

    fn begin_invocation(&mut self) -> LocalResult<()> {
        // Arm BEFORE any operation that could unwind. Only the normal checked
        // exit below disarms it; a caller catching a panic cannot reuse us.
        if self.poisoned {
            return Err(LocalError::InvalidSpec(
                "evaluated ASCII worker is poisoned".into(),
            ));
        }
        self.poisoned = true;
        self.check_owner_footprint(self.observe_storage()?)
    }

    /// Compatible entry for a single nullable Bytes argument. It rejects an
    /// operation with another input shape rather than coercing or inventing
    /// args.
    pub fn eval_one(&mut self, bytes: Option<Vec<u8>>) -> LocalResult<ComputedValue> {
        self.eval_args(EvaluatedArgs::Bytes(bytes))
    }

    /// Consume the complete ready argument shape selected by this worker's
    /// operation. Shape refusal is pure preflight; valid NULL arguments still
    /// reach the selected generated wrapper. Frontend demand/coercion order and
    /// return charset/type policy remain outside this owned-value boundary.
    pub fn eval_args(&mut self, args: EvaluatedArgs) -> LocalResult<ComputedValue> {
        self.eval_args_reported(args)
            .map_err(ReportedEvaluatedFailure::into_error)
    }

    /// The same single evaluation with a narrow, owned ABS/CONV/PERIOD/UUID
    /// failure receipt. Preparation, resource and output failures remain the
    /// original LocalError.
    pub fn eval_args_reported(
        &mut self,
        args: EvaluatedArgs,
    ) -> Result<ComputedValue, ReportedEvaluatedFailure> {
        // Preserve the semantic tag until after refusal. In particular, even
        // NULL or an eight-byte ordinary Bytes value cannot enter raw math.
        let source_types = if self.operation.is_decimal_int_div_budgeted() {
            &self.operation.input_types()[..self.operation.input_types().len() - 1]
        } else {
            self.operation.input_types()
        };
        if args.role() != self.operation.input_role()
            || args.input_types() != source_types
            || !args.admission_matches(self.operation)
        {
            return Err(ReportedEvaluatedFailure::unreported(
                LocalError::InvalidBatch(
                    "evaluated arguments differ from the operation's closed input shape".into(),
                ),
            ));
        }
        self.begin_invocation()
            .map_err(ReportedEvaluatedFailure::unreported)?;
        let mut sql_failure = None;
        let result = (|| {
            let materialization_available = if matches!(
                args.role(),
                EvaluatedArgsRole::DecimalUnary
                    | EvaluatedArgsRole::DecimalBinary
                    | EvaluatedArgsRole::DecimalDivision
                    | EvaluatedArgsRole::DecimalInt
                    | EvaluatedArgsRole::NativeVector
                    | EvaluatedArgsRole::NativeVector2
            ) || self.operation.is_decimal_int_div_budgeted()
            {
                // A real retained owner is already present even for inline or
                // NULL Decimal/vector input. Subtract it, not a guessed packet/scale
                // cap; a caller's usize::MAX limit still leaves finite room.
                self.state
                    .limits
                    .max_retained_bytes
                    .checked_sub(self.observe_storage()?.total_bytes())
                    .ok_or_else(evaluated_ascii_storage_overflow)?
            } else {
                0
            };
            // Invocation cloning shares live state, unlike owner cloning. This
            // reads no cache and adds no SQL slot; into_values moves the BBI owners.
            let like_invocation = match &args {
                EvaluatedArgs::Like { invocation, .. } => Some(invocation.clone()),
                _ => None,
            };
            let division_increment = match &args {
                EvaluatedArgs::DecimalDivision { frac_increment, .. } => Some(*frac_increment),
                _ => None,
            };
            let (ready, arity, invocation, temporal_zone) = match args {
                EvaluatedArgs::TemporalText { value, modes, zone } => {
                    let input_bytes = value
                        .capacity()
                        .checked_add(temporal_zone_heap_bytes(&zone))
                        .ok_or_else(evaluated_ascii_storage_overflow)?;
                    let bound = value
                        .len()
                        .checked_add(64)
                        .map(|bytes| bytes.max(11))
                        .ok_or_else(evaluated_ascii_storage_overflow)?;
                    // The name is an owned invocation input, not a SQL slot. Check
                    // it with the reply bound before installing or invoking anything.
                    EvalBudget::exact(self.state.limits)?.check_output(bound, input_bytes)?;
                    (
                        [
                            ScalarValue::Bytes(Some(value)),
                            ScalarValue::Int(Some(modes)),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                        ],
                        2,
                        None,
                        Some(zone),
                    )
                }
                args => {
                    let (ready, arity, invocation) =
                        args.into_values_for_operation(self.operation, materialization_available)?;
                    (ready, arity, invocation, None)
                }
            };
            if let Some(zone) = temporal_zone {
                // Install the cleanup guard before bind; unwind drops the kernel's
                // Ref first, then takes/drops the actual owned zone before lease finish.
                let guard = TemporalBindingGuard { worker: self };
                guard.worker.temporal_metadata()?.bind(zone)?;
                guard.worker.eval_ready(ready, arity, &mut sql_failure)
            } else if let Some(increment) = division_increment {
                let guard = DecimalDivisionBindingGuard { worker: self };
                guard.worker.decimal_division_metadata()?.bind(increment)?;
                let before = guard.worker.witness.invocations();
                let result = guard.worker.eval_ready(ready, arity, &mut sql_failure);
                let checked = guard.worker.decimal_division_metadata()?.finish(
                    guard.worker.witness.invocations().checked_sub(before),
                    result.is_ok(),
                );
                match result {
                    Err(primary) => Err(primary),
                    Ok(value) => checked.map(|_| value),
                }
                // The guard clears u32/status state before worker postflight.
            } else if let Some(invocation) = invocation {
                let input_bytes = ready[..arity]
                    .iter()
                    .try_fold(0usize, |sum, value| {
                        sum.checked_add(match value {
                            ScalarValue::Bytes(Some(value)) => value.capacity(),
                            _ => 0,
                        })
                    })
                    .ok_or_else(evaluated_ascii_storage_overflow)?;
                let limit = self
                    .state
                    .limits
                    .max_retained_bytes
                    .checked_sub(input_bytes)
                    .ok_or_else(evaluated_ascii_storage_overflow)?;
                // Arm the guard before binding, including any failing bind.
                let guard = RegexpBindingGuard { worker: self };
                guard.worker.regexp_metadata()?.bind(invocation, limit)?;
                guard.worker.eval_ready(ready, arity, &mut sql_failure)
                // The guard clears binding/record before finish_invocation.
            } else if let Some(invocation) = like_invocation {
                let input_bytes = ready[..arity]
                    .iter()
                    .try_fold(0usize, |sum, value| {
                        sum.checked_add(match value {
                            ScalarValue::Bytes(Some(value)) => value.capacity(),
                            _ => 0,
                        })
                    })
                    .ok_or_else(evaluated_ascii_storage_overflow)?;
                let limit = self
                    .state
                    .limits
                    .max_retained_bytes
                    .checked_sub(input_bytes)
                    // Every LIKE wrapper owns one Int result. Reserve its
                    // guaranteed minimum so even empty inputs/usize::MAX leave
                    // finite room; the driver checks actual result capacities.
                    .and_then(|limit| limit.checked_sub(int_min_storage_bytes(1)?))
                    .ok_or_else(evaluated_ascii_storage_overflow)?;
                let guard = LikeBindingGuard { worker: self };
                guard.worker.like_metadata()?.bind(invocation, limit)?;
                guard.worker.eval_ready(ready, arity, &mut sql_failure)
            } else {
                self.eval_ready(ready, arity, &mut sql_failure)
            }
        })();
        self.finish_invocation(result)
            .map_err(|error| ReportedEvaluatedFailure {
                error,
                operation: sql_failure.map(|_| self.operation),
                sql_failure,
            })
    }

    fn finish_invocation<T>(&mut self, result: LocalResult<T>) -> LocalResult<T> {
        // eval_ready has dropped both input and physical output on every normal
        // Result exit. No warning reset, context rebuild or native replay occurs.
        let postflight = self
            .observe_storage()
            .and_then(|storage| self.check_owner_footprint(storage));
        match result {
            Err(error) => {
                // A cleanup/health refusal must never replace an already-owned
                // primary error. It only prevents this worker from recycling.
                if postflight.is_ok()
                    && !matches!(
                        &error,
                        LocalError::InvalidSpec(_)
                            | LocalError::InvalidBatch(_)
                            | LocalError::BindingContract(_)
                            | LocalError::HostContract(_)
                    )
                {
                    self.poisoned = false;
                }
                Err(error)
            }
            Ok(value) => {
                postflight?;
                self.poisoned = false;
                Ok(value)
            }
        }
    }

    fn eval_ready(
        &mut self,
        ready: [ScalarValue; 6],
        arity: usize,
        sql_failure: &mut Option<EvaluatedSqlFailureKind>,
    ) -> LocalResult<ComputedValue> {
        let input_bytes = ready[..arity].iter().try_fold(0usize, |total, value| {
            let bytes = match value {
                ScalarValue::Bytes(Some(bytes)) => bytes.capacity(),
                ScalarValue::Decimal(Some(value)) => value.spill_capacity_bytes(),
                ScalarValue::VectorFloat32(Some(value)) => value.value.capacity(),
                _ => 0,
            };
            total
                .checked_add(bytes)
                .ok_or_else(evaluated_ascii_storage_overflow)
        })?;
        let input_bytes = if self.operation.is_temporal_literal() {
            let zone = self.temporal_metadata()?.zone()?;
            input_bytes
                .checked_add(temporal_zone_heap_bytes(&zone))
                .ok_or_else(evaluated_ascii_storage_overflow)?
        } else {
            input_bytes
        };
        self.state.row = [0];
        let mut budget = EvalBudget::exact(self.state.limits)?;
        if self.operation.is_temporal_literal() {
            budget.check_output(0, input_bytes)?;
        }
        let calls_before = self.witness.invocations();
        let result = self.program.expression.eval_with_ready_args(
            self.operation,
            &mut self.ctx,
            &self.program.schema,
            &ready[..arity],
            self.operation.input_role(),
            &self.state.row,
            &mut self.witness,
            &mut budget,
        );
        let division_status = if self.operation.decimal_division_kind().is_some() {
            match self.decimal_division_metadata()?.consume(
                self.witness.invocations().checked_sub(calls_before),
                result.is_ok(),
            ) {
                Ok(status) => status,
                Err(contract) => {
                    return Err(match result {
                        Err(primary) => primary,
                        Ok(_) => contract,
                    });
                }
            }
        } else {
            None
        };
        let input_bytes = if self.operation.regexp_kind().is_some() {
            let payload = self.regexp_metadata()?;
            let known = payload
                .known_cache_bytes
                .get()
                .ok_or_else(evaluated_ascii_storage_overflow)?;
            if known > payload.known_cache_limit.get() {
                return Err(evaluated_ascii_storage_overflow());
            }
            let observed = input_bytes
                .checked_add(known)
                .ok_or_else(evaluated_ascii_storage_overflow)?;
            // Check the exact use-site record even on a kernel error, before
            // authenticating any SQL cause. This also unwraps a recorded scope
            // refusal from the kernel's non-SQL Caused(LocalError) transport.
            budget.check_output(0, observed)?;
            observed
        } else if self.operation.like_kind().is_some() {
            let payload = self.like_metadata()?;
            let known = payload
                .known_cache_bytes
                .get()
                .ok_or_else(evaluated_ascii_storage_overflow)?;
            if known > payload.known_cache_limit.get() {
                return Err(evaluated_ascii_storage_overflow());
            }
            let observed = input_bytes
                .checked_add(known)
                .ok_or_else(evaluated_ascii_storage_overflow)?;
            // The actual wrapper records only known owned storage. No cache
            // peek, speculative compilation, or SQL error authentication here.
            budget.check_output(0, observed)?;
            observed
        } else {
            input_bytes
        };
        let result = result.map_err(|error| {
            // This exact closed recipe has one canonical generated wrapper.
            // Capture only its just-returned typed failure, not a later output
            // or cleanup failure, an input error, or an overflow-looking code.
            if calls_before.checked_add(1) == Some(self.witness.invocations()) {
                if let LocalError::Evaluation(cause) = &error {
                    *sql_failure = match (self.operation, cause.0.as_ref()) {
                        (operation, ErrorInner::Evaluate(EvaluateError::Caused(source)))
                            if source
                                .downcast_ref::<NativeAesError>()
                                .is_some_and(|cause| {
                                    operation.aes_error_profile()
                                        == Some((cause.operation(), cause.profile()))
                                }) =>
                        {
                            Some(EvaluatedSqlFailureKind::AesNative)
                        }
                        (operation, ErrorInner::Evaluate(EvaluateError::Caused(source)))
                            if source
                                .downcast_ref::<NativeBinaryArithmeticError>()
                                .is_some_and(|cause| {
                                    operation.native_binary_error_profile()
                                        == Some((cause.operation, cause.kind))
                                }) =>
                        {
                            Some(EvaluatedSqlFailureKind::BinaryArithmeticNative)
                        }
                        (operation, ErrorInner::Evaluate(EvaluateError::Caused(source)))
                            if source
                                .downcast_ref::<LegacyBinaryArithmeticError>()
                                .is_some_and(|cause| {
                                    operation.legacy_binary_error_profile()
                                        == Some((cause.operation, cause.unsigned))
                                }) =>
                        {
                            Some(EvaluatedSqlFailureKind::BinaryArithmeticLegacy)
                        }
                        (
                            EvaluatedBytesOp::UnaryMinusIntNative
                            | EvaluatedBytesOp::UnaryMinusUIntNative,
                            ErrorInner::Evaluate(EvaluateError::Caused(source)),
                        ) if source.downcast_ref::<NativeUnaryMinusError>().is_some_and(
                            |cause| {
                                cause.unsigned
                                    == (self.operation == EvaluatedBytesOp::UnaryMinusUIntNative)
                            },
                        ) =>
                        {
                            Some(EvaluatedSqlFailureKind::UnaryMinusNative)
                        }
                        (
                            EvaluatedBytesOp::RegexpLikeNative
                            | EvaluatedBytesOp::RegexpSubstrNative
                            | EvaluatedBytesOp::RegexpInstrNative
                            | EvaluatedBytesOp::RegexpReplaceNative,
                            ErrorInner::Evaluate(EvaluateError::Caused(source)),
                        ) if source.downcast_ref::<NativeRegexpError>().is_some() => {
                            Some(EvaluatedSqlFailureKind::RegexpNative)
                        }
                        (
                            EvaluatedBytesOp::VecFromTextNative
                            | EvaluatedBytesOp::VecL1DistanceNative
                            | EvaluatedBytesOp::VecL2DistanceNative
                            | EvaluatedBytesOp::VecNegativeInnerProductNative
                            | EvaluatedBytesOp::VecCosineDistanceNative
                            | EvaluatedBytesOp::AddVectorNative
                            | EvaluatedBytesOp::SubVectorNative
                            | EvaluatedBytesOp::MulVectorNative,
                            ErrorInner::Evaluate(EvaluateError::Caused(source)),
                        ) if source.downcast_ref::<NativeVectorError>().is_some() => {
                            Some(EvaluatedSqlFailureKind::VectorNative)
                        }
                        (
                            EvaluatedBytesOp::AbsIntNative,
                            ErrorInner::Evaluate(EvaluateError::AbsSignedOverflow { .. }),
                        ) => Some(EvaluatedSqlFailureKind::AbsSignedOverflow),
                        (
                            EvaluatedBytesOp::ConvNative
                            | EvaluatedBytesOp::ConvBinaryLiteralNative,
                            ErrorInner::Evaluate(EvaluateError::ConvUnsignedOverflow { .. }),
                        ) => Some(EvaluatedSqlFailureKind::ConvUnsignedOverflow),
                        (
                            EvaluatedBytesOp::PeriodAddNative,
                            ErrorInner::Evaluate(EvaluateError::PeriodAddIncorrectArguments),
                        ) => Some(EvaluatedSqlFailureKind::PeriodAddIncorrectArguments),
                        (
                            EvaluatedBytesOp::PeriodDiffNative,
                            ErrorInner::Evaluate(EvaluateError::PeriodDiffIncorrectArguments),
                        ) => Some(EvaluatedSqlFailureKind::PeriodDiffIncorrectArguments),
                        (
                            EvaluatedBytesOp::UuidToBinParseNative,
                            ErrorInner::Evaluate(EvaluateError::UuidToBinWhitespace),
                        ) => Some(EvaluatedSqlFailureKind::UuidToBinWhitespace),
                        (
                            EvaluatedBytesOp::UuidToBinParseNative,
                            ErrorInner::Evaluate(EvaluateError::UuidToBinInvalid),
                        ) => Some(EvaluatedSqlFailureKind::UuidToBinInvalid),
                        (
                            EvaluatedBytesOp::UuidVersionNative,
                            ErrorInner::Evaluate(EvaluateError::UuidVersionInvalid),
                        ) => Some(EvaluatedSqlFailureKind::UuidVersionInvalid),
                        (
                            EvaluatedBytesOp::UuidTimestampNative,
                            ErrorInner::Evaluate(EvaluateError::UuidTimestampInvalid),
                        ) => Some(EvaluatedSqlFailureKind::UuidTimestampInvalid),
                        (
                            EvaluatedBytesOp::BinToUuidNative,
                            ErrorInner::Evaluate(EvaluateError::BinToUuidInvalidLength { .. }),
                        ) => Some(EvaluatedSqlFailureKind::BinToUuidInvalidLength),
                        _ => None,
                    };
                }
            }
            error
        })?;
        let output = match result {
            RpnStackNode::Vector {
                value: RpnStackNodeVectorValue::Generated { physical_value },
                field_type,
            } if field_type == &self.operation.return_type() => physical_value,
            _ => {
                let message = if self.operation == EvaluatedBytesOp::Ascii {
                    "evaluated ASCII requires an owned canonical Int result"
                } else {
                    "evaluated Bytes requires an owned canonical result"
                };
                return Err(LocalError::InvalidBatch(message.into()));
            }
        };
        if output.eval_type() != self.operation.eval_type() || output.len() != 1 {
            let message = if self.operation == EvaluatedBytesOp::Ascii {
                "evaluated ASCII returned an invalid singleton Int result"
            } else {
                "evaluated Bytes returned an invalid singleton result"
            };
            return Err(LocalError::InvalidBatch(message.into()));
        }
        if let Some(status) = division_status {
            let ScalarValueRef::Decimal(value) = output.get_scalar_ref(0) else {
                return Err(self.decimal_division_metadata()?.refuse());
            };
            if value.is_none() != (status == NativeDecimalDivisionDisposition::ZeroDivisor) {
                return Err(self.decimal_division_metadata()?.refuse());
            }
        }
        // TaskGuard is gone, but the ready owner remains live. Bytes extraction
        // additionally holds the generated data/offset/bitmap buffers AND the
        // new owned Vec; precheck requested length, then check actual capacity.
        let output_bytes = vector_storage_bytes(&output, budget.mode());
        budget.check_output(output_bytes, input_bytes)?;
        let (value, retained_bytes) = match output.get_scalar_ref(0) {
            ScalarValueRef::Int(value) => (
                ComputedValue::Int(ComputedInt {
                    value: value.copied(),
                }),
                0,
            ),
            ScalarValueRef::Decimal(value) => {
                let checked_i64_view = if matches!(
                    self.operation,
                    EvaluatedBytesOp::CeilDecimalNative
                        | EvaluatedBytesOp::FloorDecimalNative
                        | EvaluatedBytesOp::UnaryMinusIntConstantNative
                        | EvaluatedBytesOp::UnaryMinusUIntConstantNative
                ) {
                    value.and_then(|value| match value.as_i64() {
                        tidb_query_datatype::codec::mysql::decimal::Res::Ok(value) => Some(value),
                        _ => None,
                    })
                } else {
                    None
                };
                let value = match value {
                    None => None,
                    Some(source) => {
                        let overlap = output_bytes
                            .checked_add(source.spill_capacity_bytes())
                            .ok_or_else(evaluated_ascii_storage_overflow)?;
                        budget.check_output(overlap, input_bytes)?;
                        let limit = self
                            .state
                            .limits
                            .max_retained_bytes
                            .checked_sub(input_bytes)
                            .and_then(|remaining| remaining.checked_sub(output_bytes))
                            .ok_or_else(evaluated_ascii_storage_overflow)?;
                        let owned = source
                            .try_clone_native_math(limit)
                            .map_err(native_decimal_bridge_error)?;
                        let overlap = output_bytes
                            .checked_add(owned.spill_capacity_bytes())
                            .ok_or_else(evaluated_ascii_storage_overflow)?;
                        budget.check_output(overlap, input_bytes)?;
                        Some(owned)
                    }
                };
                let retained = value.as_ref().map_or(0, Decimal::spill_capacity_bytes);
                let value = match division_status {
                    Some(disposition) => ComputedValue::DecimalDivision(ComputedDecimalDivision {
                        value,
                        disposition,
                    }),
                    None => ComputedValue::Decimal(ComputedDecimal {
                        value,
                        checked_i64_view,
                    }),
                };
                (value, retained)
            }
            ScalarValueRef::Bytes(value) if self.operation.returns_decimal_fast() => {
                let outcome = crate::impl_arithmetic::decode_native_decimal_fast_outcome(value)
                    .map_err(|error| {
                        LocalError::InvalidBatch(format!("invalid decimal fast result: {error}"))
                    })?;
                // Only the inline actual outcome survives; the physical encoded
                // result stays charged by output_bytes until the vector drops.
                (
                    ComputedValue::DecimalFast(ComputedDecimalFast { outcome }),
                    0,
                )
            }
            ScalarValueRef::Bytes(value)
                if self.operation == EvaluatedBytesOp::RoundInt128Legacy
                    || self.operation.is_binary_int128() =>
            {
                let value = value
                    .map(|source| {
                        <[u8; 16]>::try_from(source)
                            .map(i128::from_le_bytes)
                            .map_err(|_| {
                                LocalError::InvalidBatch(
                                    "Int128 result transport must contain exactly sixteen bytes"
                                        .into(),
                                )
                            })
                    })
                    .transpose()?;
                (ComputedValue::Int128(ComputedInt128 { value }), 0)
            }
            ScalarValueRef::Bytes(value)
                if matches!(
                    self.operation,
                    EvaluatedBytesOp::VecFromTextNative
                        | EvaluatedBytesOp::AddVectorNative
                        | EvaluatedBytesOp::SubVectorNative
                        | EvaluatedBytesOp::MulVectorNative
                ) =>
            {
                let value = value
                    .map(|source| {
                        materialize_native_vector(source, output_bytes, input_bytes, &budget)
                    })
                    .transpose()?;
                let retained = value
                    .as_ref()
                    .map_or(0, NativeVectorFloat32::elements_capacity)
                    .checked_mul(mem::size_of::<f32>())
                    .ok_or_else(evaluated_ascii_storage_overflow)?;
                (
                    ComputedValue::NativeVector(ComputedNativeVector { value }),
                    retained,
                )
            }
            ScalarValueRef::Bytes(value) if self.operation.returns_ieee754_bits() => {
                // The physical vector and input remain charged above while we
                // copy an inline bit owner. No Bytes/SQL-Int result escapes.
                let value = value
                    .map(|source| {
                        <[u8; 8]>::try_from(source)
                            .map(u64::from_le_bytes)
                            .map_err(|_| {
                                LocalError::InvalidBatch(
                                    "IEEE754 result transport must contain exactly eight bytes"
                                        .into(),
                                )
                            })
                    })
                    .transpose()?;
                (ComputedValue::Ieee754Bits(ComputedIeee754Bits { value }), 0)
            }
            ScalarValueRef::Bytes(value)
                if self.operation == EvaluatedBytesOp::UncompressNative =>
            {
                let outcome = match decode_uncompress_frame(value)? {
                    UncompressFrame::Null => UncompressOutcome::Null,
                    UncompressFrame::Corrupt => UncompressOutcome::Corrupt,
                    UncompressFrame::OutputLimit => UncompressOutcome::OutputLimit,
                    UncompressFrame::Value(source) => {
                        // The encoded physical output, including its tag, stays
                        // charged while only the decoded payload gets an owner.
                        let overlap = output_bytes
                            .checked_add(source.len())
                            .ok_or_else(evaluated_ascii_storage_overflow)?;
                        budget.check_output(overlap, input_bytes)?;
                        let mut owned = Vec::new();
                        owned.try_reserve_exact(source.len()).map_err(|_| {
                            LocalError::ResourceLimit("UNCOMPRESS result allocation failed".into())
                        })?;
                        let overlap = output_bytes
                            .checked_add(owned.capacity())
                            .ok_or_else(evaluated_ascii_storage_overflow)?;
                        budget.check_output(overlap, input_bytes)?;
                        owned.extend_from_slice(source);
                        UncompressOutcome::Value(owned)
                    }
                };
                let retained = match &outcome {
                    UncompressOutcome::Value(value) => value.capacity(),
                    _ => 0,
                };
                (
                    ComputedValue::Uncompress(ComputedUncompress { outcome }),
                    retained,
                )
            }
            ScalarValueRef::Bytes(value) if self.operation.returns_json_report() => {
                let outcome = match decode_json_report_frame(self.operation, value)? {
                    JsonReportFrame::Null => JsonReportOutcome::Null,
                    JsonReportFrame::Int(value) => JsonReportOutcome::Int(value),
                    JsonReportFrame::EmptyText => JsonReportOutcome::EmptyText,
                    JsonReportFrame::InvalidText => JsonReportOutcome::InvalidText,
                    JsonReportFrame::Bytes(source) => {
                        // Only the TYPE payload is copied. The encoded result
                        // (including its tag) and ready owner remain charged.
                        let overlap = output_bytes
                            .checked_add(source.len())
                            .ok_or_else(evaluated_ascii_storage_overflow)?;
                        budget.check_output(overlap, input_bytes)?;
                        let mut owned = Vec::new();
                        owned.try_reserve_exact(source.len()).map_err(|_| {
                            LocalError::ResourceLimit(
                                "JSON report payload allocation failed".into(),
                            )
                        })?;
                        let overlap = output_bytes
                            .checked_add(owned.capacity())
                            .ok_or_else(evaluated_ascii_storage_overflow)?;
                        budget.check_output(overlap, input_bytes)?;
                        owned.extend_from_slice(source);
                        JsonReportOutcome::Bytes(owned)
                    }
                };
                let retained = match &outcome {
                    JsonReportOutcome::Bytes(value) => value.capacity(),
                    _ => 0,
                };
                (
                    ComputedValue::JsonReport(ComputedJsonReport { outcome }),
                    retained,
                )
            }
            ScalarValueRef::Bytes(value) => {
                if self.operation == EvaluatedBytesOp::ConvertTzNative
                    && value.is_some_and(|bytes| std::str::from_utf8(bytes).is_err())
                {
                    return Err(LocalError::InvalidBatch(
                        "native CONVERT_TZ returned invalid UTF-8".into(),
                    ));
                }
                if self.operation.is_temporal_literal()
                    && value.is_none_or(|bytes| {
                        crate::decode_native_temporal_literal_result(bytes).is_none()
                    })
                {
                    return Err(LocalError::InvalidBatch(
                        "temporal literal returned NULL or an invalid reply".into(),
                    ));
                }
                if self.operation == EvaluatedBytesOp::TimestampAddNative
                    && value.is_some_and(|bytes| !crate::native_timestamp_add_result_valid(bytes))
                {
                    return Err(LocalError::InvalidBatch(
                        "native TIMESTAMPADD returned an invalid outcome packet".into(),
                    ));
                }
                if matches!(
                    self.operation,
                    EvaluatedBytesOp::AddTimeNative | EvaluatedBytesOp::SubTimeNative
                ) && value.is_some_and(|bytes| !crate::native_time_add_result_valid(bytes))
                {
                    return Err(LocalError::InvalidBatch(
                        "native ADDTIME/SUBTIME returned an invalid warning/value packet".into(),
                    ));
                }
                if self.operation == EvaluatedBytesOp::TimeNative
                    && value.is_some_and(|bytes| !crate::native_time_result_valid(bytes))
                {
                    return Err(LocalError::InvalidBatch(
                        "native TIME returned an invalid status/text packet".into(),
                    ));
                }
                if self.operation.is_native_decimal_int_div()
                    && !value.is_some_and(crate::native_intdiv_result_valid)
                {
                    return Err(LocalError::InvalidBatch(
                        "native Decimal DIV returned an invalid complete report".into(),
                    ));
                }
                let value = match value {
                    None => None,
                    Some(source) => {
                        let overlap = output_bytes
                            .checked_add(source.len())
                            .ok_or_else(evaluated_ascii_storage_overflow)?;
                        budget.check_output(overlap, input_bytes)?;
                        let mut owned = Vec::new();
                        owned.try_reserve_exact(source.len()).map_err(|_| {
                            LocalError::ResourceLimit(
                                "evaluated Bytes result allocation failed".into(),
                            )
                        })?;
                        let overlap = output_bytes
                            .checked_add(owned.capacity())
                            .ok_or_else(evaluated_ascii_storage_overflow)?;
                        budget.check_output(overlap, input_bytes)?;
                        owned.extend_from_slice(source);
                        Some(owned)
                    }
                };
                let retained = value.as_ref().map_or(0, Vec::capacity);
                (ComputedValue::Bytes(ComputedBytes { value }), retained)
            }
            _ => unreachable!("the closed result carrier was checked"),
        };
        drop(output);
        drop(ready);
        budget.set_output_bytes(retained_bytes)?;
        Ok(value)
    }
}

#[cfg(test)]
mod evaluated_ascii_tests {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::Mutex,
        thread,
    };

    use tidb_query_datatype::expr::{Flag, SqlMode};

    use super::*;
    use crate::local::{LiteralKind, LocalExpr, compile_local};

    #[test]
    fn json_search_profile_preserves_actual_specs_ordered_scopes_and_null_results() {
        let operation = EvaluatedBytesOp::JsonSearchSerdeNative;
        let getter = crate::impl_json::json_search_native_fn_meta();
        assert_eq!(
            operation.input_types(),
            &[EvalType::Bytes, EvalType::Bytes, EvalType::Bytes]
        );
        assert_eq!(operation.input_role(), EvaluatedArgsRole::Values);
        assert_eq!(operation.eval_type(), EvalType::Bytes);
        let program = compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
        assert_eq!(program.expression.len(), 4);
        assert!(program.check_entry(ProgramEntry::Row).is_err());
        let RpnExpressionNode::FnCall {
            func_meta,
            metadata,
            args_len,
            ..
        } = &program.expression[3]
        else {
            panic!()
        };
        assert_eq!(*args_len, 3);
        assert!(metadata.is::<()>());
        assert_eq!(func_meta.name, getter.name);
        assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
        assert!(std::ptr::fn_addr_eq(
            func_meta.validator_ptr,
            getter.validator_ptr
        ));
        assert!(std::ptr::fn_addr_eq(
            func_meta.metadata_ptr,
            getter.metadata_ptr
        ));
        let spec = LocalExpr::Call {
            function: operation.function_ref(),
            args: program
                .schema
                .iter()
                .enumerate()
                .map(|(slot, field_type)| LocalExpr::InputSlot {
                    slot,
                    field_type: field_type.clone(),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            return_type: operation.return_type(),
            metadata: crate::CallMetadata::None,
        };
        assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
        let document = serde_json::json!(["needle", "other", "needle"]);
        let ordered = [
            crate::parse_native_json_path("$[2]").unwrap(),
            crate::parse_native_json_path("$[0]").unwrap(),
            crate::parse_native_json_path("$[2]").unwrap(),
        ];
        let scalar = serde_json::json!("needle");
        let null = serde_json::Value::Null;
        let escaped = serde_json::json!(["a%b", "acb"]);
        let index = [crate::parse_native_json_path("$[0]").unwrap()];
        let EvaluatedArgs::Bytes3(valid) =
            prepare_json_search_args(&document, &ordered, true, "needle", '\\').unwrap()
        else {
            panic!()
        };
        let EvaluatedArgs::Bytes2(old_document, old_paths) =
            prepare_json_paths_args(&document, &ordered).unwrap()
        else {
            panic!()
        };
        assert_eq!(valid[0], old_document);
        assert_eq!(valid[1], old_paths);
        assert_eq!(
            crate::native_json_search::decode_native_json_search_spec(valid[2].as_deref().unwrap()),
            Some((true, "needle", '\\'))
        );
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        let storage = worker.retained_storage().unwrap();
        let mut malformed = Vec::new();
        for slot in 0..3 {
            let mut bad = valid.clone();
            bad[slot] = None;
            malformed.push(bad);
        }
        for (slot, bytes) in [
            (0, b"[".to_vec()),
            (1, vec![1]),
            (2, vec![]),
            (2, vec![2, 0, 0, 0, 0]),
            (2, vec![1, 0, 216, 0, 0]),
            (2, vec![0, 92, 0, 0, 0, 255]),
        ] {
            let mut bad = valid.clone();
            bad[slot] = Some(bytes);
            malformed.push(bad);
        }
        for bad in malformed {
            let mut ready = std::array::from_fn(|_| ScalarValue::Int(None));
            for slot in 0..3 {
                ready[slot] = ScalarValue::Bytes(bad[slot].clone());
            }
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::Bytes3(bad)),
                Err(LocalError::InvalidBatch(_))
            ));
            assert!(matches!(
                worker.eval_ready(ready, 3, &mut None),
                Err(LocalError::InvalidSpec(_))
            ));
        }
        for invalid in [
            EvaluatedArgs::NoArgs,
            EvaluatedArgs::NullWitness(None),
            EvaluatedArgs::Bytes(None),
            EvaluatedArgs::Bytes2(valid[0].clone(), valid[1].clone()),
            EvaluatedArgs::BytesBytesInt(valid[0].clone(), valid[1].clone(), Some(0)),
        ] {
            assert!(matches!(
                worker.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
        }
        for arity in [0, 1, 2, 4] {
            let mut ready = std::array::from_fn(|_| ScalarValue::Int(None));
            for slot in 0..3 {
                ready[slot] = ScalarValue::Bytes(valid[slot].clone());
            }
            assert!(matches!(
                worker.eval_ready(ready, arity, &mut None),
                Err(LocalError::InvalidSpec(_))
            ));
        }
        assert_eq!(worker.kernel_invocations(), 0);
        let cases: &[(
            &serde_json::Value,
            &[crate::NativeJsonPath],
            bool,
            &str,
            char,
            Option<&str>,
        )] = &[
            (&document, &[], true, "needle", '\\', Some(r#""$[0]""#)),
            (
                &document,
                &[],
                false,
                "needle",
                '\\',
                Some(r#"["$[0]", "$[2]"]"#),
            ),
            (&document, &ordered, true, "needle", '\\', Some(r#""$[2]""#)),
            (
                &document,
                &ordered,
                false,
                "needle",
                '\\',
                Some(r#"["$[2]", "$[0]"]"#),
            ),
            (&document, &[], true, "missing", '\\', None),
            (&null, &[], true, "%", '\\', None),
            (&scalar, &index, true, "needle", '\\', None), /* SEARCH must not use EXTRACT's
                                                            * scalar auto-wrap. */
            (&escaped, &[], true, "a界%b", '界', Some(r#""$[0]""#)),
        ];
        for (calls, &(document, paths, one, pattern, escape, expected)) in (1_u64..).zip(cases) {
            let args = prepare_json_search_args(document, paths, one, pattern, escape).unwrap();
            assert!(args.admission_matches(operation));
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!()
            };
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(value.value(), expected.map(str::as_bytes));
            assert_eq!(worker.kernel_invocations(), calls);
            assert_eq!(worker.retained_storage().unwrap(), storage);
            assert!(worker.is_healthy());
        }
        let mut zero = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits {
                max_steps: 0,
                ..ExecutionLimits::default()
            },
            usize::MAX,
        )
        .unwrap();
        let failure = zero
            .eval_args_reported(
                prepare_json_search_args(&document, &[], true, "missing", '\\').unwrap(),
            )
            .unwrap_err();
        assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
        assert_eq!(failure.sql_failure(), None);
        assert_eq!(zero.kernel_invocations(), 0);
        assert!(zero.is_healthy());
    }

    #[test]
    fn timestamp_add_profiles_preserve_actual_prefixes_ieee_bits_and_reports() {
        use crate::NativeTimestampAddResult as Report;
        let one = 1.0_f64.to_bits() as i64;
        let nan = 0x7ff8_0000_0000_0042_i64;
        let main = |unit: Option<&str>, date: Option<&str>, amount| {
            EvaluatedArgs::BytesBytesInt(
                unit.map(|value| value.as_bytes().to_vec()),
                date.map(|value| value.as_bytes().to_vec()),
                amount,
            )
        };
        for (operation, getter) in [
            (
                EvaluatedBytesOp::TimestampAddNative,
                crate::impl_time::timestamp_add_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::TimestampAddPrefixNullNative,
                crate::impl_time::timestamp_add_prefix_null_native_fn_meta(),
            ),
        ] {
            let prefix = operation == EvaluatedBytesOp::TimestampAddPrefixNullNative;
            let arity = if prefix { 2 } else { 3 };
            assert_eq!(operation.input_role(), EvaluatedArgsRole::Values);
            assert_eq!(operation.input_types().len(), arity);
            assert_eq!(operation.eval_type(), EvalType::Bytes);
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), arity + 1);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[arity]
            else {
                panic!()
            };
            assert_eq!(*args_len, arity);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            let invalid = if prefix {
                vec![
                    EvaluatedArgs::BytesInt(Some(b"DAY".to_vec()), Some(one)),
                    EvaluatedArgs::BytesInt(Some(b"DAY".to_vec()), Some(nan)),
                    EvaluatedArgs::BytesInt(Some(vec![255]), None),
                ]
            } else {
                vec![
                    main(None, Some("2020-01-01"), Some(one)),
                    main(Some("DAY"), None, None),
                    EvaluatedArgs::BytesBytesInt(Some(vec![255]), None, Some(one)),
                    EvaluatedArgs::BytesBytesInt(Some(b"DAY".to_vec()), Some(vec![255]), Some(one)),
                ]
            };
            for invalid in invalid {
                let mut ready = std::array::from_fn(|_| ScalarValue::Int(None));
                match &invalid {
                    EvaluatedArgs::BytesInt(unit, amount) => {
                        ready[0] = ScalarValue::Bytes(unit.clone());
                        ready[1] = ScalarValue::Int(*amount);
                    }
                    EvaluatedArgs::BytesBytesInt(unit, date, amount) => {
                        ready[0] = ScalarValue::Bytes(unit.clone());
                        ready[1] = ScalarValue::Bytes(date.clone());
                        ready[2] = ScalarValue::Int(*amount);
                    }
                    _ => unreachable!(),
                }
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
                assert!(matches!(
                    worker.eval_ready(ready, arity, &mut None),
                    Err(LocalError::InvalidSpec(_))
                ));
            }
            let wrong = if prefix {
                main(None, None, Some(nan))
            } else {
                EvaluatedArgs::BytesInt(None, Some(nan))
            };
            for invalid in [
                wrong,
                EvaluatedArgs::NoArgs,
                EvaluatedArgs::NullWitness(None),
                EvaluatedArgs::Bytes(None),
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 0);
            let mut cases = if prefix {
                vec![
                    (EvaluatedArgs::BytesInt(Some(b"DAY".to_vec()), None), None),
                    (EvaluatedArgs::BytesInt(None, None), None),
                    (EvaluatedArgs::BytesInt(Some(vec![]), None), None),
                ]
            } else {
                vec![
                    (main(Some("DAY"), None, Some(one)), None),
                    (
                        main(Some("DAY"), Some("2020-01-01"), Some(one)),
                        Some(Report::Value("2020-01-02 00:00:00")),
                    ),
                    (
                        main(
                            Some("MINUTE"),
                            Some("2020-01-01"),
                            Some(1.5_f64.to_bits() as i64),
                        ),
                        Some(Report::Value("2020-01-01 00:02:00")),
                    ),
                    (
                        main(
                            Some("SECOND"),
                            Some("2020-01-01"),
                            Some(0.0000099999_f64.to_bits() as i64),
                        ),
                        Some(Report::Value("2020-01-01 00:00:00.000009")),
                    ),
                    (
                        main(Some("unknown"), Some("2020-01-01"), Some(nan)),
                        Some(Report::UnknownUnit),
                    ),
                    (
                        main(Some("unknown"), Some("bad"), Some(nan)),
                        Some(Report::IncorrectDateTimeInput),
                    ),
                    (
                        main(Some("DAY"), Some("9999-12-31"), Some(one)),
                        Some(Report::IncorrectTimeResult(
                            "Incorrect time value: '{10000 1 1 0 0 0 0}'",
                        )),
                    ),
                ]
            };
            for (bits, unchanged) in [
                (0, true),
                (i64::MIN, true),
                (1, true),
                (i64::MAX, false),
                (-1, false),
                (nan, false),
                (f64::INFINITY.to_bits() as i64, false),
                (f64::NEG_INFINITY.to_bits() as i64, false),
            ] {
                cases.push(if prefix {
                    (EvaluatedArgs::BytesInt(None, Some(bits)), None)
                } else {
                    (
                        main(Some("DAY"), Some("2020-01-01"), Some(bits)),
                        unchanged.then_some(Report::Value("2020-01-01 00:00:00")),
                    )
                });
            }
            if !prefix {
                cases.push((
                    main(Some("DAY"), Some("2020-01-01"), Some(one)),
                    Some(Report::Value("2020-01-02 00:00:00")),
                ));
            }
            for (calls, (args, expected)) in (1_u64..).zip(cases) {
                assert!(args.admission_matches(operation));
                let ComputedValue::Bytes(value) = worker.eval_args_reported(args).unwrap() else {
                    panic!()
                };
                assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
                let decoded = value.value().map(|bytes| {
                    assert!(crate::native_timestamp_add_result_valid(bytes));
                    crate::decode_native_timestamp_add_result(bytes).unwrap()
                });
                assert_eq!(decoded, expected);
                assert_eq!(worker.kernel_invocations(), calls);
                assert_eq!(worker.retained_storage().unwrap(), storage);
                assert!(worker.is_healthy());
            }
            let mut zero = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits {
                    max_steps: 0,
                    ..ExecutionLimits::default()
                },
                usize::MAX,
            )
            .unwrap();
            let args = if prefix {
                EvaluatedArgs::BytesInt(None, Some(nan))
            } else {
                main(Some("DAY"), Some("2020-01-01"), Some(one))
            };
            let failure = zero.eval_args_reported(args).unwrap_err();
            assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
            assert_eq!(failure.sql_failure(), None);
            assert_eq!(zero.kernel_invocations(), 0);
            assert!(zero.is_healthy());
        }
        for invalid in [
            b"".as_slice(),
            &[0],
            &[3],
            &[0, 255],
            &[3, 255],
            &[1, b'x'],
            &[2, b'x'],
            &[4],
        ] {
            assert!(!crate::native_timestamp_add_result_valid(invalid));
        }
    }

    #[test]
    fn time_add_profiles_preserve_metadata_nullable_values_warning_packets_and_reuse() {
        use crate::{
            NativeTimeAddKind as Kind, NativeTimeAddMetadata as Metadata,
            NativeTimeAddResult as ResultValue, NativeTimeAddWarning as Warning,
        };
        let metadata = Metadata {
            left: Kind::Duration,
            right: Kind::Duration,
            row_path: false,
            right_binary: false,
        };
        let normal = metadata.encode();
        let right_datetime = Metadata {
            right: Kind::Datetime,
            ..metadata
        }
        .encode();
        let args = |left: Option<&str>, right: Option<&str>, metadata| {
            EvaluatedArgs::BytesBytesInt(
                left.map(|text| text.as_bytes().to_vec()),
                right.map(|text| text.as_bytes().to_vec()),
                metadata,
            )
        };
        for (operation, getter) in [
            (
                EvaluatedBytesOp::AddTimeNative,
                crate::impl_time::add_time_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::SubTimeNative,
                crate::impl_time::sub_time_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::TimeAddRightDatetimeNative,
                crate::impl_time::time_add_right_datetime_native_fn_meta(),
            ),
        ] {
            let metadata_only = operation == EvaluatedBytesOp::TimeAddRightDatetimeNative;
            let arity = if metadata_only { 1 } else { 3 };
            assert_eq!(operation.input_role(), EvaluatedArgsRole::Values);
            assert_eq!(operation.eval_type(), EvalType::Bytes);
            assert_eq!(operation.input_types().len(), arity);
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), arity + 1);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[arity]
            else {
                panic!()
            };
            assert_eq!(*args_len, arity);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            let invalid = if metadata_only {
                vec![
                    EvaluatedArgs::Int(None),
                    EvaluatedArgs::Int(Some(-1)),
                    EvaluatedArgs::Int(Some(64)),
                    EvaluatedArgs::Int(Some(normal)),
                ]
            } else {
                vec![
                    args(None, None, None),
                    args(None, None, Some(-1)),
                    args(None, None, Some(64)),
                    args(None, None, Some(right_datetime)),
                    EvaluatedArgs::BytesBytesInt(
                        Some(vec![255]),
                        Some(b"00:00:01".to_vec()),
                        Some(normal),
                    ),
                    EvaluatedArgs::BytesBytesInt(
                        Some(b"01:02:03".to_vec()),
                        Some(vec![255]),
                        Some(normal),
                    ),
                ]
            };
            for invalid in invalid {
                let mut ready = std::array::from_fn(|_| ScalarValue::Int(None));
                match &invalid {
                    EvaluatedArgs::Int(metadata) => ready[0] = ScalarValue::Int(*metadata),
                    EvaluatedArgs::BytesBytesInt(left, right, metadata) => {
                        ready[0] = ScalarValue::Bytes(left.clone());
                        ready[1] = ScalarValue::Bytes(right.clone());
                        ready[2] = ScalarValue::Int(*metadata);
                    }
                    _ => unreachable!(),
                }
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
                assert!(matches!(
                    worker.eval_ready(ready, arity, &mut None),
                    Err(LocalError::InvalidSpec(_))
                ));
            }
            let wrong = if metadata_only {
                args(None, None, Some(right_datetime))
            } else {
                EvaluatedArgs::Int(Some(right_datetime))
            };
            for invalid in [
                wrong,
                EvaluatedArgs::NoArgs,
                EvaluatedArgs::NullWitness(None),
                EvaluatedArgs::Bytes(None),
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 0);
            let value = if operation == EvaluatedBytesOp::SubTimeNative {
                "01:02:02"
            } else {
                "01:02:04"
            };
            let cases = if metadata_only {
                vec![
                    (EvaluatedArgs::Int(Some(right_datetime)), None),
                    (EvaluatedArgs::Int(Some(right_datetime)), None),
                ]
            } else {
                vec![
                    (args(None, Some("00:00:01"), Some(normal)), None),
                    (args(Some("01:02:03"), None, Some(normal)), None),
                    (
                        args(Some("01:02:03"), Some("00:00:01"), Some(normal)),
                        Some(ResultValue::Value(value)),
                    ),
                    (
                        args(Some("bad"), Some("00:00:01"), Some(normal)),
                        Some(ResultValue::Warning(Warning::TruncatedLeft)),
                    ),
                    (
                        args(Some("01:02:03"), Some("bad"), Some(normal)),
                        Some(ResultValue::Warning(Warning::TruncatedRight)),
                    ),
                    (
                        args(Some("01:02:03"), Some("00:00:01"), Some(normal)),
                        Some(ResultValue::Value(value)),
                    ),
                ]
            };
            for (calls, (args, expected)) in (1_u64..).zip(cases) {
                let ComputedValue::Bytes(value) = worker.eval_args_reported(args).unwrap() else {
                    panic!()
                };
                assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
                let decoded = value.value().map(|bytes| {
                    assert!(crate::native_time_add_result_valid(bytes));
                    crate::decode_native_time_add_result(bytes).unwrap()
                });
                assert_eq!(decoded, expected);
                assert_eq!(worker.kernel_invocations(), calls);
                assert_eq!(worker.retained_storage().unwrap(), storage);
                assert!(worker.is_healthy());
            }
            let mut zero = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits {
                    max_steps: 0,
                    ..ExecutionLimits::default()
                },
                usize::MAX,
            )
            .unwrap();
            let args = if metadata_only {
                EvaluatedArgs::Int(Some(right_datetime))
            } else {
                args(Some("01:02:03"), Some("00:00:01"), Some(normal))
            };
            let failure = zero.eval_args_reported(args).unwrap_err();
            assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
            assert_eq!(failure.sql_failure(), None);
            assert_eq!(zero.kernel_invocations(), 0);
            assert!(zero.is_healthy());
        }
        for invalid in [b"".as_slice(), &[0], &[0, 255], &[1, b'x'], &[5]] {
            assert!(!crate::native_time_add_result_valid(invalid));
        }
    }

    #[test]
    fn time_and_microsecond_profiles_keep_actual_nullable_inputs_and_reuse() {
        let text = |value: Option<&str>| {
            EvaluatedArgs::Bytes(value.map(|value| value.as_bytes().to_vec()))
        };
        for (operation, getter) in [
            (
                EvaluatedBytesOp::TimeNative,
                crate::impl_time::time_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::MicrosecondNative,
                crate::impl_time::microsecond_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::MicrosecondLegacy,
                crate::impl_time::microsecond_legacy_fn_meta(),
            ),
        ] {
            let legacy = operation == EvaluatedBytesOp::MicrosecondLegacy;
            assert_eq!(
                operation.input_types(),
                if legacy {
                    &[EvalType::Int]
                } else {
                    &[EvalType::Bytes]
                }
            );
            assert_eq!(operation.input_role(), EvaluatedArgsRole::Values);
            assert_eq!(
                operation.eval_type(),
                if operation == EvaluatedBytesOp::TimeNative {
                    EvalType::Bytes
                } else {
                    EvalType::Int
                }
            );
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), 2);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[1]
            else {
                panic!()
            };
            assert_eq!(*args_len, 1);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: vec![LocalExpr::InputSlot {
                    slot: 0,
                    field_type: program.schema[0].clone(),
                }]
                .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            let wrong = if legacy {
                text(Some("00:00:00"))
            } else {
                EvaluatedArgs::Int(Some(0))
            };
            for invalid in [
                wrong,
                EvaluatedArgs::NoArgs,
                EvaluatedArgs::NullWitness(None),
                EvaluatedArgs::Bytes2(None, None),
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            let mut ready = std::array::from_fn(|_| ScalarValue::Int(None));
            ready[0] = if legacy {
                ScalarValue::Bytes(Some(b"00:00:00".to_vec()))
            } else {
                ScalarValue::Int(Some(0))
            };
            assert!(matches!(
                worker.eval_ready(ready, 1, &mut None),
                Err(LocalError::InvalidSpec(_))
            ));
            if !legacy {
                assert!(matches!(
                    worker.eval_args(EvaluatedArgs::Bytes(Some(vec![255]))),
                    Err(LocalError::InvalidBatch(_))
                ));
                let mut ready = std::array::from_fn(|_| ScalarValue::Int(None));
                ready[0] = ScalarValue::Bytes(Some(vec![255]));
                assert!(matches!(
                    worker.eval_ready(ready, 1, &mut None),
                    Err(LocalError::InvalidSpec(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 0);
            let cases: Vec<(EvaluatedArgs, Option<i64>, Option<(&str, bool)>)> = match operation {
                EvaluatedBytesOp::TimeNative => vec![
                    (text(None), None, None),
                    (
                        text(Some("12:34:56.123456")),
                        None,
                        Some(("12:34:56.123456", false)),
                    ),
                    (text(Some("bad")), None, Some(("00:00:00", true))),
                    (
                        text(Some("12:34:56.123456")),
                        None,
                        Some(("12:34:56.123456", false)),
                    ),
                ],
                EvaluatedBytesOp::MicrosecondNative => vec![
                    (text(None), None, None),
                    (text(Some("12:34:56.123456")), Some(123456), None),
                    (text(Some("bad")), None, None),
                    (text(Some("-00:00:00.000001")), Some(1), None),
                ],
                EvaluatedBytesOp::MicrosecondLegacy => vec![
                    (EvaluatedArgs::Int(None), None, None),
                    (EvaluatedArgs::Int(Some(i64::MIN)), Some(854775), None),
                    (EvaluatedArgs::Int(Some(-1_234_567_890)), Some(234567), None),
                    (EvaluatedArgs::Int(Some(0)), Some(0), None),
                ],
                _ => unreachable!(),
            };
            for (calls, (args, expected_int, expected_time)) in (1_u64..).zip(cases) {
                match worker.eval_args_reported(args).unwrap() {
                    ComputedValue::Bytes(value) if operation == EvaluatedBytesOp::TimeNative => {
                        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
                        let decoded = value.value().map(|bytes| {
                            assert!(crate::native_time_result_valid(bytes));
                            crate::decode_native_time_result(bytes).unwrap()
                        });
                        assert_eq!(
                            decoded,
                            expected_time.map(|(value, truncated)| crate::NativeTimeResult {
                                value,
                                truncated
                            })
                        );
                    }
                    ComputedValue::Int(value) if operation != EvaluatedBytesOp::TimeNative => {
                        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
                        assert_eq!(value.value(), expected_int);
                    }
                    _ => panic!("TIME/MICROSECOND returned the wrong ownership domain"),
                }
                assert_eq!(worker.kernel_invocations(), calls);
                assert_eq!(worker.retained_storage().unwrap(), storage);
                assert!(worker.is_healthy());
            }
            let mut zero = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits {
                    max_steps: 0,
                    ..ExecutionLimits::default()
                },
                usize::MAX,
            )
            .unwrap();
            let args = if legacy {
                EvaluatedArgs::Int(Some(i64::MIN))
            } else {
                text(Some("bad"))
            };
            let failure = zero.eval_args_reported(args).unwrap_err();
            assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
            assert_eq!(failure.sql_failure(), None);
            assert_eq!(zero.kernel_invocations(), 0);
            assert!(zero.is_healthy());
        }
        assert!(!crate::native_time_result_valid(&[]));
        assert!(!crate::native_time_result_valid(&[2, b'x']));
        assert!(!crate::native_time_result_valid(&[0, 255]));
        assert!(!crate::native_time_result_valid(
            b"\x01not-a-truncated-value"
        ));
    }

    #[test]
    fn native_decimal_division_profiles_keep_precision_receipts_and_complete_reports() {
        use crate::{
            NativeIntDivOutcome as Outcome, NativeIntDivReport, decode_native_intdiv_report,
        };
        let frame = |coefficient: &[u8], scale| {
            crate::encode_native_identity(crate::NativeIdentityRef::Decimal {
                negative: false,
                scale,
                storage_scale: scale,
                declared_shape: None,
                coefficient,
            })
            .unwrap()
        };
        let wide = format!("1{}", "0".repeat(70));
        let warning = format!(
            "Truncated incorrect DECIMAL value: '{}.{}'",
            "3".repeat(70),
            "3".repeat(9)
        );
        for operation in [
            EvaluatedBytesOp::IntDivDecimalSignedNative,
            EvaluatedBytesOp::IntDivDecimalUnsignedNative,
        ] {
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            for (probe, fallback) in [
                (None, None),
                (Some(4), Some(8)),
                (Some(31), None),
                (Some(-1), None),
                (Some(31), Some(i64::from(u32::MAX) + 1)),
            ] {
                let left = frame(b"13", 0);
                let right = frame(b"5", 0);
                assert!(matches!(
                    worker.eval_args(EvaluatedArgs::BytesIntIntBytes(
                        Some(left.clone()),
                        probe,
                        fallback,
                        Some(right.clone())
                    )),
                    Err(LocalError::InvalidBatch(_))
                ));
                let ready = [
                    ScalarValue::Bytes(Some(left)),
                    ScalarValue::Int(probe),
                    ScalarValue::Int(fallback),
                    ScalarValue::Bytes(Some(right)),
                    ScalarValue::Int(Some(4096)),
                    ScalarValue::Int(None),
                ];
                assert!(matches!(
                    worker.eval_ready(ready, 5, &mut None),
                    Err(LocalError::InvalidSpec(_))
                ));
            }
            for invalid in [
                EvaluatedArgs::NoArgs,
                EvaluatedArgs::NullWitness(None),
                EvaluatedArgs::Bytes2(Some(frame(b"13", 0)), Some(frame(b"5", 0))),
                EvaluatedArgs::BytesIntIntBytes(None, Some(4), None, Some(frame(b"5", 0))),
                EvaluatedArgs::BytesIntIntBytes(
                    Some(frame(b"13", 0)),
                    Some(4),
                    None,
                    Some(frame(b"0", 0)),
                ),
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            for (budget, arity) in [(None, 5), (Some(-1), 5), (Some(4096), 4)] {
                let ready = [
                    ScalarValue::Bytes(Some(frame(b"13", 0))),
                    ScalarValue::Int(Some(0)),
                    ScalarValue::Int(None),
                    ScalarValue::Bytes(Some(frame(b"5", 0))),
                    ScalarValue::Int(budget),
                    ScalarValue::Int(None),
                ];
                assert!(matches!(
                    worker.eval_ready(ready, arity, &mut None),
                    Err(LocalError::InvalidSpec(_))
                ));
            }
            let left = frame(b"13", 0);
            let right = frame(b"5", 0);
            worker.state.limits.max_retained_bytes =
                storage.total_bytes() + left.capacity() + right.capacity() - 1;
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::BytesIntIntBytes(
                    Some(left),
                    Some(0),
                    None,
                    Some(right)
                )),
                Err(LocalError::ResourceLimit(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            worker.state.limits = ExecutionLimits::default();
            let cases = vec![
                (
                    frame(&[255], 0),
                    None,
                    None,
                    frame(b"000", 0),
                    Outcome::ZeroDivisor,
                    None,
                ),
                (
                    frame(b"13", 0),
                    Some(0),
                    None,
                    frame(b"5", 0),
                    Outcome::Value(2),
                    None,
                ),
                (
                    frame(b"13", 0),
                    Some(31),
                    Some(2),
                    frame(b"5", 0),
                    Outcome::Value(2),
                    None,
                ),
                (
                    frame(b"130000", 4),
                    None,
                    Some(7),
                    frame(b"5", 0),
                    Outcome::Value(2),
                    None,
                ),
                (
                    frame(b"18446744073709551615", 0),
                    Some(4),
                    None,
                    frame(b"1", 0),
                    if operation == EvaluatedBytesOp::IntDivDecimalUnsignedNative {
                        Outcome::Value(-1)
                    } else {
                        Outcome::IntOverflow
                    },
                    None,
                ),
                (
                    frame(wide.as_bytes(), 0),
                    Some(4),
                    Some(30),
                    frame(b"3", 0),
                    Outcome::IntOverflow,
                    Some(warning.as_str()),
                ),
                (
                    frame(b"13", 0),
                    Some(0),
                    None,
                    frame(b"5", 0),
                    Outcome::Value(2),
                    None,
                ),
            ];
            for (calls, (left, probe, fallback, right, outcome, warning)) in (1_u64..).zip(cases) {
                let args =
                    EvaluatedArgs::BytesIntIntBytes(Some(left), probe, fallback, Some(right));
                assert!(args.admission_matches(operation));
                let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                    panic!()
                };
                assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
                let bytes = value
                    .value()
                    .expect("business NULL/overflow must retain the complete report");
                assert!(crate::native_intdiv_result_valid(bytes));
                assert_eq!(
                    decode_native_intdiv_report(bytes),
                    Some(NativeIntDivReport { warning, outcome })
                );
                assert_eq!(worker.kernel_invocations(), calls);
                assert_eq!(worker.retained_storage().unwrap(), storage);
                assert!(worker.is_healthy());
            }
        }
        assert!(!crate::native_intdiv_result_valid(&[]));
        assert!(!crate::native_intdiv_result_valid(&[0, 1]));
    }

    #[test]
    fn decimal_division_profiles_project_only_actual_finite_budgets_and_keep_legacy_zero() {
        let frame = |coefficient: &[u8]| {
            crate::encode_native_identity(crate::NativeIdentityRef::Decimal {
                negative: false,
                scale: 0,
                storage_scale: 0,
                declared_shape: None,
                coefficient,
            })
            .unwrap()
        };
        for (operation, getter) in [
            (
                EvaluatedBytesOp::IntDivDecimalSignedNative,
                crate::impl_arithmetic::int_div_decimal_signed_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::IntDivDecimalUnsignedNative,
                crate::impl_arithmetic::int_div_decimal_unsigned_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::IntDivDecimalLegacy,
                crate::impl_arithmetic::int_div_decimal_legacy_fn_meta(),
            ),
        ] {
            let native = operation.is_native_decimal_int_div();
            let arity = if native { 5 } else { 3 };
            assert_eq!(operation.input_role(), EvaluatedArgsRole::Values);
            assert_eq!(operation.input_types().len(), arity);
            assert_eq!(
                operation.eval_type(),
                if native {
                    EvalType::Bytes
                } else {
                    EvalType::Int
                }
            );
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), arity + 1);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[arity]
            else {
                panic!()
            };
            assert_eq!(*args_len, arity);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let operands = || {
                let mut left = frame(b"13");
                let mut right = frame(b"0");
                left.reserve(37);
                right.reserve(73);
                let live = left.capacity() + right.capacity();
                let args = if native {
                    EvaluatedArgs::BytesIntIntBytes(Some(left), None, None, Some(right))
                } else {
                    EvaluatedArgs::Bytes2(Some(left), Some(right))
                };
                (args, live)
            };
            let (args, live) = operands();
            assert_eq!(args.input_types(), &operation.input_types()[..arity - 1]);
            let (ready, physical, invocation) = args
                .into_values_for_operation(operation, live + 1234)
                .unwrap();
            assert_eq!(physical, arity);
            assert!(invocation.is_none());
            assert!(matches!(ready[arity - 1], ScalarValue::Int(Some(1234))));
            assert!(matches!(&ready[0], ScalarValue::Bytes(Some(bytes)) if bytes == &frame(b"13")));
            assert!(
                matches!(&ready[arity - 2], ScalarValue::Bytes(Some(bytes)) if bytes == &frame(b"0"))
            );
            let (args, live) = operands();
            assert!(matches!(
                args.into_values_for_operation(operation, live - 1),
                Err(LocalError::ResourceLimit(_))
            ));
        }
        // Ordinary INSERT retains its original four physical operands and no budget
        // slot.
        let (ready, arity, _) = EvaluatedArgs::BytesIntIntBytes(
            Some(b"abc".to_vec()),
            Some(1),
            Some(2),
            Some(b"x".to_vec()),
        )
        .into_values_for_operation(EvaluatedBytesOp::InsertUtf8Native, 0)
        .unwrap();
        assert_eq!(arity, 4);
        assert!(matches!(ready[4], ScalarValue::Int(None)));
        let mut worker = prepare_evaluated_bytes(
            EvaluatedBytesOp::IntDivDecimalLegacy,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        let storage = worker.retained_storage().unwrap();
        for invalid in [
            EvaluatedArgs::NoArgs,
            EvaluatedArgs::NullWitness(None),
            EvaluatedArgs::Decimal2 {
                left: Some(Decimal::from(13_i64)),
                right: Some(Decimal::from(5_i64)),
            },
            EvaluatedArgs::Bytes2(None, Some(frame(b"5"))),
            EvaluatedArgs::Bytes2(Some(vec![0]), Some(frame(b"5"))),
        ] {
            assert!(matches!(
                worker.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
        }
        for budget in [None, Some(-1)] {
            let mut ready = std::array::from_fn(|_| ScalarValue::Int(None));
            ready[0] = ScalarValue::Bytes(Some(frame(b"13")));
            ready[1] = ScalarValue::Bytes(Some(frame(b"5")));
            ready[2] = ScalarValue::Int(budget);
            assert!(matches!(
                worker.eval_ready(ready, 3, &mut None),
                Err(LocalError::InvalidSpec(_))
            ));
        }
        assert_eq!(worker.kernel_invocations(), 0);
        for (calls, left, right, expected) in [
            (1, b"13".as_slice(), b"5".as_slice(), Some(2)),
            (2, &[255], b"000", None), /* Actual zero divisor wins before invalid left numeric
                                        * bytes. */
            (3, b"9223372036854775808", b"1", None),
            (4, b"13", b"5", Some(2)),
        ] {
            let ComputedValue::Int(value) = worker
                .eval_args(EvaluatedArgs::Bytes2(Some(frame(left)), Some(frame(right))))
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.value(), expected);
            assert_eq!(worker.kernel_invocations(), calls);
            assert_eq!(worker.retained_storage().unwrap(), storage);
            assert!(worker.is_healthy());
        }
    }

    #[test]
    fn integer_division_profiles_preserve_signedness_width_zero_and_reuse() {
        use crate::impl_arithmetic::*;
        for (operation, getter, left, right, expected) in [
            (
                EvaluatedBytesOp::IntDivIntSsNative,
                int_div_int_ss_native_fn_meta(),
                -13_i128,
                5_i128,
                -2_i128,
            ),
            (
                EvaluatedBytesOp::IntDivIntUsNative,
                int_div_int_us_native_fn_meta(),
                -1,
                1,
                -1,
            ),
            (
                EvaluatedBytesOp::IntDivIntSuNative,
                int_div_int_su_native_fn_meta(),
                -1,
                2,
                0,
            ),
            (
                EvaluatedBytesOp::IntDivIntUuNative,
                int_div_int_uu_native_fn_meta(),
                -1,
                -2,
                1,
            ),
            (
                EvaluatedBytesOp::IntDivInt128Legacy,
                int_div_int128_legacy_fn_meta(),
                1_i128 << 100,
                1_i128 << 20,
                1_i128 << 80,
            ),
        ] {
            let legacy = operation == EvaluatedBytesOp::IntDivInt128Legacy;
            let args = |left: Option<i128>, right: Option<i128>| {
                if legacy {
                    EvaluatedArgs::Int1282(left, right)
                } else {
                    EvaluatedArgs::Int2(
                        left.map(|value| value as i64),
                        right.map(|value| value as i64),
                    )
                }
            };
            assert_eq!(
                operation.input_role(),
                if legacy {
                    EvaluatedArgsRole::Int1282
                } else {
                    EvaluatedArgsRole::Values
                }
            );
            assert_eq!(
                operation.input_types(),
                if legacy {
                    &[EvalType::Bytes, EvalType::Bytes]
                } else {
                    &[EvalType::Int, EvalType::Int]
                }
            );
            assert_eq!(
                operation.eval_type(),
                if legacy {
                    EvalType::Bytes
                } else {
                    EvalType::Int
                }
            );
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), 3);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[2]
            else {
                panic!()
            };
            assert_eq!(*args_len, 2);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            let wrong = if legacy {
                EvaluatedArgs::Bytes2(Some(vec![0; 16]), Some(vec![1; 16]))
            } else {
                EvaluatedArgs::Int1282(Some(1), Some(1))
            };
            for invalid in [
                args(None, Some(1)),
                args(Some(1), None),
                wrong,
                EvaluatedArgs::NoArgs,
                EvaluatedArgs::NullWitness(None),
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            for missing_left in [true, false] {
                let mut ready = std::array::from_fn(|_| ScalarValue::Int(None));
                for index in 0..2 {
                    let missing = (index == 0) == missing_left;
                    ready[index] = if legacy {
                        ScalarValue::Bytes((!missing).then(|| 1_i128.to_le_bytes().to_vec()))
                    } else {
                        ScalarValue::Int((!missing).then_some(1))
                    };
                }
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(ready, 2, &mut reported),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
            }
            if legacy {
                let mut ready = std::array::from_fn(|_| ScalarValue::Int(None));
                ready[0] = ScalarValue::Bytes(Some(vec![0; 15]));
                ready[1] = ScalarValue::Bytes(Some(1_i128.to_le_bytes().to_vec()));
                assert!(matches!(
                    worker.eval_ready(ready, 2, &mut None),
                    Err(LocalError::InvalidSpec(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 0);
            for (calls, divisor, expected) in [
                (1, right, Some(expected)),
                (2, 0, None),
                (3, right, Some(expected)),
            ] {
                match worker.eval_args(args(Some(left), Some(divisor))).unwrap() {
                    ComputedValue::Int(value) if !legacy => {
                        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
                        assert_eq!(value.value().map(i128::from), expected);
                    }
                    ComputedValue::Int128(value) if legacy => {
                        assert_eq!(value.metadata(), ComputedInt128Metadata::OwnInt128);
                        assert_eq!(value.value(), expected);
                    }
                    _ => panic!("integer DIV returned the wrong ownership domain"),
                }
                assert_eq!(worker.kernel_invocations(), calls);
                assert_eq!(worker.retained_storage().unwrap(), storage);
                assert!(worker.is_healthy());
            }
        }
    }

    #[test]
    fn integer_division_typed_overflow_and_legacy_unwind_retirement() {
        let prepare = |operation| {
            prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap()
        };
        for (operation, left, right) in [
            (EvaluatedBytesOp::IntDivIntSsNative, i64::MIN, -1),
            (EvaluatedBytesOp::IntDivIntUsNative, 1, -1),
            (EvaluatedBytesOp::IntDivIntSuNative, -1, 1),
        ] {
            let mut worker = prepare(operation);
            let storage = worker.retained_storage().unwrap();
            let mut failure = worker
                .eval_args_reported(EvaluatedArgs::Int2(Some(left), Some(right)))
                .unwrap_err();
            assert_eq!(
                failure.sql_failure(),
                Some(EvaluatedSqlFailureKind::BinaryArithmeticNative)
            );
            assert_eq!(
                failure.native_binary_arithmetic_error(),
                Some(&NativeBinaryArithmeticError {
                    operation: BinaryArithmeticOperation::IntDivide,
                    kind: BinaryArithmeticErrorKind::IntOverflow,
                })
            );
            assert!(failure.legacy_binary_arithmetic_error().is_none());
            for wrong in [
                EvaluatedBytesOp::AddIntSsNative,
                EvaluatedBytesOp::DivRealNative,
                EvaluatedBytesOp::IntDivInt128Legacy,
                EvaluatedBytesOp::ModIntSsNative,
            ] {
                failure.operation = Some(wrong);
                assert!(failure.native_binary_arithmetic_error().is_none());
            }
            assert_eq!(worker.kernel_invocations(), 1);
            assert!(worker.is_healthy());
            let ComputedValue::Int(value) = worker
                .eval_args(EvaluatedArgs::Int2(Some(8), Some(2)))
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(value.value(), Some(4));
            assert_eq!(worker.kernel_invocations(), 2);
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let mut legacy = prepare(EvaluatedBytesOp::IntDivInt128Legacy);
        let ComputedValue::Int128(value) = legacy
            .eval_args(EvaluatedArgs::Int1282(Some(i128::MIN), Some(0)))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(value.value(), None);
        assert_eq!(legacy.kernel_invocations(), 1);
        assert!(legacy.is_healthy());
        // Only the test harness catches this original raw arithmetic panic.
        let panic = catch_unwind(AssertUnwindSafe(|| {
            legacy.eval_args(EvaluatedArgs::Int1282(Some(i128::MIN), Some(-1)))
        }));
        assert!(panic.is_err());
        assert!(legacy.poisoned);
        assert!(!legacy.is_healthy());
        assert!(legacy.retained_storage().is_err());
        assert!(matches!(
            legacy.eval_args(EvaluatedArgs::Int1282(Some(4), Some(2))),
            Err(LocalError::InvalidSpec(_))
        ));
    }

    #[test]
    fn tso_and_timediff_profiles_preserve_conditional_inputs_and_reuse() {
        use tidb_query_datatype::codec::mysql::Time;
        let tso = 1001_i64 << 18;
        let text = |left: Option<&str>, right: Option<&str>| {
            EvaluatedArgs::Bytes2(
                left.map(|value| value.as_bytes().to_vec()),
                right.map(|value| value.as_bytes().to_vec()),
            )
        };
        for (operation, getter) in [
            (
                EvaluatedBytesOp::TidbParseTsoNative,
                crate::impl_time::tidb_parse_tso_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::TimeDiffTextNative,
                crate::impl_time::time_diff_text_native_fn_meta(),
            ),
        ] {
            assert_eq!(operation.input_types().len(), 2);
            assert_eq!(operation.input_role(), EvaluatedArgsRole::Values);
            assert_eq!(operation.eval_type(), EvalType::Bytes);
            assert_eq!(operation.call_count(), 1);
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), 3);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[2]
            else {
                panic!()
            };
            assert_eq!(*args_len, 2);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            let invalid = if operation == EvaluatedBytesOp::TidbParseTsoNative {
                vec![
                    EvaluatedArgs::Int2(None, Some(0)),
                    EvaluatedArgs::Int2(Some(-1), Some(0)),
                    EvaluatedArgs::Int2(Some(tso), None),
                    EvaluatedArgs::Int2(Some(tso), Some(i64::from(i32::MAX) + 1)),
                ]
            } else {
                vec![
                    text(None, Some("00:00:00")),
                    text(Some("2023-02-29"), Some("00:00:00")),
                    EvaluatedArgs::Bytes2(Some(vec![255]), None),
                    EvaluatedArgs::Bytes2(Some(b"01:00:00".to_vec()), Some(vec![255])),
                ]
            };
            for invalid in invalid {
                let mut ready = std::array::from_fn(|_| ScalarValue::Int(None));
                match &invalid {
                    EvaluatedArgs::Int2(left, right) => {
                        ready[0] = ScalarValue::Int(*left);
                        ready[1] = ScalarValue::Int(*right);
                    }
                    EvaluatedArgs::Bytes2(left, right) => {
                        ready[0] = ScalarValue::Bytes(left.clone());
                        ready[1] = ScalarValue::Bytes(right.clone());
                    }
                    _ => unreachable!(),
                }
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(ready, 2, &mut reported),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
            }
            let wrong = if operation == EvaluatedBytesOp::TidbParseTsoNative {
                text(None, None)
            } else {
                EvaluatedArgs::Int2(None, None)
            };
            for invalid in [
                wrong,
                EvaluatedArgs::NoArgs,
                EvaluatedArgs::NullWitness(None),
                EvaluatedArgs::Bytes(None),
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 0);
            let cases: Vec<(EvaluatedArgs, Option<u64>, Option<&[u8]>)> =
                if operation == EvaluatedBytesOp::TidbParseTsoNative {
                    vec![
                        (EvaluatedArgs::Int2(None, None), None, None),
                        (EvaluatedArgs::Int2(Some(-1), None), None, None),
                        (EvaluatedArgs::Int2(Some(0), None), None, None),
                        (
                            EvaluatedArgs::Int2(Some(tso), Some(0)),
                            Some(Time::native_core_from_fields(1970, 1, 1, 0, 0, 1, 1000)),
                            None,
                        ),
                        (
                            EvaluatedArgs::Int2(Some(tso), Some(i64::from(i32::MAX))),
                            Some(Time::native_core_from_fields(2038, 1, 19, 3, 14, 8, 1000)),
                            None,
                        ),
                        (
                            EvaluatedArgs::Int2(Some(tso), Some(i64::from(i32::MIN))),
                            Some(Time::native_core_from_fields(
                                1901, 12, 13, 20, 45, 53, 1000,
                            )),
                            None,
                        ),
                    ]
                } else {
                    vec![
                        (text(None, None), None, None),
                        (text(Some("2023-02-29"), None), None, None),
                        (text(Some("01:00:00"), None), None, None),
                        (
                            text(Some("2020-01-01 00:00:01"), Some("00:00:00")),
                            None,
                            None,
                        ),
                        (
                            text(Some("02:03:04.500000"), Some("01:02:03.250000")),
                            None,
                            Some(b"01:01:01.250000".as_slice()),
                        ),
                    ]
                };
            for (calls, (args, core, expected)) in (1_u64..).zip(cases) {
                assert_eq!(args.input_types(), operation.input_types());
                assert!(args.admission_matches(operation));
                let ComputedValue::Bytes(output) = worker.eval_args(args).unwrap() else {
                    panic!()
                };
                assert_eq!(output.metadata(), ComputedBytesMetadata::OwnBytes);
                if let Some(core) = core {
                    assert_eq!(
                        crate::decode_native_identity(output.value().unwrap()).unwrap(),
                        crate::NativeIdentityRef::Time {
                            core,
                            kind: 1,
                            fsp: 6
                        }
                    );
                } else {
                    assert_eq!(output.value(), expected);
                }
                assert_eq!(worker.kernel_invocations(), calls);
                assert_eq!(worker.retained_storage().unwrap(), storage);
                assert!(worker.is_healthy());
            }
        }
    }

    #[test]
    fn identity_profiles_share_leaf_but_keep_fixed_arity_and_actual_frames() {
        use crate::{
            NativeIdentityRef as Identity, decode_native_identity, encode_native_identity,
        };
        let views = [
            Identity::MinNotNull,
            Identity::MaxValue,
            Identity::Int(i64::MIN),
            Identity::UInt(u64::MAX),
            Identity::Real(0x7ff8_0000_0000_0042),
            Identity::String {
                collation: 11,
                bytes: &[255, 0, b'A'],
            },
            Identity::Decimal {
                negative: true,
                scale: 4,
                storage_scale: 9,
                declared_shape: Some((38, 7)),
                coefficient: b"0012300",
            },
            Identity::Time {
                core: 0xfedc_ba98_7654_3210,
                kind: 2,
                fsp: 0,
            },
        ];
        let mut frames = vec![None]; // Physical NULL, never a synthetic zero tag.
        frames.extend(
            views
                .iter()
                .copied()
                .map(|value| Some(encode_native_identity(value).unwrap())),
        );
        // The ordinary vararg leaf's zero/multiple-argument behavior is unchanged.
        assert_eq!(
            crate::impl_miscellaneous::any_value_bytes(&[]).unwrap(),
            None
        );
        assert_eq!(
            crate::impl_miscellaneous::any_value_bytes(&[
                frames[1].as_deref(),
                frames[2].as_deref()
            ])
            .unwrap(),
            frames[1]
        );
        let getter = crate::impl_miscellaneous::any_value_bytes_fn_meta();
        for (operation, identity) in [
            (
                EvaluatedBytesOp::AnyValueNative,
                crate::LocalFunctionId::AnyValueNative,
            ),
            (
                EvaluatedBytesOp::NameConstNative,
                crate::LocalFunctionId::NameConstNative,
            ),
        ] {
            assert!(
                matches!(operation.kernel_kind(), EvaluatedKernelKind::ClosedPrivate(id) if id == identity)
            );
            assert_eq!(operation.input_types(), &[EvalType::Bytes]);
            assert_eq!(operation.input_role(), EvaluatedArgsRole::Values);
            assert_eq!(operation.eval_type(), EvalType::Bytes);
            assert_eq!(operation.call_count(), 1);
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), 2);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[1]
            else {
                panic!()
            };
            assert_eq!(*args_len, 1);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: vec![LocalExpr::InputSlot {
                    slot: 0,
                    field_type: program.schema[0].clone(),
                }]
                .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            for bad in [vec![], vec![0], vec![19]] {
                assert!(matches!(
                    worker.eval_args(EvaluatedArgs::Bytes(Some(bad.clone()))),
                    Err(LocalError::InvalidBatch(_))
                ));
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(
                        [
                            ScalarValue::Bytes(Some(bad)),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None)
                        ],
                        1,
                        &mut reported
                    ),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
            }
            for invalid in [
                EvaluatedArgs::NoArgs,
                EvaluatedArgs::NullWitness(None),
                EvaluatedArgs::Int(Some(0)),
                EvaluatedArgs::Bytes2(frames[1].clone(), frames[2].clone()),
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            for arity in [0, 2] {
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(
                        [
                            ScalarValue::Bytes(frames[1].clone()),
                            ScalarValue::Bytes(frames[2].clone()),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None)
                        ],
                        arity,
                        &mut reported
                    ),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
            }
            assert_eq!(worker.kernel_invocations(), 0);
            for (calls, frame) in (1_u64..).zip(&frames) {
                let ComputedValue::Bytes(output) = worker
                    .eval_args(EvaluatedArgs::Bytes(frame.clone()))
                    .unwrap()
                else {
                    panic!()
                };
                assert_eq!(output.metadata(), ComputedBytesMetadata::OwnBytes);
                assert_eq!(output.value(), frame.as_deref());
                if let Some(bytes) = output.value() {
                    assert_eq!(
                        decode_native_identity(bytes).unwrap(),
                        views[calls as usize - 2]
                    );
                }
                assert_eq!(worker.kernel_invocations(), calls);
                assert_eq!(worker.retained_storage().unwrap(), storage);
                assert!(worker.is_healthy());
            }
        }
    }

    #[test]
    fn weight_and_format_profiles_keep_actual_metadata_null_rules_and_reuse() {
        use crate::impl_string::*;
        let mut padded = 2_i64.to_le_bytes().to_vec();
        padded.push(1);
        padded.extend_from_slice(&1_u64.to_le_bytes());
        padded.extend_from_slice(&[0, 0]);
        for (operation, getter, args, expected) in [
            (
                EvaluatedBytesOp::WeightStringNative,
                weight_string_native_fn_meta(),
                EvaluatedArgs::Bytes2(Some(b"A".to_vec()), Some(vec![7, 1])),
                Some(vec![0, 65]),
            ),
            (
                EvaluatedBytesOp::WeightStringCharNative,
                weight_string_char_native_fn_meta(),
                EvaluatedArgs::Bytes2(Some(b"a".to_vec()), Some(padded.clone())),
                Some(b"a ".to_vec()),
            ),
            (
                EvaluatedBytesOp::WeightStringBinaryNative,
                weight_string_binary_native_fn_meta(),
                EvaluatedArgs::Bytes2(Some(b"a".to_vec()), Some(padded.clone())),
                Some(vec![b'a', 0]),
            ),
            (
                EvaluatedBytesOp::WeightStringNumericNative,
                weight_string_numeric_native_fn_meta(),
                EvaluatedArgs::Int(Some(246)),
                None,
            ), // Actual numeric type, not a fabricated SQL NULL.
            (
                EvaluatedBytesOp::FormatLocaleNative,
                format_locale_native_fn_meta(),
                EvaluatedArgs::BytesBytesInt(Some(b"1234.5".to_vec()), None, Some(1)),
                Some(b"1,234.5".to_vec()),
            ),
        ] {
            let copy_args = || match &args {
                EvaluatedArgs::Bytes2(value, meta) => {
                    EvaluatedArgs::Bytes2(value.clone(), meta.clone())
                }
                EvaluatedArgs::Int(kind) => EvaluatedArgs::Int(*kind),
                EvaluatedArgs::BytesBytesInt(value, locale, precision) => {
                    EvaluatedArgs::BytesBytesInt(value.clone(), locale.clone(), *precision)
                }
                _ => unreachable!(),
            };
            assert!(args.admission_matches(operation));
            assert_eq!(args.role(), EvaluatedArgsRole::Values);
            assert_eq!(args.input_types(), operation.input_types());
            assert_eq!(operation.eval_type(), EvalType::Bytes);
            let arity = operation.input_types().len();
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), arity + 1);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[arity]
            else {
                panic!()
            };
            assert_eq!(*args_len, arity);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            let invalid = match &args {
                EvaluatedArgs::Bytes2(value, metadata) => {
                    let mut bad_metadata = metadata.clone().unwrap();
                    *bad_metadata.last_mut().unwrap() = 2; // Not an actual padding overflow/global-undemanded case.
                    vec![
                        EvaluatedArgs::Bytes2(None, metadata.clone()),
                        EvaluatedArgs::Bytes2(value.clone(), Some(bad_metadata)),
                    ]
                }
                EvaluatedArgs::Int(_) => {
                    vec![EvaluatedArgs::Int(None), EvaluatedArgs::Int(Some(253))]
                }
                EvaluatedArgs::BytesBytesInt(value, ..) => {
                    assert!(operation.weight_or_format_args_valid(
                        value.as_deref(),
                        None,
                        Some(i64::MAX)
                    ));
                    vec![
                        EvaluatedArgs::BytesBytesInt(None, None, Some(1)),
                        EvaluatedArgs::BytesBytesInt(value.clone(), Some(vec![255]), Some(1)),
                        EvaluatedArgs::BytesBytesInt(value.clone(), None, None),
                    ]
                }
                _ => unreachable!(),
            };
            for invalid in invalid {
                let mut ready = std::array::from_fn(|_| ScalarValue::Int(None));
                match &invalid {
                    EvaluatedArgs::Bytes2(value, metadata) => {
                        ready[0] = ScalarValue::Bytes(value.clone());
                        ready[1] = ScalarValue::Bytes(metadata.clone());
                    }
                    EvaluatedArgs::Int(kind) => ready[0] = ScalarValue::Int(*kind),
                    EvaluatedArgs::BytesBytesInt(value, locale, precision) => {
                        ready[0] = ScalarValue::Bytes(value.clone());
                        ready[1] = ScalarValue::Bytes(locale.clone());
                        ready[2] = ScalarValue::Int(*precision);
                    }
                    _ => unreachable!(),
                }
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(ready, arity, &mut reported),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
            }
            for invalid in [
                EvaluatedArgs::NoArgs,
                EvaluatedArgs::NullWitness(None),
                EvaluatedArgs::Bytes(None),
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 0);
            for calls in 1..=2 {
                let ComputedValue::Bytes(output) = worker.eval_args(copy_args()).unwrap() else {
                    panic!()
                };
                assert_eq!(output.metadata(), ComputedBytesMetadata::OwnBytes);
                assert_eq!(output.value(), expected.as_deref());
                assert_eq!(worker.kernel_invocations(), calls);
                assert_eq!(worker.retained_storage().unwrap(), storage);
                assert!(worker.is_healthy());
            }
        }
        let mut null = prepare_evaluated_bytes(
            EvaluatedBytesOp::GetFormatNullNative,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        let ComputedValue::Bytes(output) = null.eval_args(EvaluatedArgs::Bytes(None)).unwrap()
        else {
            panic!()
        };
        assert_eq!(output.value(), None);
        assert_eq!(null.kernel_invocations(), 1);
    }

    #[test]
    fn date_core_profiles_keep_signed_bits_nullable_predicates_and_reuse() {
        let date = (1_u64 << 63) | (1_u64 << 46) | (2_u64 << 41);
        let hidden = date | ((1_u64 << 41) - 1);
        let native = |core: u64, flags| {
            EvaluatedArgs::BytesInt(Some(core.to_le_bytes().to_vec()), Some(flags))
        };
        for (operation, getter) in [
            (
                EvaluatedBytesOp::DateCoreNative,
                crate::impl_time::date_core_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::DateCorePredicateLegacy,
                crate::impl_time::date_core_predicate_legacy_fn_meta(),
            ),
        ] {
            let arity = operation.input_types().len();
            assert_eq!(operation.eval_type(), EvalType::Int);
            assert_eq!(operation.call_count(), 1);
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), arity + 1);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[arity]
            else {
                panic!()
            };
            assert_eq!(*args_len, arity);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            if operation == EvaluatedBytesOp::DateCoreNative {
                for invalid in [
                    EvaluatedArgs::BytesInt(Some(vec![0; 7]), Some(0)),
                    EvaluatedArgs::BytesInt(None, Some(0)),
                    EvaluatedArgs::BytesInt(Some(hidden.to_le_bytes().to_vec()), None),
                    native(hidden, -1),
                    native(hidden, 8),
                ] {
                    let EvaluatedArgs::BytesInt(core, modes) = &invalid else {
                        panic!()
                    };
                    let ready = [
                        ScalarValue::Bytes(core.clone()),
                        ScalarValue::Int(*modes),
                        ScalarValue::Int(None),
                        ScalarValue::Int(None),
                        ScalarValue::Int(None),
                        ScalarValue::Int(None),
                    ];
                    assert!(matches!(
                        worker.eval_args(invalid),
                        Err(LocalError::InvalidBatch(_))
                    ));
                    let mut reported = None;
                    assert!(matches!(
                        worker.eval_ready(ready, 2, &mut reported),
                        Err(LocalError::InvalidSpec(_))
                    ));
                    assert_eq!(reported, None);
                }
                assert!(matches!(
                    worker.eval_args(EvaluatedArgs::TimeCoreBits(Some(hidden))),
                    Err(LocalError::InvalidBatch(_))
                ));
            } else {
                // Typed Option<u64> cannot carry a short frame. The official
                // nullable temporal boundary still rejects malformed raw bytes.
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(
                        [
                            ScalarValue::Bytes(Some(vec![0; 7])),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None)
                        ],
                        1,
                        &mut reported
                    ),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
                assert!(matches!(
                    worker.eval_args(EvaluatedArgs::Bytes(Some(hidden.to_le_bytes().to_vec()))),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            for invalid in [EvaluatedArgs::NoArgs, EvaluatedArgs::NullWitness(None)] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 0);
            let cases = if operation == EvaluatedBytesOp::DateCoreNative {
                assert!((date as i64) < 0);
                vec![
                    (native(hidden, 7), Some(date as i64)), /* Rebuilt DATE clears hidden
                                                             * clock/reserved bits. */
                    (native(hidden, 3), Some(date as i64)), // allow-invalid does not affect DATE.
                    (native(1, 1), Some(0)),                /* Original raw core is nonzero
                                                             * before projection. */
                    (native(1, 2), None),
                    (native(0, 1), None),
                    (native(0, 4), Some(0)),
                ]
            } else {
                vec![
                    (EvaluatedArgs::TimeCoreBits(Some(hidden)), Some(1)),
                    (EvaluatedArgs::TimeCoreBits(Some(1)), Some(0)),
                    (EvaluatedArgs::TimeCoreBits(None), None),
                ]
            };
            for (calls, (args, expected)) in (1_u64..).zip(cases) {
                assert_eq!(args.role(), operation.input_role());
                assert_eq!(args.input_types(), operation.input_types());
                let ComputedValue::Int(output) = worker.eval_args(args).unwrap() else {
                    panic!()
                };
                assert_eq!(output.metadata(), ComputedIntMetadata::OwnSignedInt);
                assert_eq!(output.value(), expected);
                assert_eq!(worker.kernel_invocations(), calls);
                assert_eq!(worker.retained_storage().unwrap(), storage);
                assert!(worker.is_healthy());
            }
        }
        let mut null = prepare_evaluated_bytes(
            EvaluatedBytesOp::DateDiffNullNative,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        let ComputedValue::Int(output) = null.eval_args(EvaluatedArgs::NullWitness(None)).unwrap()
        else {
            panic!()
        };
        assert_eq!(output.value(), None);
        assert_eq!(null.kernel_invocations(), 1);
    }

    #[test]
    fn local_clock_profiles_use_actual_inputs_and_reuse_fixed_metadata() {
        use crate::impl_time::*;
        let mut clock = 86_399_i64.to_le_bytes().to_vec();
        clock.extend_from_slice(&999_600_000_u32.to_le_bytes());
        clock.extend_from_slice(&3600_i32.to_le_bytes());
        for (operation, getter, first, second) in [
            (
                EvaluatedBytesOp::NowNative,
                now_native_fn_meta(),
                "1970-01-02 00:59:59.999",
                "1970-01-01 00:00:00.000",
            ),
            (
                EvaluatedBytesOp::CurrentDateNative,
                current_date_native_fn_meta(),
                "1970-01-02",
                "1970-01-01",
            ),
            (
                EvaluatedBytesOp::SysdateNative,
                sysdate_native_fn_meta(),
                "1970-01-02 01:00:00.000",
                "1970-01-01 00:00:00.000",
            ),
        ] {
            let arity = operation.input_types().len();
            let args = |clock: Vec<u8>| {
                if arity == 2 {
                    EvaluatedArgs::BytesInt(Some(clock), Some(3))
                } else {
                    EvaluatedArgs::Bytes(Some(clock))
                }
            };
            assert_eq!(operation.input_role(), EvaluatedArgsRole::Values);
            assert_eq!(operation.eval_type(), EvalType::Bytes);
            assert_eq!(operation.call_count(), 1);
            assert_eq!(args(clock.clone()).input_types(), operation.input_types());
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), arity + 1);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[arity]
            else {
                panic!()
            };
            assert_eq!(*args_len, arity);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            let invalid = if arity == 2 {
                vec![
                    EvaluatedArgs::BytesInt(Some(vec![0; 15]), Some(3)),
                    EvaluatedArgs::BytesInt(None, Some(3)),
                    EvaluatedArgs::BytesInt(Some(clock.clone()), None),
                    EvaluatedArgs::BytesInt(Some(clock.clone()), Some(7)),
                ]
            } else {
                vec![
                    EvaluatedArgs::Bytes(Some(vec![0; 15])),
                    EvaluatedArgs::Bytes(None),
                ]
            };
            for invalid in invalid {
                let mut ready = std::array::from_fn(|_| ScalarValue::Int(None));
                match &invalid {
                    EvaluatedArgs::Bytes(bytes) => ready[0] = ScalarValue::Bytes(bytes.clone()),
                    EvaluatedArgs::BytesInt(bytes, fsp) => {
                        ready[0] = ScalarValue::Bytes(bytes.clone());
                        ready[1] = ScalarValue::Int(*fsp);
                    }
                    _ => unreachable!(),
                }
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(ready, arity, &mut reported),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
            }
            for invalid in [EvaluatedArgs::NoArgs, EvaluatedArgs::NullWitness(None)] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 0);
            for (calls, (clock, expected)) in
                (1_u64..=2).zip([(clock.clone(), first), (vec![0; 16], second)])
            {
                let ComputedValue::Bytes(output) = worker.eval_args(args(clock)).unwrap() else {
                    panic!()
                };
                assert_eq!(output.metadata(), ComputedBytesMetadata::OwnBytes);
                assert_eq!(output.value(), Some(expected.as_bytes()));
                assert_eq!(worker.kernel_invocations(), calls);
                assert_eq!(worker.retained_storage().unwrap(), storage);
                assert!(worker.is_healthy());
            }
        }
    }

    #[test]
    fn json_merge_profiles_pack_actual_nullable_raw_lists_and_reuse_workers() {
        use crate::impl_json::*;
        let nullable =
            prepare_json_nullable_values_args(&[None, Some(serde_json::Value::Null)]).unwrap();
        assert!(nullable.admission_matches(EvaluatedBytesOp::JsonMergePatchSerdeNative));
        let EvaluatedArgs::Bytes(Some(nullable)) = nullable else {
            panic!()
        };
        assert_eq!(&nullable[..8], &2_u64.to_le_bytes());
        assert_eq!(&nullable[8..10], &[0, 1]);
        assert_eq!(&nullable[10..18], &4_u64.to_le_bytes());
        assert_eq!(&nullable[18..], b"null");
        let raw = prepare_json_raw_values_args(&[(255, &[])]).unwrap();
        assert!(raw.admission_matches(EvaluatedBytesOp::JsonMergePatchRawLegacy));
        let EvaluatedArgs::Bytes(Some(raw)) = raw else {
            panic!()
        };
        assert_eq!(&raw[8..16], &1_u64.to_le_bytes());
        assert_eq!(&raw[16..], &[255]); // Unknown tag and empty payload stay actual data.
        let one = 1_i64.to_le_bytes();
        let two = 2_i64.to_le_bytes();
        let mut raw_result = vec![9];
        raw_result.extend_from_slice(&two);
        for (operation, getter, args, empty, expected) in [
            (
                EvaluatedBytesOp::JsonMergeSerdeNative,
                json_merge_serde_native_fn_meta(),
                prepare_json_array_args(&[
                    serde_json::json!({"a": 1}),
                    serde_json::json!({"a": 2}),
                ])
                .unwrap(),
                prepare_json_array_args(&[]).unwrap(),
                br#"{"a": [1, 2]}"#.to_vec(),
            ),
            (
                EvaluatedBytesOp::JsonMergePatchSerdeNative,
                json_merge_patch_serde_native_fn_meta(),
                prepare_json_nullable_values_args(&[
                    Some(serde_json::json!({"a": 1})),
                    Some(serde_json::json!({"a": null, "b": 2})),
                ])
                .unwrap(),
                prepare_json_nullable_values_args(&[]).unwrap(),
                br#"{"b": 2}"#.to_vec(),
            ),
            (
                EvaluatedBytesOp::JsonMergePatchRawLegacy,
                json_merge_patch_raw_legacy_fn_meta(),
                prepare_json_raw_values_args(&[(9, one.as_slice()), (9, two.as_slice())]).unwrap(),
                prepare_json_raw_values_args(&[]).unwrap(),
                raw_result,
            ),
        ] {
            // Empty lists remain structurally admissible. In particular,
            // native PATCH's empty-list panic is not preempted by admission.
            assert!(empty.admission_matches(operation));
            let EvaluatedArgs::Bytes(Some(empty)) = empty else {
                panic!()
            };
            assert_eq!(empty, 0_u64.to_le_bytes());
            let EvaluatedArgs::Bytes(Some(packet)) = args else {
                panic!()
            };
            assert_eq!(operation.input_types(), &[EvalType::Bytes]);
            assert_eq!(operation.input_role(), EvaluatedArgsRole::Values);
            assert_eq!(operation.eval_type(), EvalType::Bytes);
            assert!(!operation.returns_json_report());
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), 2);
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[1]
            else {
                panic!()
            };
            assert_eq!(*args_len, 1);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: vec![LocalExpr::InputSlot {
                    slot: 0,
                    field_type: program.schema[0].clone(),
                }]
                .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            let malformed = if operation == EvaluatedBytesOp::JsonMergePatchSerdeNative {
                let mut bytes = 1_u64.to_le_bytes().to_vec();
                bytes.push(2);
                bytes
            } else {
                vec![0; 7]
            };
            for invalid in [None, Some(malformed)] {
                assert!(matches!(
                    worker.eval_args(EvaluatedArgs::Bytes(invalid.clone())),
                    Err(LocalError::InvalidBatch(_))
                ));
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(
                        [
                            ScalarValue::Bytes(invalid),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None)
                        ],
                        1,
                        &mut reported
                    ),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
            }
            for invalid in [EvaluatedArgs::NoArgs, EvaluatedArgs::NullWitness(None)] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 0);
            for calls in 1..=2 {
                let ComputedValue::Bytes(output) = worker
                    .eval_args(EvaluatedArgs::Bytes(Some(packet.clone())))
                    .unwrap()
                else {
                    panic!()
                };
                assert_eq!(output.metadata(), ComputedBytesMetadata::OwnBytes);
                assert_eq!(output.value(), Some(expected.as_slice()));
                assert_eq!(worker.kernel_invocations(), calls);
                assert_eq!(worker.retained_storage().unwrap(), storage);
                assert!(worker.is_healthy());
            }
            if operation == EvaluatedBytesOp::JsonMergePatchRawLegacy {
                let ComputedValue::Bytes(output) =
                    worker.eval_args(EvaluatedArgs::Bytes(Some(empty))).unwrap()
                else {
                    panic!()
                };
                assert_eq!(output.value(), None);
                assert_eq!(worker.kernel_invocations(), 3);
                assert_eq!(worker.retained_storage().unwrap(), storage);
                assert!(worker.is_healthy());
            }
        }
    }

    #[test]
    fn clock_fixed_profiles_validate_actual_frames_and_reuse_owned_bytes() {
        use crate::impl_time::*;
        let mut clock = 0_i64.to_le_bytes().to_vec();
        clock.extend_from_slice(&123_456_789_u32.to_le_bytes());
        clock.extend_from_slice(&3600_i32.to_le_bytes());
        for (operation, getter, expected) in [
            (
                EvaluatedBytesOp::UtcDateNative,
                utc_date_native_fn_meta(),
                Some("1970-01-01"),
            ),
            (
                EvaluatedBytesOp::UtcTimestampNative,
                utc_timestamp_native_fn_meta(),
                Some("1970-01-01 00:00:00.123"),
            ),
            (
                EvaluatedBytesOp::CurrentTimeWithoutFspNative,
                current_time_without_fsp_native_fn_meta(),
                Some("01:00:00"),
            ),
            (
                EvaluatedBytesOp::CurrentTimeWithFspNative,
                current_time_with_fsp_native_fn_meta(),
                Some("01:00:00.123"),
            ),
            (
                EvaluatedBytesOp::UtcTimeWithoutFspNative,
                utc_time_without_fsp_native_fn_meta(),
                Some("00:00:00"),
            ),
            (
                EvaluatedBytesOp::UtcTimeWithFspNative,
                utc_time_with_fsp_native_fn_meta(),
                Some("00:00:00.123"),
            ),
            (
                EvaluatedBytesOp::UtcTimeNullNative,
                utc_time_null_native_fn_meta(),
                None,
            ),
        ] {
            let arity = operation.input_types().len();
            let args = || {
                if operation == EvaluatedBytesOp::UtcTimeNullNative {
                    EvaluatedArgs::NullWitness(None)
                } else if arity == 2 {
                    EvaluatedArgs::BytesInt(Some(clock.clone()), Some(3))
                } else {
                    EvaluatedArgs::Bytes(Some(clock.clone()))
                }
            };
            let bad = if operation == EvaluatedBytesOp::UtcTimeNullNative {
                vec![EvaluatedArgs::NullWitness(Some(0))]
            } else if arity == 2 {
                vec![
                    EvaluatedArgs::BytesInt(Some(vec![0; 15]), Some(3)),
                    EvaluatedArgs::BytesInt(None, Some(3)),
                    EvaluatedArgs::BytesInt(Some(clock.clone()), None),
                    EvaluatedArgs::BytesInt(Some(clock.clone()), Some(-1)),
                    EvaluatedArgs::BytesInt(Some(clock.clone()), Some(7)),
                ]
            } else {
                vec![
                    EvaluatedArgs::Bytes(Some(vec![0; 15])),
                    EvaluatedArgs::Bytes(None),
                ]
            };
            // Admission can be checked before constructing the reusable factory.
            for invalid in &bad {
                assert!(!invalid.admission_matches(operation));
            }
            if operation.is_clock_value() {
                let unrestricted = vec![255; 16];
                assert!(operation.clock_args_valid(
                    Some(&unrestricted),
                    if arity == 2 { Some(6) } else { None }
                ));
                if arity == 2 {
                    for fsp in [0, 1] {
                        assert!(operation.clock_args_valid(Some(&clock), Some(fsp)));
                    }
                }
            }
            assert_eq!(operation.eval_type(), EvalType::Bytes);
            assert_eq!(operation.call_count(), 1);
            assert_eq!(args().role(), operation.input_role());
            assert_eq!(args().input_types(), operation.input_types());
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), arity + 1);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[arity]
            else {
                panic!()
            };
            assert_eq!(*args_len, arity); // BytesInt is two actual wire columns.
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            for invalid in bad {
                let mut ready = std::array::from_fn(|_| ScalarValue::Int(None));
                match &invalid {
                    EvaluatedArgs::Bytes(clock) => ready[0] = ScalarValue::Bytes(clock.clone()),
                    EvaluatedArgs::BytesInt(clock, fsp) => {
                        ready[0] = ScalarValue::Bytes(clock.clone());
                        ready[1] = ScalarValue::Int(*fsp);
                    }
                    EvaluatedArgs::NullWitness(witness) => ready[0] = ScalarValue::Int(*witness),
                    _ => unreachable!(),
                }
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(ready, arity, &mut reported),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
            }
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::NoArgs),
                Err(LocalError::InvalidBatch(_))
            ));
            let wrong_role = if operation == EvaluatedBytesOp::UtcTimeNullNative {
                EvaluatedArgs::Bytes(None)
            } else {
                EvaluatedArgs::NullWitness(None)
            };
            assert!(matches!(
                worker.eval_args(wrong_role),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            for calls in 1..=2 {
                let ComputedValue::Bytes(output) = worker.eval_args(args()).unwrap() else {
                    panic!()
                };
                assert_eq!(output.metadata(), ComputedBytesMetadata::OwnBytes);
                assert_eq!(output.value(), expected.map(str::as_bytes));
                assert_eq!(worker.kernel_invocations(), calls);
                assert_eq!(worker.retained_storage().unwrap(), storage);
                assert!(worker.is_healthy());
            }
        }
    }

    #[test]
    fn json_unquote_fixed_profiles_keep_text_binary_and_null_distinct() {
        for (operation, getter, args, expected) in [
            (
                EvaluatedBytesOp::JsonUnquoteTextNative,
                crate::impl_json::json_unquote_text_native_fn_meta(),
                EvaluatedArgs::Bytes(Some(b"\"\\n\"".to_vec())),
                b"\n".as_slice(),
            ),
            (
                EvaluatedBytesOp::JsonUnquoteBinaryNative,
                crate::impl_json::json_unquote_binary_native_fn_meta(),
                prepare_json_raw_identity_args((12, &[4, b'"', b'\\', b'n', b'"'])).unwrap(),
                b"\"\\n\"".as_slice(),
            ),
        ] {
            assert_eq!(operation.eval_type(), EvalType::Bytes);
            assert_eq!(operation.input_types(), &[EvalType::Bytes]);
            assert_eq!(operation.input_role(), EvaluatedArgsRole::Values);
            assert!(!operation.returns_json_report());
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), 2);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[1]
            else {
                panic!()
            };
            assert_eq!(*args_len, 1);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: vec![LocalExpr::InputSlot {
                    slot: 0,
                    field_type: program.schema[0].clone(),
                }]
                .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            for invalid in [
                EvaluatedArgs::Bytes(None),
                EvaluatedArgs::NullWitness(None),
                EvaluatedArgs::NoArgs,
                EvaluatedArgs::Int(Some(0)),
                EvaluatedArgs::Bytes2(Some(Vec::new()), Some(Vec::new())),
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            let bad_utf8 = if operation == EvaluatedBytesOp::JsonUnquoteTextNative {
                vec![255]
            } else {
                vec![12, 1, 255]
            };
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::Bytes(Some(bad_utf8.clone()))),
                Err(LocalError::InvalidBatch(_))
            ));
            let mut reported = None;
            assert!(matches!(
                worker.eval_ready(
                    [
                        ScalarValue::Bytes(Some(bad_utf8)),
                        ScalarValue::Int(None),
                        ScalarValue::Int(None),
                        ScalarValue::Int(None),
                        ScalarValue::Int(None),
                        ScalarValue::Int(None)
                    ],
                    1,
                    &mut reported
                ),
                Err(LocalError::InvalidSpec(_))
            ));
            assert_eq!(reported, None);
            assert_eq!(worker.kernel_invocations(), 0);
            let ComputedValue::Bytes(output) = worker.eval_args(args).unwrap() else {
                panic!()
            };
            assert_eq!(output.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(output.value(), Some(expected));
            assert_eq!(worker.kernel_invocations(), 1);
            if operation == EvaluatedBytesOp::JsonUnquoteBinaryNative {
                // Malformed raw containers retain the old Display empty-string
                // policy. This is Some(empty STRING bytes), not JSON or NULL.
                let ComputedValue::Bytes(output) = worker
                    .eval_args(prepare_json_raw_identity_args((3, &[255])).unwrap())
                    .unwrap()
                else {
                    panic!()
                };
                assert_eq!(output.value(), Some(b"".as_slice()));
                assert_eq!(output.metadata(), ComputedBytesMetadata::OwnBytes);
                assert_eq!(worker.kernel_invocations(), 2);
            }
            assert_eq!(worker.retained_storage().unwrap(), storage);
            assert!(worker.is_healthy());
        }
        let mut null = prepare_evaluated_bytes(
            EvaluatedBytesOp::JsonOutputNullNative,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        let ComputedValue::Bytes(output) =
            null.eval_args(EvaluatedArgs::NullWitness(None)).unwrap()
        else {
            panic!()
        };
        assert_eq!(output.value(), None);
        assert_eq!(output.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(null.kernel_invocations(), 1);
    }

    #[test]
    fn json_raw_legacy_profiles_preserve_codec_identity_and_real_absence() {
        use tidb_query_datatype::codec::mysql::json::NativeBinaryJsonPathLeg as Leg;

        use crate::impl_json::*;
        let padded = [0, 0, 0, 0, 9, 0, 0, 0, 77];
        let empty_array = [0, 0, 0, 0, 8, 0, 0, 0];
        let value = 2_i64.to_le_bytes();
        let root: &[Leg] = &[];
        let mut appended = vec![3, 1, 0, 0, 0, 21, 0, 0, 0, 9, 13, 0, 0, 0];
        appended.extend_from_slice(&value);
        for (operation, getter, args, expected) in [
            (
                EvaluatedBytesOp::JsonReplaceRawLegacy,
                json_replace_raw_legacy_fn_meta(),
                prepare_json_raw_paths_values_args(
                    (3, &padded),
                    std::iter::empty(),
                    std::iter::empty(),
                )
                .unwrap(),
                Some(vec![3, 0, 0, 0, 0, 8, 0, 0, 0]),
            ),
            (
                EvaluatedBytesOp::JsonArrayAppendRawLegacy,
                json_array_append_raw_legacy_fn_meta(),
                prepare_json_raw_paths_values_args(
                    (3, &empty_array),
                    std::iter::once((root, false)),
                    std::iter::once((9, value.as_slice())),
                )
                .unwrap(),
                Some(appended),
            ),
            (
                EvaluatedBytesOp::JsonArrayAppendEmptyLegacy,
                json_array_append_empty_legacy_fn_meta(),
                prepare_json_raw_identity_args((255, &[77, 0, 255])).unwrap(),
                Some(vec![255, 77, 0, 255]),
            ),
            (
                EvaluatedBytesOp::JsonValueAbsentLegacy,
                json_value_absent_legacy_fn_meta(),
                EvaluatedArgs::NoArgs,
                None,
            ),
        ] {
            assert_eq!(operation.eval_type(), EvalType::Bytes);
            assert_eq!(operation.call_count(), 1);
            assert!(!operation.returns_json_report());
            assert_eq!(args.role(), operation.input_role());
            assert_eq!(args.input_types(), operation.input_types());
            let arity = operation.input_types().len();
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), arity + 1);
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[arity]
            else {
                panic!()
            };
            assert_eq!(*args_len, arity);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::NullWitness(None)),
                Err(LocalError::InvalidBatch(_))
            ));
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::Bytes(None)),
                Err(LocalError::InvalidBatch(_))
            ));
            if operation != EvaluatedBytesOp::JsonValueAbsentLegacy {
                assert!(matches!(
                    worker.eval_args(EvaluatedArgs::NoArgs),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 0);
            let ComputedValue::Bytes(output) = worker.eval_args(args).unwrap() else {
                panic!()
            };
            assert_eq!(output.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(output.value(), expected.as_deref());
            assert_eq!(worker.kernel_invocations(), 1);
            assert_eq!(worker.retained_storage().unwrap(), storage);
            assert!(worker.is_healthy());
        }
    }

    #[test]
    fn json_raw_legacy_counted_packets_reject_framing_not_business_values() {
        use tidb_query_datatype::codec::mysql::json::{
            NativeBinaryJsonArraySelection as Array, NativeBinaryJsonPathLeg as Leg,
        };
        struct Counted<I>(I, usize);
        impl<I: Iterator> Iterator for Counted<I> {
            type Item = I::Item;
            fn next(&mut self) -> Option<Self::Item> {
                self.0.next()
            }
            fn size_hint(&self) -> (usize, Option<usize>) {
                (self.1, Some(self.1))
            }
        }
        impl<I: Iterator> ExactSizeIterator for Counted<I> {
            fn len(&self) -> usize {
                self.1
            }
        }
        let root: &[Leg] = &[];
        let raw: &[u8] = &[];
        assert!(matches!(
            prepare_json_raw_paths_values_args(
                (255, raw),
                Counted(std::iter::once((root, false)), 0),
                std::iter::empty()
            ),
            Err(LocalError::InvalidBatch(_))
        ));
        assert!(matches!(
            prepare_json_raw_paths_values_args(
                (255, raw),
                Counted(std::iter::empty(), 1),
                std::iter::once((255, raw))
            ),
            Err(LocalError::InvalidBatch(_))
        ));
        assert!(matches!(
            prepare_json_raw_paths_values_args(
                (255, raw),
                std::iter::empty(),
                Counted(std::iter::once((255, raw)), 0)
            ),
            Err(LocalError::InvalidBatch(_))
        ));
        assert!(matches!(
            prepare_json_raw_paths_values_args(
                (255, raw),
                std::iter::once((root, false)),
                Counted(std::iter::empty(), 1)
            ),
            Err(LocalError::InvalidBatch(_))
        ));
        let legs = [
            Leg::Key("*".into()),
            Leg::Array(Array::Asterisk),
            Leg::Array(Array::Index(i64::MIN)),
            Leg::Array(Array::Range {
                start: -2,
                end: i64::MAX,
            }),
            Leg::DoubleAsterisk,
        ];
        for multiple in [false, true] {
            let args = prepare_json_raw_paths_values_args(
                (254, &[0, 255]),
                std::iter::once((legs.as_slice(), multiple)),
                std::iter::once((255, raw)),
            )
            .unwrap();
            assert!(args.admission_matches(EvaluatedBytesOp::JsonReplaceRawLegacy));
            assert!(args.admission_matches(EvaluatedBytesOp::JsonArrayAppendRawLegacy));
            let EvaluatedArgs::Bytes3([Some(document), Some(paths), Some(values)]) = args else {
                panic!()
            };
            assert_eq!(document, [254, 0, 255]);
            assert_eq!(paths[8], u8::from(multiple));
            assert_eq!(paths[17], 0);
            assert_eq!(paths[26], b'*');
            assert_eq!(paths[27], 1);
            assert_eq!(paths[28], 2);
            assert_eq!(&paths[29..37], &i64::MIN.to_le_bytes());
            assert_eq!(paths[37], 3);
            assert_eq!(&paths[38..46], &(-2_i64).to_le_bytes());
            assert_eq!(&paths[46..54], &i64::MAX.to_le_bytes());
            assert_eq!(paths[54], 4);
            assert_eq!(&values[..8], &1_u64.to_le_bytes());
            assert_eq!(&values[8..16], &1_u64.to_le_bytes());
            assert_eq!(values[16], 255);
        }
        let one = 1_i64.to_le_bytes();
        let two = 2_i64.to_le_bytes();
        for operation in [
            EvaluatedBytesOp::JsonReplaceRawLegacy,
            EvaluatedBytesOp::JsonArrayAppendRawLegacy,
        ] {
            let EvaluatedArgs::Bytes3(valid) = prepare_json_raw_paths_values_args(
                (9, &one),
                std::iter::once((root, false)),
                std::iter::once((9, two.as_slice())),
            )
            .unwrap() else {
                panic!()
            };
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            let mut invalid_cases = Vec::new();
            for slot in 0..3 {
                let mut absent = valid.clone();
                absent[slot] = None;
                invalid_cases.push(absent);
                let mut empty = valid.clone();
                empty[slot] = Some(Vec::new());
                invalid_cases.push(empty);
            }
            for packet in [
                u64::MAX.to_le_bytes().to_vec(),
                vec![0; 7],
                [0_u64.to_le_bytes(), 0_u64.to_le_bytes()].concat(),
            ] {
                let mut invalid = valid.clone();
                invalid[1] = Some(packet);
                invalid_cases.push(invalid);
            }
            let mut flag = valid.clone();
            flag[1].as_mut().unwrap()[8] = 2;
            invalid_cases.push(flag);
            let mut unknown_leg = 1_u64.to_le_bytes().to_vec();
            unknown_leg.push(0);
            unknown_leg.extend_from_slice(&1_u64.to_le_bytes());
            unknown_leg.push(5);
            let mut bad_key = unknown_leg.clone();
            *bad_key.last_mut().unwrap() = 0;
            bad_key.extend_from_slice(&1_u64.to_le_bytes());
            bad_key.push(255);
            for paths in [unknown_leg, bad_key] {
                let mut invalid = valid.clone();
                invalid[1] = Some(paths);
                invalid_cases.push(invalid);
            }
            for packet in [
                0_u64.to_le_bytes().to_vec(),
                [1_u64.to_le_bytes(), 0_u64.to_le_bytes()].concat(),
                [1_u64.to_le_bytes(), u64::MAX.to_le_bytes()].concat(),
            ] {
                let mut invalid = valid.clone();
                invalid[2] = Some(packet);
                invalid_cases.push(invalid);
            }
            if operation == EvaluatedBytesOp::JsonArrayAppendRawLegacy {
                let mut zero = valid.clone();
                zero[1] = Some(0_u64.to_le_bytes().to_vec());
                zero[2] = Some(0_u64.to_le_bytes().to_vec());
                invalid_cases.push(zero);
            }
            for invalid in invalid_cases {
                assert!(matches!(
                    worker.eval_args(EvaluatedArgs::Bytes3(invalid.clone())),
                    Err(LocalError::InvalidBatch(_))
                ));
                let [document, paths, values] = invalid;
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(
                        [
                            ScalarValue::Bytes(document),
                            ScalarValue::Bytes(paths),
                            ScalarValue::Bytes(values),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None)
                        ],
                        3,
                        &mut reported
                    ),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
            }
            assert_eq!(worker.kernel_invocations(), 0);
            // All raw values and actual flags reach the real kernel. Their
            // business None/identity outcomes are not transport rejections.
            let multiple = prepare_json_raw_paths_values_args(
                (9, &one),
                std::iter::once((root, true)),
                std::iter::once((9, two.as_slice())),
            )
            .unwrap();
            let ComputedValue::Bytes(output) = worker.eval_args(multiple).unwrap() else {
                panic!()
            };
            assert_eq!(output.value(), None);
            let malformed = prepare_json_raw_paths_values_args(
                (3, &[255]),
                std::iter::once((root, false)),
                std::iter::once((255, raw)),
            )
            .unwrap();
            let ComputedValue::Bytes(output) = worker.eval_args(malformed).unwrap() else {
                panic!()
            };
            assert_eq!(output.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(
                output.value(),
                if operation == EvaluatedBytesOp::JsonReplaceRawLegacy {
                    None
                } else {
                    Some([3, 255].as_slice())
                }
            );
            assert_eq!(worker.kernel_invocations(), 2);
            if operation == EvaluatedBytesOp::JsonArrayAppendRawLegacy {
                let zero = prepare_json_raw_paths_values_args(
                    (9, &one),
                    std::iter::empty(),
                    std::iter::empty(),
                )
                .unwrap();
                assert!(matches!(
                    worker.eval_args(zero),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            assert_eq!(worker.retained_storage().unwrap(), storage);
            assert!(worker.is_healthy());
        }
        let mut identity = prepare_evaluated_bytes(
            EvaluatedBytesOp::JsonArrayAppendEmptyLegacy,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        for invalid in [
            EvaluatedArgs::Bytes(None),
            EvaluatedArgs::Bytes(Some(Vec::new())),
            EvaluatedArgs::NoArgs,
        ] {
            assert!(matches!(
                identity.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
        }
        for invalid in [None, Some(Vec::new())] {
            let mut reported = None;
            assert!(matches!(
                identity.eval_ready(
                    [
                        ScalarValue::Bytes(invalid),
                        ScalarValue::Int(None),
                        ScalarValue::Int(None),
                        ScalarValue::Int(None),
                        ScalarValue::Int(None),
                        ScalarValue::Int(None)
                    ],
                    1,
                    &mut reported
                ),
                Err(LocalError::InvalidSpec(_))
            ));
            assert_eq!(reported, None);
        }
        assert_eq!(identity.kernel_invocations(), 0);
        let ComputedValue::Bytes(output) = identity
            .eval_args(prepare_json_raw_identity_args((255, raw)).unwrap())
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(output.value(), Some([255].as_slice()));
        assert_eq!(identity.kernel_invocations(), 1);
    }

    #[test]
    fn json_path_unit_profiles_dispatch_borrowed_selectors_and_owned_bytes() {
        use crate::impl_json::*;
        for (operation, getter, document, path, expected) in [
            (
                EvaluatedBytesOp::JsonExtractSerdeNative,
                json_extract_serde_native_fn_meta(),
                serde_json::json!({"a": [1, 2]}),
                "$.a[1]",
                "2",
            ),
            (
                EvaluatedBytesOp::JsonInsertSerdeNative,
                json_insert_serde_native_fn_meta(),
                serde_json::json!({"a": 1}),
                "$.b",
                r#"{"a": 1, "b": 2}"#,
            ),
            (
                EvaluatedBytesOp::JsonSetSerdeNative,
                json_set_serde_native_fn_meta(),
                serde_json::json!({"a": 1}),
                "$.a",
                r#"{"a": 2}"#,
            ),
            (
                EvaluatedBytesOp::JsonReplaceSerdeNative,
                json_replace_serde_native_fn_meta(),
                serde_json::json!({"a": 1}),
                "$.a",
                r#"{"a": 2}"#,
            ),
            (
                EvaluatedBytesOp::JsonRemoveSerdeNative,
                json_remove_serde_native_fn_meta(),
                serde_json::json!({"a": 1, "b": 2}),
                "$.a",
                r#"{"b": 2}"#,
            ),
            (
                EvaluatedBytesOp::JsonArrayAppendSerdeNative,
                json_array_append_serde_native_fn_meta(),
                serde_json::json!({"a": [1]}),
                "$.a",
                r#"{"a": [1, 2]}"#,
            ),
            (
                EvaluatedBytesOp::JsonArrayInsertSerdeNative,
                json_array_insert_serde_native_fn_meta(),
                serde_json::json!({"a": [1, 3]}),
                "$.a[1]",
                r#"{"a": [1, 2, 3]}"#,
            ),
        ] {
            let paths = [crate::parse_native_json_path(path).unwrap()];
            let arity = operation.input_types().len();
            let args = if arity == 2 {
                prepare_json_paths_args(&document, &paths).unwrap()
            } else {
                prepare_json_path_values_args(&document, &paths, &[serde_json::json!(2)]).unwrap()
            };
            assert_eq!(operation.eval_type(), EvalType::Bytes);
            assert_eq!(operation.input_role(), EvaluatedArgsRole::Values);
            assert_eq!(operation.call_count(), 1);
            assert!(!operation.returns_json_report());
            assert_eq!(args.input_types(), operation.input_types());
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), arity + 1);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[arity]
            else {
                panic!()
            };
            assert_eq!(*args_len, arity);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!()
            };
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(value.value(), Some(expected.as_bytes()));
            assert_eq!(worker.kernel_invocations(), 1);
            assert_eq!(worker.retained_storage().unwrap(), storage);
            assert!(worker.is_healthy());
        }
    }

    #[test]
    fn json_path_packets_preserve_flags_and_reject_invalid_ready_shapes() {
        let document = serde_json::json!({"a": 1});
        let all_legs = ["$.a", "$.*", "$[*]", "$[last]", "$[1 to last]", "$**.a"]
            .into_iter()
            .map(|path| crate::parse_native_json_path(path).unwrap())
            .collect::<Vec<_>>();
        let args = prepare_json_paths_args(&document, &all_legs).unwrap();
        assert!(args.admission_matches(EvaluatedBytesOp::JsonExtractSerdeNative));
        let mut quoted = crate::parse_native_json_path(r#"$."*""#).unwrap();
        let original = encode_json_paths(std::slice::from_ref(&quoted)).unwrap();
        assert_eq!(&original[..8], &1_u64.to_le_bytes());
        assert_eq!(original[8], u8::from(quoted.could_match_multiple));
        assert_eq!(original[17], 0); // Quoted star is an actual Key, not KeyWildcard.
        assert_eq!(&original[18..26], &1_u64.to_le_bytes());
        assert_eq!(original[26], b'*');
        quoted.could_match_multiple = !quoted.could_match_multiple;
        let changed = encode_json_paths(std::slice::from_ref(&quoted)).unwrap();
        assert_eq!(changed[8], u8::from(quoted.could_match_multiple));
        assert_eq!(&changed[9..], &original[9..]);
        assert!(matches!(
            prepare_json_path_values_args(&document, std::slice::from_ref(&quoted), &[]),
            Err(LocalError::InvalidBatch(_))
        ));
        let root_packet = || {
            let mut packet = 1_u64.to_le_bytes().to_vec();
            packet.push(0);
            packet.extend_from_slice(&0_u64.to_le_bytes());
            packet
        };
        let mut bad_flag = root_packet();
        bad_flag[8] = 2;
        let mut bad_leg_count = root_packet();
        bad_leg_count[9..17].copy_from_slice(&u64::MAX.to_le_bytes());
        let leg_packet = |tag: u8, payload: &[u8]| {
            let mut packet = 1_u64.to_le_bytes().to_vec();
            packet.push(0);
            packet.extend_from_slice(&1_u64.to_le_bytes());
            packet.push(tag);
            packet.extend_from_slice(payload);
            packet
        };
        let mut bad_key = 1_u64.to_le_bytes().to_vec();
        bad_key.push(0xff);
        let bad_paths = [
            Vec::new(),
            vec![0; 7],
            u64::MAX.to_le_bytes().to_vec(),
            [0_u64.to_le_bytes(), 0_u64.to_le_bytes()].concat(),
            bad_flag,
            bad_leg_count,
            leg_packet(6, &[]),
            leg_packet(0, &bad_key),
            leg_packet(0, &u64::MAX.to_le_bytes()),
            leg_packet(3, &[]),
            leg_packet(4, &0_i64.to_le_bytes()),
        ];
        let make_args = |values: &[Option<Vec<u8>>]| match values {
            [first] => EvaluatedArgs::Bytes(first.clone()),
            [first, second] => EvaluatedArgs::Bytes2(first.clone(), second.clone()),
            [first, second, third] => {
                EvaluatedArgs::Bytes3([first.clone(), second.clone(), third.clone()])
            }
            _ => unreachable!(),
        };
        for operation in [
            EvaluatedBytesOp::JsonExtractSerdeNative,
            EvaluatedBytesOp::JsonInsertSerdeNative,
            EvaluatedBytesOp::JsonSetSerdeNative,
            EvaluatedBytesOp::JsonReplaceSerdeNative,
            EvaluatedBytesOp::JsonRemoveSerdeNative,
            EvaluatedBytesOp::JsonArrayAppendSerdeNative,
            EvaluatedBytesOp::JsonArrayInsertSerdeNative,
        ] {
            let arity = operation.input_types().len();
            let args = if arity == 2 {
                prepare_json_paths_args(&document, &[]).unwrap()
            } else {
                prepare_json_path_values_args(&document, &[], &[]).unwrap()
            };
            let values = match args {
                EvaluatedArgs::Bytes2(first, paths) => vec![first, paths],
                EvaluatedArgs::Bytes3(parts) => parts.into_iter().collect(),
                _ => panic!(),
            };
            assert_eq!(values[1].as_deref(), Some(0_u64.to_le_bytes().as_slice()));
            if arity == 3 {
                assert_eq!(values[2].as_deref(), Some(0_u64.to_le_bytes().as_slice()));
            }
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            let expected = if operation == EvaluatedBytesOp::JsonExtractSerdeNative {
                None
            } else {
                Some(br#"{"a": 1}"#.as_slice())
            };
            let ComputedValue::Bytes(value) = worker.eval_args(make_args(&values)).unwrap() else {
                panic!()
            };
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(value.value(), expected);
            let mut invalid_cases = Vec::new();
            for slot in 0..arity {
                let mut absent = values.clone();
                absent[slot] = None;
                invalid_cases.push(absent);
            }
            for paths in &bad_paths {
                let mut invalid = values.clone();
                invalid[1] = Some(paths.clone());
                invalid_cases.push(invalid);
            }
            let mut wrong_slot = values.clone();
            wrong_slot.swap(0, 1);
            invalid_cases.push(wrong_slot);
            if arity == 3 {
                let EvaluatedArgs::Bytes(packet) =
                    prepare_json_array_args(&[serde_json::Value::Null]).unwrap()
                else {
                    panic!()
                };
                let mut count_mismatch = values.clone();
                count_mismatch[2] = packet;
                invalid_cases.push(count_mismatch);
            }
            for invalid in invalid_cases {
                assert!(matches!(
                    worker.eval_args(make_args(&invalid)),
                    Err(LocalError::InvalidBatch(_))
                ));
                let ready = std::array::from_fn(|slot| {
                    if slot < arity {
                        ScalarValue::Bytes(invalid[slot].clone())
                    } else {
                        ScalarValue::Int(None)
                    }
                });
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(ready, arity, &mut reported),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
            }
            assert!(matches!(
                worker.eval_args(make_args(&values[..arity - 1])),
                Err(LocalError::InvalidBatch(_))
            ));
            for invalid in [
                EvaluatedArgs::NoArgs,
                EvaluatedArgs::NullWitness(None),
                EvaluatedArgs::Int2(Some(0), Some(0)),
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 1);
            let ComputedValue::Bytes(value) = worker.eval_args(make_args(&values)).unwrap() else {
                panic!()
            };
            assert_eq!(value.value(), expected);
            assert_eq!(worker.kernel_invocations(), 2);
            assert_eq!(worker.retained_storage().unwrap(), storage);
            assert!(worker.is_healthy());
        }
        let mut null = prepare_evaluated_bytes(
            EvaluatedBytesOp::JsonOutputNullNative,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        let ComputedValue::Bytes(value) = null.eval_args(EvaluatedArgs::NullWitness(None)).unwrap()
        else {
            panic!()
        };
        assert_eq!(value.value(), None);
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        assert_eq!(null.kernel_invocations(), 1);
    }

    #[test]
    fn json_output_unit_profiles_dispatch_actual_owned_bytes() {
        use crate::impl_json::*;
        for (operation, getter, args, expected) in [
            (
                EvaluatedBytesOp::JsonArraySerdeNative,
                json_array_serde_native_fn_meta(),
                prepare_json_array_args(&[serde_json::Value::Null, serde_json::json!(1)]).unwrap(),
                Some(b"[null, 1]".as_slice()),
            ),
            (
                EvaluatedBytesOp::JsonObjectSerdeNative,
                json_object_serde_native_fn_meta(),
                prepare_json_object_args(&[
                    ("x".into(), serde_json::json!(1)),
                    ("x".into(), serde_json::json!(2)),
                ])
                .unwrap(),
                Some(br#"{"x": 2}"#.as_slice()),
            ),
            (
                EvaluatedBytesOp::JsonKeysSerdeNative,
                json_keys_serde_native_fn_meta(),
                prepare_json_serde_args(&serde_json::json!({"a": 1, "b": 2}), None, None).unwrap(),
                Some(br#"["a", "b"]"#.as_slice()),
            ),
            (
                EvaluatedBytesOp::JsonKeysPathSerdeNative,
                json_keys_path_serde_native_fn_meta(),
                prepare_json_serde_args(&serde_json::json!({"o": {"a": 1}}), None, Some("$.o"))
                    .unwrap(),
                Some(br#"["a"]"#.as_slice()),
            ),
            (
                EvaluatedBytesOp::JsonPrettySerdeNative,
                json_pretty_serde_native_fn_meta(),
                prepare_json_serde_args(&serde_json::json!({"a": 1}), None, None).unwrap(),
                Some(b"{\n  \"a\": 1\n}".as_slice()),
            ),
            (
                EvaluatedBytesOp::JsonOutputNullNative,
                json_output_null_native_fn_meta(),
                EvaluatedArgs::NullWitness(None),
                None,
            ),
        ] {
            assert_eq!(operation.eval_type(), EvalType::Bytes);
            assert_eq!(operation.call_count(), 1);
            assert!(!operation.returns_json_report());
            assert_eq!(args.input_types(), operation.input_types());
            assert_eq!(args.role(), operation.input_role());
            assert!(matches!(
                operation.kernel_kind(),
                EvaluatedKernelKind::ClosedPrivate(_)
            ));
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            let arity = operation.input_types().len();
            assert_eq!(program.expression.len(), arity + 1);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[arity]
            else {
                panic!()
            };
            assert_eq!(*args_len, arity);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            if operation == EvaluatedBytesOp::JsonOutputNullNative {
                for invalid in [
                    EvaluatedArgs::NullWitness(Some(0)),
                    EvaluatedArgs::Bytes(None),
                    EvaluatedArgs::NoArgs,
                ] {
                    assert!(matches!(
                        worker.eval_args(invalid),
                        Err(LocalError::InvalidBatch(_))
                    ));
                }
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(
                        [
                            ScalarValue::Int(Some(0)),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                        ],
                        1,
                        &mut reported
                    ),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
                assert_eq!(worker.kernel_invocations(), 0);
            }
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!()
            };
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(value.value(), expected);
            assert_eq!(worker.kernel_invocations(), 1);
            assert_eq!(worker.retained_storage().unwrap(), storage);
            assert!(worker.is_healthy());
        }
    }

    #[test]
    fn json_output_packets_keep_empty_lists_pairs_and_reject_malformed_inputs() {
        let EvaluatedArgs::Bytes(Some(empty_array)) = prepare_json_array_args(&[]).unwrap() else {
            panic!()
        };
        let EvaluatedArgs::Bytes(Some(empty_object)) = prepare_json_object_args(&[]).unwrap()
        else {
            panic!()
        };
        assert_eq!(empty_array, 0_u64.to_le_bytes());
        assert_eq!(empty_object, 0_u64.to_le_bytes());
        let EvaluatedArgs::Bytes(Some(pairs)) = prepare_json_object_args(&[
            ("x".into(), serde_json::json!(1)),
            ("x".into(), serde_json::json!(2)),
        ])
        .unwrap() else {
            panic!()
        };
        let mut expected_pairs = 2_u64.to_le_bytes().to_vec();
        for value in [b'1', b'2'] {
            expected_pairs.extend_from_slice(&1_u64.to_le_bytes());
            expected_pairs.push(b'x');
            expected_pairs.extend_from_slice(&1_u64.to_le_bytes());
            expected_pairs.push(value);
        }
        assert_eq!(pairs, expected_pairs);
        let make_args = |values: &[Option<Vec<u8>>]| match values {
            [first] => EvaluatedArgs::Bytes(first.clone()),
            [first, second] => EvaluatedArgs::Bytes2(first.clone(), second.clone()),
            _ => unreachable!(),
        };
        for (operation, values, expected) in [
            (
                EvaluatedBytesOp::JsonArraySerdeNative,
                vec![empty_array],
                b"[]".as_slice(),
            ),
            (
                EvaluatedBytesOp::JsonObjectSerdeNative,
                vec![empty_object],
                b"{}".as_slice(),
            ),
            (
                EvaluatedBytesOp::JsonKeysSerdeNative,
                vec![br#"{"a":1}"#.to_vec()],
                br#"["a"]"#.as_slice(),
            ),
            (
                EvaluatedBytesOp::JsonKeysPathSerdeNative,
                vec![br#"{"a":{"x":1}}"#.to_vec(), b"$.a".to_vec()],
                br#"["x"]"#.as_slice(),
            ),
            (
                EvaluatedBytesOp::JsonPrettySerdeNative,
                vec![b"null".to_vec()],
                b"null".as_slice(),
            ),
        ] {
            let values = values.into_iter().map(Some).collect::<Vec<_>>();
            let arity = values.len();
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            let ComputedValue::Bytes(value) = worker.eval_args(make_args(&values)).unwrap() else {
                panic!()
            };
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(value.value(), Some(expected));
            let mut invalid_cases = Vec::new();
            for slot in 0..arity {
                let mut absent = values.clone();
                absent[slot] = None;
                invalid_cases.push(absent);
            }
            if matches!(
                operation,
                EvaluatedBytesOp::JsonArraySerdeNative | EvaluatedBytesOp::JsonObjectSerdeNative
            ) {
                for packet in [
                    Vec::new(),
                    vec![0; 7],
                    1_u64.to_le_bytes().to_vec(),
                    u64::MAX.to_le_bytes().to_vec(),
                    [0_u64.to_le_bytes(), 0_u64.to_le_bytes()].concat(),
                    [1_u64.to_le_bytes(), u64::MAX.to_le_bytes()].concat(),
                ] {
                    invalid_cases.push(vec![Some(packet)]);
                }
                let mut malformed_json = json_operand_list_buffer(1).unwrap();
                if operation == EvaluatedBytesOp::JsonObjectSerdeNative {
                    push_json_operand_bytes(&mut malformed_json, b"x").unwrap();
                    let mut malformed_key = json_operand_list_buffer(1).unwrap();
                    push_json_operand_bytes(&mut malformed_key, &[0xff]).unwrap();
                    push_json_operand_bytes(&mut malformed_key, b"1").unwrap();
                    invalid_cases.push(vec![Some(malformed_key)]);
                }
                push_json_operand_bytes(&mut malformed_json, b"{").unwrap();
                invalid_cases.push(vec![Some(malformed_json)]);
            } else {
                let mut malformed = values.clone();
                malformed[0] = Some(b"{".to_vec());
                invalid_cases.push(malformed);
                if operation == EvaluatedBytesOp::JsonKeysPathSerdeNative {
                    for path in [b"$[".as_slice(), b"$[*]".as_slice()] {
                        invalid_cases.push(vec![values[0].clone(), Some(path.to_vec())]);
                    }
                }
            }
            for invalid in invalid_cases {
                assert!(matches!(
                    worker.eval_args(make_args(&invalid)),
                    Err(LocalError::InvalidBatch(_))
                ));
                let ready = std::array::from_fn(|slot| {
                    if slot < arity {
                        ScalarValue::Bytes(invalid[slot].clone())
                    } else {
                        ScalarValue::Int(None)
                    }
                });
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(ready, arity, &mut reported),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
            }
            for invalid in [
                EvaluatedArgs::NoArgs,
                EvaluatedArgs::NullWitness(None),
                EvaluatedArgs::Int(Some(0)),
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 1);
            let ComputedValue::Bytes(value) = worker.eval_args(make_args(&values)).unwrap() else {
                panic!()
            };
            assert_eq!(value.value(), Some(expected));
            assert_eq!(worker.kernel_invocations(), 2);
            let absent_selection = match operation {
                EvaluatedBytesOp::JsonKeysSerdeNative => {
                    Some(EvaluatedArgs::Bytes(Some(b"null".to_vec())))
                }
                EvaluatedBytesOp::JsonKeysPathSerdeNative => Some(EvaluatedArgs::Bytes2(
                    values[0].clone(),
                    Some(b"$.missing".to_vec()),
                )),
                _ => None,
            };
            if let Some(args) = absent_selection {
                let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                    panic!()
                };
                assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
                assert_eq!(value.value(), None);
                assert_eq!(worker.kernel_invocations(), 3);
            }
            assert_eq!(worker.retained_storage().unwrap(), storage);
            assert!(worker.is_healthy());
        }
        assert!(
            EvaluatedArgs::Bytes(None).admission_matches(EvaluatedBytesOp::JsonValidTextNative)
        );
    }

    #[test]
    fn json_predicate_unit_profiles_use_actual_carriers_and_owned_ints() {
        use tidb_query_datatype::codec::mysql::Json;

        use crate::impl_json::*;
        let raw = |value: Json| {
            let mut bytes = vec![value.get_type() as u8];
            bytes.extend_from_slice(value.as_ref().value());
            bytes
        };
        for (operation, getter, expected) in [
            (
                EvaluatedBytesOp::JsonContainsSerdeNative,
                json_contains_serde_native_fn_meta(),
                Some(1),
            ),
            (
                EvaluatedBytesOp::JsonContainsPathSerdeNative,
                json_contains_path_serde_native_fn_meta(),
                Some(1),
            ),
            (
                EvaluatedBytesOp::JsonOverlapsSerdeNative,
                json_overlaps_serde_native_fn_meta(),
                Some(1),
            ),
            (
                EvaluatedBytesOp::JsonMemberOfSerdeNative,
                json_member_of_serde_native_fn_meta(),
                Some(1),
            ),
            (
                EvaluatedBytesOp::JsonLengthSerdeNative,
                json_length_serde_native_fn_meta(),
                Some(2),
            ),
            (
                EvaluatedBytesOp::JsonLengthPathSerdeNative,
                json_length_path_serde_native_fn_meta(),
                Some(3),
            ),
            (
                EvaluatedBytesOp::JsonPathExistsSerdeNative,
                json_path_exists_serde_native_fn_meta(),
                Some(1),
            ),
            (
                EvaluatedBytesOp::JsonMemberOfBinaryLegacy,
                json_member_of_binary_legacy_fn_meta(),
                Some(1),
            ),
            (
                EvaluatedBytesOp::JsonPredicateNullNative,
                json_predicate_null_native_fn_meta(),
                None,
            ),
            (
                EvaluatedBytesOp::JsonPredicateMissingLegacy,
                json_predicate_missing_legacy_fn_meta(),
                None,
            ),
        ] {
            let args = match operation {
                EvaluatedBytesOp::JsonContainsSerdeNative => {
                    EvaluatedArgs::Bytes2(Some(b"[1,2]".to_vec()), Some(b"1".to_vec()))
                }
                EvaluatedBytesOp::JsonContainsPathSerdeNative => EvaluatedArgs::Bytes3([
                    Some(br#"{"a":[1,2]}"#.to_vec()),
                    Some(b"1".to_vec()),
                    Some(b"$.a".to_vec()),
                ]),
                EvaluatedBytesOp::JsonOverlapsSerdeNative => {
                    EvaluatedArgs::Bytes2(Some(b"[1,2]".to_vec()), Some(b"[2,3]".to_vec()))
                }
                EvaluatedBytesOp::JsonMemberOfSerdeNative => {
                    EvaluatedArgs::Bytes2(Some(b"2".to_vec()), Some(b"[1,2]".to_vec()))
                }
                EvaluatedBytesOp::JsonLengthSerdeNative => {
                    EvaluatedArgs::Bytes(Some(br#"{"a":1,"b":2}"#.to_vec()))
                }
                EvaluatedBytesOp::JsonLengthPathSerdeNative => {
                    EvaluatedArgs::Bytes2(Some(br#"{"a":[1,2,3]}"#.to_vec()), Some(b"$.a".to_vec()))
                }
                EvaluatedBytesOp::JsonPathExistsSerdeNative => EvaluatedArgs::Bytes2(
                    Some(br#"{"a":[1,2]}"#.to_vec()),
                    Some(b"$.a[*]".to_vec()),
                ),
                EvaluatedBytesOp::JsonMemberOfBinaryLegacy => EvaluatedArgs::Bytes2(
                    Some(raw(Json::from_i64(1).unwrap())),
                    Some(raw(
                        Json::from_array(vec![Json::from_i64(1).unwrap()]).unwrap()
                    )),
                ),
                EvaluatedBytesOp::JsonPredicateNullNative => EvaluatedArgs::NullWitness(None),
                EvaluatedBytesOp::JsonPredicateMissingLegacy => EvaluatedArgs::NoArgs,
                _ => unreachable!(),
            };
            assert_eq!(operation.eval_type(), EvalType::Int);
            assert_eq!(operation.call_count(), 1);
            assert_eq!(args.input_types(), operation.input_types());
            assert_eq!(args.role(), operation.input_role());
            assert!(matches!(
                operation.kernel_kind(),
                EvaluatedKernelKind::ClosedPrivate(_)
            ));
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            let arity = operation.input_types().len();
            assert_eq!(program.expression.len(), arity + 1);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[arity]
            else {
                panic!()
            };
            assert_eq!(*args_len, arity);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            if expected.is_none() {
                for invalid in [
                    EvaluatedArgs::Int(None),
                    EvaluatedArgs::NullWitness(Some(0)),
                ] {
                    assert!(matches!(
                        worker.eval_args(invalid),
                        Err(LocalError::InvalidBatch(_))
                    ));
                }
                let opposite = if operation == EvaluatedBytesOp::JsonPredicateNullNative {
                    EvaluatedArgs::NoArgs
                } else {
                    EvaluatedArgs::NullWitness(None)
                };
                assert!(matches!(
                    worker.eval_args(opposite),
                    Err(LocalError::InvalidBatch(_))
                ));
                assert_eq!(worker.kernel_invocations(), 0);
            }
            let ComputedValue::Int(value) = worker.eval_args(args).unwrap() else {
                panic!()
            };
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.value(), expected);
            assert_eq!(worker.kernel_invocations(), 1);
            assert_eq!(worker.retained_storage().unwrap(), storage);
            assert!(worker.is_healthy());
        }
    }

    #[test]
    fn json_predicate_checked_packing_and_both_boundary_rejections() {
        use tidb_query_datatype::codec::mysql::Json;
        let document = serde_json::json!({"a": [1, 2]});
        let candidate = serde_json::json!(1);
        let first_bytes = serde_json::to_vec(&document).unwrap();
        for (args, arity) in [
            (prepare_json_serde_args(&document, None, None).unwrap(), 1),
            (
                prepare_json_serde_args(&document, Some(&candidate), None).unwrap(),
                2,
            ),
            (
                prepare_json_serde_args(&document, None, Some("$.a")).unwrap(),
                2,
            ),
            (
                prepare_json_serde_args(&document, Some(&candidate), Some("$.a")).unwrap(),
                3,
            ),
        ] {
            assert_eq!(args.input_types().len(), arity);
            match args {
                EvaluatedArgs::Bytes(Some(first)) => assert_eq!(first, first_bytes),
                EvaluatedArgs::Bytes2(Some(first), Some(second)) => {
                    assert_eq!(first, first_bytes);
                    assert!(second == b"1" || second == b"$.a");
                }
                EvaluatedArgs::Bytes3([Some(first), Some(second), Some(path)]) => {
                    assert_eq!(first, first_bytes);
                    assert_eq!(second, b"1");
                    assert_eq!(path, b"$.a");
                }
                _ => panic!(),
            }
        }
        let EvaluatedArgs::Bytes(Some(json_null)) =
            prepare_json_serde_args(&serde_json::Value::Null, None, None).unwrap()
        else {
            panic!()
        };
        assert_eq!(json_null, b"null");
        let EvaluatedArgs::Bytes2(Some(first), Some(second)) =
            prepare_json_binary_pair_args(0x09, &[1, 2, 3], 0xfe, &[9, 8]).unwrap()
        else {
            panic!()
        };
        assert_eq!(first, [0x09, 1, 2, 3]);
        assert_eq!(second, [0xfe, 9, 8]);
        let raw = |value: Json| {
            let mut bytes = vec![value.get_type() as u8];
            bytes.extend_from_slice(value.as_ref().value());
            bytes
        };
        let binary_target = raw(Json::from_i64(2).unwrap());
        let binary_document = raw(Json::from_array(vec![Json::from_i64(1).unwrap()]).unwrap());
        let make_args = |values: &[Option<Vec<u8>>]| match values {
            [first] => EvaluatedArgs::Bytes(first.clone()),
            [first, second] => EvaluatedArgs::Bytes2(first.clone(), second.clone()),
            [first, second, third] => {
                EvaluatedArgs::Bytes3([first.clone(), second.clone(), third.clone()])
            }
            _ => unreachable!(),
        };
        for (operation, values) in [
            (
                EvaluatedBytesOp::JsonContainsSerdeNative,
                vec![b"[1,2]".to_vec(), b"3".to_vec()],
            ),
            (
                EvaluatedBytesOp::JsonContainsPathSerdeNative,
                vec![br#"{"a":[1,2]}"#.to_vec(), b"3".to_vec(), b"$.a".to_vec()],
            ),
            (
                EvaluatedBytesOp::JsonOverlapsSerdeNative,
                vec![b"[1]".to_vec(), b"[2]".to_vec()],
            ),
            (
                EvaluatedBytesOp::JsonMemberOfSerdeNative,
                vec![b"3".to_vec(), b"[1,2]".to_vec()],
            ),
            (
                EvaluatedBytesOp::JsonLengthSerdeNative,
                vec![b"[]".to_vec()],
            ),
            (
                EvaluatedBytesOp::JsonLengthPathSerdeNative,
                vec![br#"{"a":[]}"#.to_vec(), b"$.a".to_vec()],
            ),
            (
                EvaluatedBytesOp::JsonPathExistsSerdeNative,
                vec![br#"{"a":[]}"#.to_vec(), b"$.missing".to_vec()],
            ),
            (
                EvaluatedBytesOp::JsonMemberOfBinaryLegacy,
                vec![binary_target, binary_document],
            ),
        ] {
            let values = values.into_iter().map(Some).collect::<Vec<_>>();
            let arity = values.len();
            let path_slot = match operation {
                EvaluatedBytesOp::JsonContainsPathSerdeNative => Some(2),
                EvaluatedBytesOp::JsonLengthPathSerdeNative
                | EvaluatedBytesOp::JsonPathExistsSerdeNative => Some(1),
                _ => None,
            };
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            let ComputedValue::Int(value) = worker.eval_args(make_args(&values)).unwrap() else {
                panic!()
            };
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.value(), Some(0));
            let mut invalid_cases = Vec::new();
            for slot in 0..arity {
                let mut absent = values.clone();
                absent[slot] = None;
                invalid_cases.push(absent);
                let mut malformed = values.clone();
                malformed[slot] =
                    Some(if operation == EvaluatedBytesOp::JsonMemberOfBinaryLegacy {
                        Vec::new()
                    } else if path_slot == Some(slot) {
                        b"$[".to_vec()
                    } else {
                        b"{".to_vec()
                    });
                invalid_cases.push(malformed);
            }
            if matches!(
                operation,
                EvaluatedBytesOp::JsonContainsPathSerdeNative
                    | EvaluatedBytesOp::JsonLengthPathSerdeNative
            ) {
                let mut multiple = values.clone();
                multiple[path_slot.unwrap()] = Some(b"$[*]".to_vec());
                invalid_cases.push(multiple);
            }
            if operation == EvaluatedBytesOp::JsonMemberOfBinaryLegacy {
                // Keep an actual array header but truncate its child payload:
                // legacy element_count required a full array decode first.
                let mut malformed = values.clone();
                let _ = malformed[1].as_mut().unwrap().pop();
                invalid_cases.push(malformed);
            }
            for invalid in invalid_cases {
                assert!(matches!(
                    worker.eval_args(make_args(&invalid)),
                    Err(LocalError::InvalidBatch(_))
                ));
                let ready = std::array::from_fn(|slot| {
                    if slot < arity {
                        ScalarValue::Bytes(invalid[slot].clone())
                    } else {
                        ScalarValue::Int(None)
                    }
                });
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(ready, arity, &mut reported),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
            }
            for wrong_role in [
                EvaluatedArgs::NullWitness(None),
                EvaluatedArgs::Int2(Some(0), Some(0)),
            ] {
                assert!(matches!(
                    worker.eval_args(wrong_role),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 1);
            let ComputedValue::Int(value) = worker.eval_args(make_args(&values)).unwrap() else {
                panic!()
            };
            assert_eq!(value.value(), Some(0));
            assert_eq!(worker.kernel_invocations(), 2);
            if operation == EvaluatedBytesOp::JsonMemberOfBinaryLegacy {
                // Non-array raw fallback is deliberately not full serde decode.
                let ComputedValue::Int(value) = worker
                    .eval_args(EvaluatedArgs::Bytes2(Some(vec![0x09]), Some(vec![0x09])))
                    .unwrap()
                else {
                    panic!()
                };
                assert_eq!(value.value(), Some(1));
                assert_eq!(worker.kernel_invocations(), 3);
            }
            assert_eq!(worker.retained_storage().unwrap(), storage);
            assert!(worker.is_healthy());
        }
        assert!(
            EvaluatedArgs::Bytes(None).admission_matches(EvaluatedBytesOp::JsonValidTextNative)
        );
        assert!(
            EvaluatedArgs::Bytes2(None, Some(Vec::new()))
                .admission_matches(EvaluatedBytesOp::SqlEncodeNative)
        );
    }

    #[test]
    fn grouping_closed_profiles_keep_unit_metadata_and_actual_null_terminal() {
        use crate::impl_miscellaneous::*;
        for (operation, getter) in [
            (
                EvaluatedBytesOp::GroupingBitAndNative,
                grouping_bit_and_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::GroupingNumericCmpNative,
                grouping_numeric_cmp_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::GroupingNumericSetNative,
                grouping_numeric_set_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::GroupingNullNative,
                grouping_null_native_fn_meta(),
            ),
        ] {
            let terminal = operation == EvaluatedBytesOp::GroupingNullNative;
            let arity = if terminal { 1 } else { 2 };
            assert_eq!(operation.call_count(), 1);
            assert_eq!(operation.eval_type(), EvalType::Int);
            assert_eq!(
                operation.input_types(),
                if terminal {
                    &[EvalType::Int][..]
                } else {
                    &[EvalType::Bytes, EvalType::Bytes][..]
                }
            );
            assert_eq!(
                operation.input_role(),
                if terminal {
                    EvaluatedArgsRole::NullWitness
                } else {
                    EvaluatedArgsRole::Values
                }
            );
            assert!(matches!(
                operation.kernel_kind(),
                EvaluatedKernelKind::ClosedPrivate(_)
            ));
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(program.expression.len(), arity + 1);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[arity]
            else {
                panic!()
            };
            assert_eq!(*args_len, arity);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            if terminal {
                for invalid in [
                    EvaluatedArgs::NullWitness(Some(0)),
                    EvaluatedArgs::Int(None),
                    EvaluatedArgs::NoArgs,
                ] {
                    assert!(matches!(
                        worker.eval_args(invalid),
                        Err(LocalError::InvalidBatch(_))
                    ));
                }
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(
                        [
                            ScalarValue::Int(Some(0)),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                        ],
                        1,
                        &mut reported
                    ),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
                assert_eq!(worker.kernel_invocations(), 0);
            }
            let args = if let Some(mode) = operation.grouping_mode() {
                let metadata = crate::GroupingMetadata::new(mode, Vec::new()).unwrap();
                prepare_grouping_args(u64::MAX, &metadata).unwrap()
            } else {
                EvaluatedArgs::NullWitness(None)
            };
            let ComputedValue::Int(value) = worker.eval_args(args).unwrap() else {
                panic!()
            };
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.value(), if terminal { None } else { Some(0) });
            assert_eq!(worker.kernel_invocations(), 1);
            assert!(worker.is_healthy());
        }
    }

    #[test]
    fn grouping_checked_packing_malformed_roles_and_reused_owned_bits() {
        use crate::{GroupingMetadata, GroupingMode};
        let words = |values: &[u64]| {
            values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect::<Vec<_>>()
        };
        let metadata = GroupingMetadata::new(
            GroupingMode::NumericSet,
            vec![[9, 2, 5, 2].into_iter().collect()],
        )
        .unwrap();
        let EvaluatedArgs::Bytes2(Some(gid), Some(marks)) =
            prepare_grouping_args(u64::MAX, &metadata).unwrap()
        else {
            panic!()
        };
        assert_eq!(gid, u64::MAX.to_le_bytes());
        assert_eq!(marks, words(&[1, 3, 2, 5, 9]));
        for (operation, gid, groups) in [
            (
                EvaluatedBytesOp::GroupingBitAndNative,
                1_u64,
                vec![vec![1_u64], vec![2], vec![4]],
            ),
            (
                EvaluatedBytesOp::GroupingNumericCmpNative,
                2,
                vec![vec![1], vec![2], vec![3]],
            ),
            (
                EvaluatedBytesOp::GroupingNumericSetNative,
                2,
                vec![vec![1, 2], vec![1, 3], vec![]],
            ),
        ] {
            let mode = operation.grouping_mode().unwrap();
            let metadata = GroupingMetadata::new(
                mode,
                groups
                    .into_iter()
                    .map(|group| group.into_iter().collect())
                    .collect(),
            )
            .unwrap();
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            let ComputedValue::Int(value) = worker
                .eval_args(prepare_grouping_args(gid, &metadata).unwrap())
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.value(), Some(3));
            for (gid, marks) in [
                (None, Some(words(&[0]))),
                (Some(words(&[1])), None),
                (None, None),
                (Some(vec![0; 7]), Some(words(&[0]))),
                (Some(vec![0; 9]), Some(words(&[0]))),
                (Some(words(&[1])), Some(Vec::new())),
                (Some(words(&[1])), Some(words(&[1, 1]))),
                (Some(words(&[1])), Some(words(&[0, 0]))),
                (Some(words(&[1])), Some(words(&[u64::MAX]))),
                (Some(words(&[1])), Some(words(&[1, u64::MAX]))),
                (Some(words(&[1])), Some(words(&[1, 2, 1, 1]))),
                (Some(words(&[1])), Some(words(&[1, 2, 2, 1]))),
            ] {
                assert!(matches!(
                    worker.eval_args(EvaluatedArgs::Bytes2(gid.clone(), marks.clone())),
                    Err(LocalError::InvalidBatch(_))
                ));
                let mut reported = None;
                assert!(matches!(
                    worker.eval_ready(
                        [
                            ScalarValue::Bytes(gid),
                            ScalarValue::Bytes(marks),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                            ScalarValue::Int(None),
                        ],
                        2,
                        &mut reported
                    ),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(reported, None);
            }
            for invalid in [
                EvaluatedArgs::NullWitness(None),
                EvaluatedArgs::Int2(Some(0), Some(0)),
                EvaluatedArgs::Ieee754Bits2 {
                    left: ReadyIeee754Arg::Value(Some(0)),
                    right: ReadyIeee754Arg::Value(Some(0)),
                },
                EvaluatedArgs::Bytes3([Some(words(&[0])), Some(words(&[0])), Some(Vec::new())]),
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 1);
            assert!(worker.is_healthy());
            // All 64 output bits remain owned Int bits; no signed narrowing or
            // frontend mask calculation substitutes for the generated wrapper.
            let all_bits = GroupingMetadata::new(
                mode,
                (0..64).map(|_| [0_u64].into_iter().collect()).collect(),
            )
            .unwrap();
            let gid = if mode == GroupingMode::NumericSet {
                1
            } else {
                0
            };
            let ComputedValue::Int(value) = worker
                .eval_args(prepare_grouping_args(gid, &all_bits).unwrap())
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.value(), Some(-1));
            let empty = GroupingMetadata::new(mode, Vec::new()).unwrap();
            let ComputedValue::Int(value) = worker
                .eval_args(prepare_grouping_args(u64::MAX, &empty).unwrap())
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(value.value(), Some(0));
            assert_eq!(worker.kernel_invocations(), 3);
            assert_eq!(worker.retained_storage().unwrap(), storage);
            assert!(worker.is_healthy());
        }
        assert!(
            EvaluatedArgs::Bytes2(None, Some(Vec::new()))
                .admission_matches(EvaluatedBytesOp::SqlEncodeNative)
        );
    }

    #[test]
    fn comparison_profiles_are_closed_unit_calls_with_strict_actual_roles() {
        use crate::{ComparisonOp, impl_compare::*};
        let mut identities: Vec<(EvaluatedBytesOp, crate::FunctionRef, &str)> = Vec::new();
        for predicate in [
            ComparisonOp::Eq,
            ComparisonOp::Ne,
            ComparisonOp::Lt,
            ComparisonOp::Le,
            ComparisonOp::Gt,
            ComparisonOp::Ge,
        ] {
            let profiles = [
                (
                    EvaluatedBytesOp::CompareIntSsNative(predicate),
                    compare_int_ss_native_fn_meta(predicate),
                ),
                (
                    EvaluatedBytesOp::CompareIntSuNative(predicate),
                    compare_int_su_native_fn_meta(predicate),
                ),
                (
                    EvaluatedBytesOp::CompareIntUsNative(predicate),
                    compare_int_us_native_fn_meta(predicate),
                ),
                (
                    EvaluatedBytesOp::CompareIntUuNative(predicate),
                    compare_int_uu_native_fn_meta(predicate),
                ),
                (
                    EvaluatedBytesOp::CompareInt128Legacy(predicate),
                    compare_int128_legacy_fn_meta(predicate),
                ),
                (
                    EvaluatedBytesOp::CompareRealNative(predicate),
                    compare_real_native_fn_meta(predicate),
                ),
                (
                    EvaluatedBytesOp::CompareRealLegacy(predicate),
                    compare_real_legacy_fn_meta(predicate),
                ),
                (
                    EvaluatedBytesOp::CompareDecimalNative(predicate),
                    compare_decimal_native_fn_meta(predicate),
                ),
                (
                    EvaluatedBytesOp::CompareBytesNative(predicate),
                    compare_bytes_native_fn_meta(predicate),
                ),
                (
                    EvaluatedBytesOp::CompareVectorNative(predicate),
                    compare_vector_native_fn_meta(predicate),
                ),
                (
                    EvaluatedBytesOp::CompareTimeCoreNative(predicate),
                    compare_time_core_native_fn_meta(predicate),
                ),
                (
                    EvaluatedBytesOp::CompareDurationNative(predicate),
                    compare_duration_native_fn_meta(predicate),
                ),
                (
                    EvaluatedBytesOp::CompareJsonNative(predicate),
                    compare_json_native_fn_meta(predicate),
                ),
            ];
            for (operation, getter) in profiles {
                for (prior, function, name) in &identities {
                    assert_ne!(*prior, operation);
                    assert_ne!(*function, operation.function_ref());
                    assert_ne!(*name, getter.name);
                }
                identities.push((operation, operation.function_ref(), getter.name));
                assert_eq!(operation.comparison_op(), Some(predicate));
                assert_eq!(operation.call_count(), 1);
                assert_eq!(operation.eval_type(), EvalType::Int);
                assert!(matches!(
                    operation.kernel_kind(),
                    EvaluatedKernelKind::ClosedPrivate(_)
                ));
                let program =
                    compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
                let arity = operation.input_types().len();
                assert_eq!(program.expression.len(), arity + 1);
                assert!(program.check_entry(ProgramEntry::Row).is_err());
                let RpnExpressionNode::FnCall {
                    func_meta,
                    metadata,
                    args_len,
                    ..
                } = &program.expression[arity]
                else {
                    panic!("missing comparison wrapper")
                };
                assert_eq!(*args_len, arity);
                assert!(metadata.is::<()>());
                assert_eq!(func_meta.name, getter.name);
                assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
                assert!(std::ptr::fn_addr_eq(
                    func_meta.validator_ptr,
                    getter.validator_ptr
                ));
                assert!(std::ptr::fn_addr_eq(
                    func_meta.metadata_ptr,
                    getter.metadata_ptr
                ));
                let spec = LocalExpr::Call {
                    function: operation.function_ref(),
                    args: program
                        .schema
                        .iter()
                        .enumerate()
                        .map(|(slot, field_type)| LocalExpr::InputSlot {
                            slot,
                            field_type: field_type.clone(),
                        })
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                    return_type: operation.return_type(),
                    metadata: crate::CallMetadata::None,
                };
                assert!(
                    compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err()
                );
                let args = |null_slot: Option<usize>| {
                    let left = null_slot != Some(0);
                    let right = null_slot != Some(1);
                    match operation {
                        EvaluatedBytesOp::CompareIntSsNative(_)
                        | EvaluatedBytesOp::CompareIntSuNative(_)
                        | EvaluatedBytesOp::CompareIntUsNative(_)
                        | EvaluatedBytesOp::CompareIntUuNative(_)
                        | EvaluatedBytesOp::CompareDurationNative(_) => {
                            EvaluatedArgs::Int2(left.then_some(7), right.then_some(7))
                        }
                        EvaluatedBytesOp::CompareInt128Legacy(_) => EvaluatedArgs::Int1282(
                            left.then_some(i128::MAX),
                            right.then_some(i128::MAX),
                        ),
                        EvaluatedBytesOp::CompareRealNative(_)
                        | EvaluatedBytesOp::CompareRealLegacy(_) => EvaluatedArgs::Ieee754Bits2 {
                            left: ReadyIeee754Arg::Value(left.then_some(1.0_f64.to_bits())),
                            right: ReadyIeee754Arg::Value(right.then_some(1.0_f64.to_bits())),
                        },
                        EvaluatedBytesOp::CompareDecimalNative(_) => EvaluatedArgs::Decimal2 {
                            left: left.then(|| "1.50".parse().unwrap()),
                            right: right.then(|| "1.5".parse().unwrap()),
                        },
                        EvaluatedBytesOp::CompareBytesNative(_) => EvaluatedArgs::CollatedBytes2 {
                            left: left.then(|| b"a".to_vec()),
                            right: right.then(|| b"a".to_vec()),
                            collation: NativeCollation::Binary,
                        },
                        EvaluatedBytesOp::CompareVectorNative(_) => EvaluatedArgs::NativeVector2(
                            left.then(|| NativeVectorFloat32::must_create(vec![1.0])),
                            right.then(|| NativeVectorFloat32::must_create(vec![1.0])),
                        ),
                        EvaluatedBytesOp::CompareTimeCoreNative(_) => {
                            EvaluatedArgs::TimeCoreBits2(left.then_some(0), right.then_some(0))
                        }
                        // Binary JSON null is an actual value, not SQL NULL.
                        EvaluatedBytesOp::CompareJsonNative(_) => EvaluatedArgs::Bytes2(
                            left.then(|| vec![0x04, 0x00]),
                            right.then(|| vec![0x04, 0x00]),
                        ),
                        _ => unreachable!(),
                    }
                };
                assert_eq!(args(None).role(), operation.input_role());
                assert_eq!(args(None).input_types(), operation.input_types());
                let mut worker = prepare_evaluated_bytes(
                    operation,
                    LocalCompileContext::default(),
                    ExecutionLimits::default(),
                    usize::MAX,
                )
                .unwrap();
                let storage = worker.retained_storage().unwrap();
                for null_slot in 0..2 {
                    assert!(matches!(
                        worker.eval_args(args(Some(null_slot))),
                        Err(LocalError::InvalidBatch(_))
                    ));
                    let (ready, ready_arity, invocation) =
                        args(Some(null_slot)).into_values(4096).unwrap();
                    assert!(invocation.is_none());
                    let mut reported = None;
                    assert!(matches!(
                        worker.eval_ready(ready, ready_arity, &mut reported),
                        Err(LocalError::InvalidSpec(_))
                    ));
                    assert_eq!(reported, None);
                }
                if matches!(
                    operation,
                    EvaluatedBytesOp::CompareRealNative(_) | EvaluatedBytesOp::CompareRealLegacy(_)
                ) {
                    for (left, right) in [
                        (ReadyIeee754Arg::Undemanded, ReadyIeee754Arg::Value(Some(0))),
                        (ReadyIeee754Arg::Value(Some(0)), ReadyIeee754Arg::Undemanded),
                    ] {
                        assert!(matches!(
                            worker.eval_args(EvaluatedArgs::Ieee754Bits2 { left, right }),
                            Err(LocalError::InvalidBatch(_))
                        ));
                    }
                }
                assert!(matches!(
                    worker.eval_args(EvaluatedArgs::NullWitness(None)),
                    Err(LocalError::InvalidBatch(_))
                ));
                let wrong_role = if matches!(operation, EvaluatedBytesOp::CompareJsonNative(_)) {
                    EvaluatedArgs::Int2(Some(1), Some(1))
                } else {
                    EvaluatedArgs::Bytes2(Some(vec![0; 8]), Some(vec![0; 8]))
                };
                assert!(matches!(
                    worker.eval_args(wrong_role),
                    Err(LocalError::InvalidBatch(_))
                ));
                assert_eq!(worker.kernel_invocations(), 0);
                let ComputedValue::Int(value) = worker.eval_args(args(None)).unwrap() else {
                    panic!("comparison must return an owned integer")
                };
                assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
                assert_eq!(
                    value.value(),
                    Some(i64::from(matches!(
                        predicate,
                        ComparisonOp::Eq | ComparisonOp::Le | ComparisonOp::Ge
                    )))
                );
                assert_eq!(worker.kernel_invocations(), 1);
                assert_eq!(worker.retained_storage().unwrap(), storage);
                assert!(worker.is_healthy());
            }
        }
        assert_eq!(identities.len(), 78);
        for (operation, args, getter) in [
            (
                EvaluatedBytesOp::CompareNullNative,
                EvaluatedArgs::NullWitness(None),
                compare_null_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::CompareMissingLegacy,
                EvaluatedArgs::NoArgs,
                compare_missing_legacy_fn_meta(),
            ),
        ] {
            assert_eq!(operation.comparison_op(), None);
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            let arity = operation.input_types().len();
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[arity]
            else {
                panic!("missing comparison terminal")
            };
            assert_eq!(*args_len, arity);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            for invalid in [
                EvaluatedArgs::Int(None),
                EvaluatedArgs::NullWitness(Some(0)),
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            let opposite_terminal = if operation == EvaluatedBytesOp::CompareNullNative {
                EvaluatedArgs::NoArgs
            } else {
                EvaluatedArgs::NullWitness(None)
            };
            assert!(matches!(
                worker.eval_args(opposite_terminal),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            let ComputedValue::Int(value) = worker.eval_args(args).unwrap() else {
                panic!()
            };
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.value(), None);
            assert_eq!(worker.kernel_invocations(), 1);
        }
        // Do not narrow the pre-existing nullable value recipes.
        assert!(
            EvaluatedArgs::Int2(None, Some(1)).admission_matches(EvaluatedBytesOp::AddIntSsNative)
        );
        assert!(
            EvaluatedArgs::Decimal2 {
                left: None,
                right: Some("1".parse().unwrap())
            }
            .admission_matches(EvaluatedBytesOp::AddDecimalNative)
        );
        assert!(
            EvaluatedArgs::Bytes2(None, Some(Vec::new()))
                .admission_matches(EvaluatedBytesOp::SqlEncodeNative)
        );
    }

    #[test]
    fn comparison_actual_bits_profiles_and_owned_boolean_results() {
        use crate::ComparisonOp;
        let run = |operation, args, expected| {
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let ComputedValue::Int(value) = worker.eval_args(args).unwrap() else {
                panic!()
            };
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.value(), Some(expected));
            assert_eq!(worker.kernel_invocations(), 1);
            assert!(worker.is_healthy());
        };
        for (predicate, less, equal, greater, unordered) in [
            (ComparisonOp::Eq, 0, 1, 0, 0),
            (ComparisonOp::Ne, 1, 0, 1, 1),
            (ComparisonOp::Lt, 1, 0, 0, 0),
            (ComparisonOp::Le, 1, 1, 0, 0),
            (ComparisonOp::Gt, 0, 0, 1, 0),
            (ComparisonOp::Ge, 0, 1, 1, 0),
        ] {
            run(
                EvaluatedBytesOp::CompareIntSsNative(predicate),
                EvaluatedArgs::Int2(Some(-1), Some(1)),
                less,
            );
            run(
                EvaluatedBytesOp::CompareIntSuNative(predicate),
                EvaluatedArgs::Int2(Some(-1), Some(1)),
                less,
            );
            run(
                EvaluatedBytesOp::CompareIntUsNative(predicate),
                EvaluatedArgs::Int2(Some(-1), Some(1)),
                greater,
            );
            run(
                EvaluatedBytesOp::CompareIntUuNative(predicate),
                EvaluatedArgs::Int2(Some(-1), Some(-2)),
                greater,
            );
            run(
                EvaluatedBytesOp::CompareInt128Legacy(predicate),
                EvaluatedArgs::Int1282(Some(i128::MAX), Some(i128::MIN)),
                greater,
            );
            let real = |left: f64, right: f64| EvaluatedArgs::Ieee754Bits2 {
                left: ReadyIeee754Arg::Value(Some(left.to_bits())),
                right: ReadyIeee754Arg::Value(Some(right.to_bits())),
            };
            run(
                EvaluatedBytesOp::CompareRealNative(predicate),
                real(f64::NAN, f64::NAN),
                unordered,
            );
            run(
                EvaluatedBytesOp::CompareRealLegacy(predicate),
                real(f64::NAN, f64::NAN),
                equal,
            );
            run(
                EvaluatedBytesOp::CompareRealNative(predicate),
                real(-0.0, 0.0),
                equal,
            );
            run(
                EvaluatedBytesOp::CompareRealLegacy(predicate),
                real(-0.0, 0.0),
                less,
            );
            run(EvaluatedBytesOp::CompareDecimalNative(predicate), EvaluatedArgs::Decimal2 {
                left: Some(Decimal::try_from_native_digits(false, b"100000000000000000000000000000000000000000000000000000000000000000000000000000000001", 0, 0, 4096).unwrap()),
                right: Some(Decimal::try_from_native_digits(false, b"100000000000000000000000000000000000000000000000000000000000000000000000000000000000", 0, 0, 4096).unwrap()),
            }, greater);
            run(
                EvaluatedBytesOp::CompareBytesNative(predicate),
                EvaluatedArgs::CollatedBytes2 {
                    left: Some(b"a".to_vec()),
                    right: Some(b"A".to_vec()),
                    collation: NativeCollation::Binary,
                },
                greater,
            );
            run(
                EvaluatedBytesOp::CompareVectorNative(predicate),
                EvaluatedArgs::NativeVector2(
                    Some(NativeVectorFloat32::must_create(vec![1.0, 2.0])),
                    Some(NativeVectorFloat32::must_create(vec![1.0, 3.0])),
                ),
                less,
            );
            run(
                EvaluatedBytesOp::CompareTimeCoreNative(predicate),
                EvaluatedArgs::TimeCoreBits2(Some(1), Some(15)),
                equal,
            );
            run(
                EvaluatedBytesOp::CompareDurationNative(predicate),
                EvaluatedArgs::Int2(Some(i64::MIN), Some(i64::MAX)),
                less,
            );
            run(
                EvaluatedBytesOp::CompareJsonNative(predicate),
                EvaluatedArgs::Bytes2(
                    Some(vec![0x09, 1, 0, 0, 0, 0, 0, 0, 0]),
                    Some(vec![0x09, 2, 0, 0, 0, 0, 0, 0, 0]),
                ),
                less,
            );
        }
    }

    #[test]
    fn aes_dispatch_all_private_unit_profiles_and_nonnull_roles() {
        use crate::impl_encryption::*;
        let cases = [
            (
                EvaluatedBytesOp::AesEncrypt128EcbNative,
                aes_encrypt_128_ecb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesEncrypt192EcbNative,
                aes_encrypt_192_ecb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesEncrypt256EcbNative,
                aes_encrypt_256_ecb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesDecrypt128EcbNative,
                aes_decrypt_128_ecb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesDecrypt192EcbNative,
                aes_decrypt_192_ecb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesDecrypt256EcbNative,
                aes_decrypt_256_ecb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesEncrypt128CbcNative,
                aes_encrypt_128_cbc_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesEncrypt192CbcNative,
                aes_encrypt_192_cbc_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesEncrypt256CbcNative,
                aes_encrypt_256_cbc_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesDecrypt128CbcNative,
                aes_decrypt_128_cbc_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesDecrypt192CbcNative,
                aes_decrypt_192_cbc_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesDecrypt256CbcNative,
                aes_decrypt_256_cbc_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesEncrypt128OfbNative,
                aes_encrypt_128_ofb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesEncrypt192OfbNative,
                aes_encrypt_192_ofb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesEncrypt256OfbNative,
                aes_encrypt_256_ofb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesDecrypt128OfbNative,
                aes_decrypt_128_ofb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesDecrypt192OfbNative,
                aes_decrypt_192_ofb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesDecrypt256OfbNative,
                aes_decrypt_256_ofb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesEncrypt128CfbNative,
                aes_encrypt_128_cfb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesEncrypt192CfbNative,
                aes_encrypt_192_cfb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesEncrypt256CfbNative,
                aes_encrypt_256_cfb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesDecrypt128CfbNative,
                aes_decrypt_128_cfb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesDecrypt192CfbNative,
                aes_decrypt_192_cfb_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::AesDecrypt256CfbNative,
                aes_decrypt_256_cfb_native_fn_meta(),
            ),
            (EvaluatedBytesOp::AesNullNative, aes_null_native_fn_meta()),
        ];
        for (operation, getter) in cases {
            let terminal = operation == EvaluatedBytesOp::AesNullNative;
            let arity = operation.input_types().len();
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            assert_eq!(operation.call_count(), 1);
            assert_eq!(operation.eval_type(), EvalType::Bytes);
            assert_eq!(
                operation.input_role(),
                if terminal {
                    EvaluatedArgsRole::NullWitness
                } else {
                    EvaluatedArgsRole::Values
                }
            );
            assert_eq!(
                arity,
                if terminal {
                    1
                } else if operation.aes_error_profile().is_some() {
                    3
                } else {
                    2
                }
            );
            assert_eq!(program.expression.len(), arity + 1);
            assert!(program.check_entry(ProgramEntry::Row).is_err());
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[arity]
            else {
                panic!("missing AES generated wrapper")
            };
            assert_eq!(*args_len, arity);
            assert!(metadata.is::<()>());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            if terminal {
                for invalid in [
                    EvaluatedArgs::NullWitness(Some(0)),
                    EvaluatedArgs::Int(None),
                    EvaluatedArgs::NoArgs,
                ] {
                    assert!(matches!(
                        worker.eval_args(invalid),
                        Err(LocalError::InvalidBatch(_))
                    ));
                }
            } else {
                for null_slot in 0..arity {
                    let mut values = [Some(Vec::new()), Some(Vec::new()), Some(vec![0; 16])];
                    values[null_slot] = None;
                    let args = if arity == 2 {
                        EvaluatedArgs::Bytes2(values[0].take(), values[1].take())
                    } else {
                        EvaluatedArgs::Bytes3(values)
                    };
                    assert!(matches!(
                        worker.eval_args(args),
                        Err(LocalError::InvalidBatch(_))
                    ));
                    let mut ready = [
                        ScalarValue::Bytes(Some(Vec::new())),
                        ScalarValue::Bytes(Some(Vec::new())),
                        ScalarValue::Bytes(Some(vec![0; 16])),
                        ScalarValue::Int(None),
                        ScalarValue::Int(None),
                        ScalarValue::Int(None),
                    ];
                    ready[null_slot] = ScalarValue::Bytes(None);
                    let mut reported = None;
                    assert!(matches!(
                        worker.eval_ready(ready, arity, &mut reported),
                        Err(LocalError::InvalidSpec(_))
                    ));
                    assert_eq!(reported, None);
                }
                assert!(matches!(
                    worker.eval_args(EvaluatedArgs::Ieee754Bits2 {
                        left: ReadyIeee754Arg::Value(Some(0)),
                        right: ReadyIeee754Arg::Value(Some(0)),
                    }),
                    Err(LocalError::InvalidBatch(_))
                ));
                assert!(matches!(
                    worker.eval_args(EvaluatedArgs::NullWitness(None)),
                    Err(LocalError::InvalidBatch(_))
                ));
                let wrong_arity = if arity == 2 {
                    EvaluatedArgs::Bytes3([Some(Vec::new()), Some(Vec::new()), Some(vec![0; 16])])
                } else {
                    EvaluatedArgs::Bytes2(Some(Vec::new()), Some(Vec::new()))
                };
                assert!(matches!(
                    worker.eval_args(wrong_arity),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 0);
            let args = if terminal {
                EvaluatedArgs::NullWitness(None)
            } else if arity == 2 {
                EvaluatedArgs::Bytes2(Some(Vec::new()), Some(Vec::new()))
            } else {
                EvaluatedArgs::Bytes3([Some(Vec::new()), Some(Vec::new()), Some(vec![0; 16])])
            };
            let ComputedValue::Bytes(value) = worker.eval_args(args).unwrap() else {
                panic!("AES lost owned Bytes")
            };
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            if terminal {
                assert_eq!(value.into_option(), None);
            }
            assert_eq!(worker.kernel_invocations(), 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        // Ordinary nullable Bytes pairs are not narrowed by AES value admission.
        let mut nullable = prepare_evaluated_bytes(
            EvaluatedBytesOp::SqlEncodeNative,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        assert!(
            nullable
                .eval_args(EvaluatedArgs::Bytes2(None, Some(Vec::new())))
                .is_ok()
        );
    }

    #[test]
    fn aes_actual_iv_receipts_budget_reuse_and_fixed_ciphertexts() {
        let prepare = |operation| {
            prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap()
        };
        let operations = [
            EvaluatedBytesOp::AesEncrypt128CbcNative,
            EvaluatedBytesOp::AesEncrypt192CbcNative,
            EvaluatedBytesOp::AesEncrypt256CbcNative,
            EvaluatedBytesOp::AesDecrypt128CbcNative,
            EvaluatedBytesOp::AesDecrypt192CbcNative,
            EvaluatedBytesOp::AesDecrypt256CbcNative,
            EvaluatedBytesOp::AesEncrypt128OfbNative,
            EvaluatedBytesOp::AesEncrypt192OfbNative,
            EvaluatedBytesOp::AesEncrypt256OfbNative,
            EvaluatedBytesOp::AesDecrypt128OfbNative,
            EvaluatedBytesOp::AesDecrypt192OfbNative,
            EvaluatedBytesOp::AesDecrypt256OfbNative,
            EvaluatedBytesOp::AesEncrypt128CfbNative,
            EvaluatedBytesOp::AesEncrypt192CfbNative,
            EvaluatedBytesOp::AesEncrypt256CfbNative,
            EvaluatedBytesOp::AesDecrypt128CfbNative,
            EvaluatedBytesOp::AesDecrypt192CfbNative,
            EvaluatedBytesOp::AesDecrypt256CfbNative,
        ];
        let iv_args = |len| {
            EvaluatedArgs::Bytes3([
                Some(Vec::new()),
                Some(b"password".to_vec()),
                Some(vec![0; len]),
            ])
        };
        for operation in operations {
            let mut worker = prepare(operation);
            let storage = worker.retained_storage().unwrap();
            let mut failure = worker.eval_args_reported(iv_args(15)).unwrap_err();
            assert_eq!(failure.operation(), Some(operation));
            assert_eq!(
                failure.sql_failure(),
                Some(EvaluatedSqlFailureKind::AesNative)
            );
            let cause = failure.native_aes_error().unwrap();
            assert_eq!(
                Some((cause.operation(), cause.profile())),
                operation.aes_profile()
            );
            for wrong in operations.into_iter().chain([
                EvaluatedBytesOp::AesEncrypt128EcbNative,
                EvaluatedBytesOp::AesDecrypt256EcbNative,
                EvaluatedBytesOp::AesNullNative,
                EvaluatedBytesOp::ModRealNative,
            ]) {
                if wrong != operation {
                    failure.operation = Some(wrong);
                    assert!(failure.native_aes_error().is_none());
                }
            }
            failure.operation = Some(operation);
            assert!(failure.native_aes_error().is_some());
            let unreported = ReportedEvaluatedFailure::unreported(failure.into_error());
            assert!(unreported.native_aes_error().is_none());
            assert_eq!(worker.kernel_invocations(), 1);
            assert!(worker.eval_args(iv_args(16)).is_ok());
            assert_eq!(worker.kernel_invocations(), 2);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            let mut zero = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits {
                    max_steps: 0,
                    ..ExecutionLimits::default()
                },
                usize::MAX,
            )
            .unwrap();
            let failure = zero.eval_args_reported(iv_args(0)).unwrap_err();
            assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
            assert_eq!(failure.sql_failure(), None);
            assert!(failure.native_aes_error().is_none());
            assert_eq!(zero.kernel_invocations(), 0);
            assert!(zero.is_healthy());
        }
        // Complete original Go ciphertexts, never a provider-derived oracle.
        let key = b"1234567890123456";
        for (encrypt, decrypt, ciphertext) in [
            (
                EvaluatedBytesOp::AesEncrypt128EcbNative,
                EvaluatedBytesOp::AesDecrypt128EcbNative,
                "697BFE9B3F8C2F289DD82C88C7BC95C4",
            ),
            (
                EvaluatedBytesOp::AesEncrypt128CbcNative,
                EvaluatedBytesOp::AesDecrypt128CbcNative,
                "2ECA0077C5EA5768A0485AA522774792",
            ),
        ] {
            let expected = hex::decode(ciphertext).unwrap();
            let args = |data: Vec<u8>| {
                if encrypt.input_types().len() == 2 {
                    EvaluatedArgs::Bytes2(Some(data), Some(key.to_vec()))
                } else {
                    EvaluatedArgs::Bytes3([
                        Some(data),
                        Some(key.to_vec()),
                        Some(b"1234567890123456ignored".to_vec()),
                    ])
                }
            };
            let mut worker = prepare(encrypt);
            let ComputedValue::Bytes(value) = worker.eval_args(args(b"pingcap".to_vec())).unwrap()
            else {
                panic!("AES encrypt lost owned Bytes")
            };
            assert_eq!(value.value(), Some(expected.as_slice()));
            drop(worker);
            assert_eq!(value.into_option(), Some(expected.clone()));
            let mut worker = prepare(decrypt);
            let ComputedValue::Bytes(value) = worker.eval_args(args(expected)).unwrap() else {
                panic!("AES decrypt lost owned Bytes")
            };
            assert_eq!(value.into_option(), Some(b"pingcap".to_vec()));
            let ComputedValue::Bytes(value) =
                worker.eval_args_reported(args(b"short".to_vec())).unwrap()
            else {
                panic!("cipher rejection must remain successful Bytes NULL")
            };
            assert_eq!(value.into_option(), None);
            assert!(worker.is_healthy());
        }
        // NIST SP 800-38A first block is identical for OFB/CFB with this IV.
        let plaintext = hex::decode("6bc1bee22e409f96e93d7e117393172a").unwrap();
        let ciphertext = hex::decode("3b3fd92eb72dad20333449f8e83cfb4a").unwrap();
        let key = hex::decode("2b7e151628aed2a6abf7158809cf4f3c").unwrap();
        let iv = hex::decode("000102030405060708090a0b0c0d0e0f").unwrap();
        for (operation, input, expected) in [
            (
                EvaluatedBytesOp::AesEncrypt128OfbNative,
                &plaintext,
                &ciphertext,
            ),
            (
                EvaluatedBytesOp::AesEncrypt128CfbNative,
                &plaintext,
                &ciphertext,
            ),
            (
                EvaluatedBytesOp::AesDecrypt128OfbNative,
                &ciphertext,
                &plaintext,
            ),
            (
                EvaluatedBytesOp::AesDecrypt128CfbNative,
                &ciphertext,
                &plaintext,
            ),
        ] {
            let ComputedValue::Bytes(value) = prepare(operation)
                .eval_args(EvaluatedArgs::Bytes3([
                    Some(input.clone()),
                    Some(key.clone()),
                    Some(iv.clone()),
                ]))
                .unwrap()
            else {
                panic!("AES stream mode lost Bytes")
            };
            assert_eq!(value.value(), Some(expected.as_slice()));
        }
    }

    #[test]
    fn division_dispatch_profiles_nonnull_roles_and_real_receipts() {
        let cases = [
            (
                EvaluatedBytesOp::DivRealNative,
                crate::impl_arithmetic::div_real_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::DivRealLegacy,
                crate::impl_arithmetic::div_real_legacy_fn_meta(),
            ),
            (
                EvaluatedBytesOp::DivDecimalNative,
                crate::impl_arithmetic::div_decimal_native_fn_meta(),
            ),
            (
                EvaluatedBytesOp::DivDecimalLegacy,
                crate::impl_arithmetic::div_decimal_legacy_fn_meta(),
            ),
        ];
        for (operation, getter) in cases {
            let program =
                compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
            let decimal = operation.decimal_division_kind().is_some();
            assert_eq!(
                operation.input_role(),
                if decimal {
                    EvaluatedArgsRole::DecimalDivision
                } else {
                    EvaluatedArgsRole::Ieee754Bits2
                }
            );
            assert_eq!(
                operation.input_types(),
                if decimal {
                    &[EvalType::Decimal, EvalType::Decimal, EvalType::Int][..]
                } else {
                    &[EvalType::Bytes, EvalType::Bytes][..]
                }
            );
            assert_eq!(
                operation.eval_type(),
                if decimal {
                    EvalType::Decimal
                } else {
                    EvalType::Bytes
                }
            );
            assert_eq!(operation.call_count(), 1);
            assert_eq!(program.expression.len(), operation.input_types().len() + 1);
            let RpnExpressionNode::FnCall {
                func_meta,
                metadata,
                args_len,
                ..
            } = &program.expression[operation.input_types().len()]
            else {
                panic!("missing DIV wrapper")
            };
            assert_eq!(*args_len, operation.input_types().len());
            assert_eq!(func_meta.name, getter.name);
            assert!(std::ptr::fn_addr_eq(func_meta.fn_ptr, getter.fn_ptr));
            assert!(std::ptr::fn_addr_eq(
                func_meta.metadata_ptr,
                getter.metadata_ptr
            ));
            assert!(std::ptr::fn_addr_eq(
                func_meta.validator_ptr,
                getter.validator_ptr
            ));
            assert!(operation.metadata_matches(metadata.as_ref()));
            if let Some(kind) = operation.decimal_division_kind() {
                let payload = metadata
                    .downcast_ref::<NativeDecimalDivisionCallMetadata>()
                    .unwrap();
                assert_eq!(payload.kind, kind);
                assert!(payload.is_unbound());
            } else {
                assert!(metadata.is::<()>());
            }
            let spec = LocalExpr::Call {
                function: operation.function_ref(),
                args: program
                    .schema
                    .iter()
                    .enumerate()
                    .map(|(slot, field_type)| LocalExpr::InputSlot {
                        slot,
                        field_type: field_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                return_type: operation.return_type(),
                metadata: crate::CallMetadata::None,
            };
            assert!(compile_local(&spec, &program.schema, LocalCompileContext::default()).is_err());
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            for (left, right) in [(None, Some(2i64)), (Some(6), None), (None, None)] {
                let args = if decimal {
                    EvaluatedArgs::DecimalDivision {
                        left: left.map(Decimal::from),
                        right: right.map(Decimal::from),
                        frac_increment: 4,
                    }
                } else {
                    EvaluatedArgs::Ieee754Bits2 {
                        left: ReadyIeee754Arg::Value(left.map(|v| (v as f64).to_bits())),
                        right: ReadyIeee754Arg::Value(right.map(|v| (v as f64).to_bits())),
                    }
                };
                assert!(matches!(
                    worker.eval_args(args),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            for invalid in [
                EvaluatedArgs::Decimal2 {
                    left: Some(Decimal::from(6i64)),
                    right: Some(Decimal::from(2i64)),
                },
                EvaluatedArgs::Bytes2(Some(vec![0; 8]), Some(vec![0; 8])),
                EvaluatedArgs::NullWitness(None),
                EvaluatedArgs::Ieee754Bits2 {
                    left: ReadyIeee754Arg::Undemanded,
                    right: ReadyIeee754Arg::Value(Some(0)),
                },
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            // Raw ready transport must independently retain strict non-NULL admission.
            let ready = if decimal {
                vec![
                    ScalarValue::Decimal(None),
                    ScalarValue::Decimal(Some(Decimal::from(2i64))),
                    ScalarValue::Int(Some(4096)),
                ]
            } else {
                vec![
                    ScalarValue::Bytes(None),
                    ScalarValue::Bytes(Some(2f64.to_bits().to_le_bytes().to_vec())),
                ]
            };
            let mut ctx = EvalContext::default();
            let mut witness = EvaluatedAsciiWitness::default();
            let mut budget = EvalBudget::exact(ExecutionLimits::default()).unwrap();
            assert!(
                program
                    .expression
                    .eval_with_ready_args(
                        operation,
                        &mut ctx,
                        &program.schema,
                        &ready,
                        operation.input_role(),
                        &[0],
                        &mut witness,
                        &mut budget
                    )
                    .is_err()
            );
            assert_eq!(witness.invocations(), 0);
        }
        let real = |left: f64, right: f64| EvaluatedArgs::Ieee754Bits2 {
            left: ReadyIeee754Arg::Value(Some(left.to_bits())),
            right: ReadyIeee754Arg::Value(Some(right.to_bits())),
        };
        for operation in [
            EvaluatedBytesOp::DivRealNative,
            EvaluatedBytesOp::DivRealLegacy,
        ] {
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            for (left, right, expected) in [
                (6.0, 2.0, Some(3f64.to_bits())),
                (-0.0, 2.0, Some((-0f64).to_bits())),
                (f64::NAN, -0.0, None),
            ] {
                let ComputedValue::Ieee754Bits(value) =
                    worker.eval_args(real(left, right)).unwrap()
                else {
                    panic!("DIV real lost IEEE result")
                };
                assert_eq!(value.into_option(), expected);
            }
            if operation == EvaluatedBytesOp::DivRealNative {
                let mut failure = worker.eval_args_reported(real(f64::MAX, 0.5)).unwrap_err();
                assert_eq!(
                    failure.native_binary_arithmetic_error(),
                    Some(&NativeBinaryArithmeticError {
                        operation: BinaryArithmeticOperation::Divide,
                        kind: BinaryArithmeticErrorKind::FloatOverflow,
                    })
                );
                for wrong in [
                    EvaluatedBytesOp::ModRealNative,
                    EvaluatedBytesOp::DivRealLegacy,
                    EvaluatedBytesOp::DivDecimalNative,
                    EvaluatedBytesOp::DivDecimalLegacy,
                ] {
                    failure.operation = Some(wrong);
                    assert!(failure.native_binary_arithmetic_error().is_none());
                }
            } else {
                let ComputedValue::Ieee754Bits(value) =
                    worker.eval_args(real(f64::MAX, 0.5)).unwrap()
                else {
                    panic!("legacy DIV lost raw infinity")
                };
                assert_eq!(value.into_option(), Some(f64::INFINITY.to_bits()));
            }
            assert!(worker.eval_args(real(6.0, 2.0)).is_ok());
            assert_eq!(worker.kernel_invocations(), 5);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }

    #[test]
    fn division_decimal_reports_binding_cleanup_budget_and_reuse() {
        use NativeDecimalDivisionDisposition::{Ok as Exact, Overflow, Truncated, ZeroDivisor};
        use NativeDecimalDivisionKind::{Legacy, Native};
        let prepare = |operation| {
            prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap()
        };
        let input = |left, right, frac_increment| EvaluatedArgs::DecimalDivision {
            left: Some(left),
            right: Some(right),
            frac_increment,
        };
        let lhs = || Decimal::try_from_native_digits(false, b"10", 1, 1, 4096).unwrap();
        for operation in [
            EvaluatedBytesOp::DivDecimalNative,
            EvaluatedBytesOp::DivDecimalLegacy,
        ] {
            let mut worker = prepare(operation);
            let storage = worker.retained_storage().unwrap();
            let mut retained = None;
            for (increment, divisor, status, scale) in [
                (4, 3i64, Exact, Some(5)),
                (0, 3, Exact, Some(1)),
                (100, 3, Truncated, None),
                (u32::MAX, 0, ZeroDivisor, None),
                (4, 3, Exact, Some(5)),
            ] {
                let ComputedValue::DecimalDivision(value) = worker
                    .eval_args(input(lhs(), Decimal::from(divisor), increment))
                    .unwrap()
                else {
                    panic!("DIV Decimal lost its report carrier")
                };
                assert_eq!(
                    value.metadata(),
                    ComputedDecimalDivisionMetadata::OwnDecimalDivision
                );
                assert_eq!(value.disposition(), status);
                assert_eq!(value.value().is_none(), status == ZeroDivisor);
                if let Some(scale) = scale {
                    assert_eq!(value.value().unwrap().result_scale(), scale);
                }
                let (value, actual) = value.into_parts();
                assert_eq!(actual, status);
                retained = value;
                assert!(worker.decimal_division_metadata().unwrap().is_unbound());
                assert!(worker.is_healthy());
                assert_eq!(worker.retained_storage().unwrap(), storage);
            }
            let wide = Decimal::try_from_native_digits(true, &[b'9'; 82], 0, 0, 4096).unwrap();
            let ComputedValue::DecimalDivision(value) = worker
                .eval_args(input(wide, Decimal::from(1i64), 0))
                .unwrap()
            else {
                panic!("DIV overflow is a successful report, not SQL error")
            };
            assert_eq!(value.disposition(), Overflow);
            assert!(value.value().is_some());
            let calls = worker.kernel_invocations();
            let limits = worker.state.limits;
            worker.state.limits.max_steps = 0;
            let failure = worker
                .eval_args_reported(input(lhs(), Decimal::from(3i64), 4))
                .unwrap_err();
            assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
            assert_eq!(failure.sql_failure(), None);
            assert_eq!(worker.kernel_invocations(), calls);
            assert!(worker.decimal_division_metadata().unwrap().is_unbound());
            assert!(worker.is_healthy());
            worker.state.limits = limits;
            // The real third slot receives zero after accounting for the worker.
            worker.state.limits.max_retained_bytes = storage.total_bytes();
            let failure = worker
                .eval_args_reported(input(lhs(), Decimal::from(3i64), 4))
                .unwrap_err();
            assert!(matches!(failure.error(), LocalError::Evaluation(_)));
            assert_eq!(failure.operation(), None);
            assert_eq!(failure.sql_failure(), None);
            assert!(failure.native_binary_arithmetic_error().is_none());
            assert_eq!(worker.kernel_invocations(), calls + 1);
            assert!(worker.decimal_division_metadata().unwrap().is_unbound());
            assert!(worker.is_healthy());
            worker.state.limits = limits;
            assert!(
                worker
                    .eval_args(input(lhs(), Decimal::from(3i64), 4))
                    .is_ok()
            );
            assert_eq!(worker.retained_storage().unwrap(), storage);
            drop(worker);
            assert_eq!(retained.unwrap().result_scale(), 5);
        }
        let wide = || Decimal::try_from_native_digits(false, &[b'9'; 90], 0, 0, 4096).unwrap();
        let (left, right) = (wide(), wide());
        let live = left.spill_capacity_bytes() + right.spill_capacity_bytes();
        let (ready, arity, _) = input(left, right, 7).into_values(live + 64).unwrap();
        assert_eq!(arity, 3);
        assert_eq!(ready[2], ScalarValue::Int(Some(64)));
        for bad in 0..5 {
            let metadata = NativeDecimalDivisionCallMetadata::new(Native);
            metadata.bind(7).unwrap();
            match bad {
                0 => assert!(metadata.begin_kernel(Legacy).is_err()),
                1 => {
                    metadata.begin_kernel(Native).unwrap();
                    assert!(metadata.begin_kernel(Native).is_err());
                }
                2 => {
                    metadata.begin_kernel(Native).unwrap();
                    metadata.record_disposition(Exact).unwrap();
                    assert!(metadata.record_error().is_err());
                }
                3 => assert!(metadata.consume(Some(1), true).is_err()),
                _ => {
                    metadata.begin_kernel(Native).unwrap();
                    metadata.record_disposition(Exact).unwrap();
                    assert!(metadata.consume(Some(0), true).is_err());
                }
            }
            assert!(
                metadata.unbind().is_err(),
                "invalid report must not silently become reusable"
            );
        }
        let metadata = NativeDecimalDivisionCallMetadata::new(Native);
        metadata.bind(9).unwrap();
        assert_eq!(metadata.begin_kernel(Native).unwrap(), 9);
        metadata.record_error().unwrap();
        assert_eq!(metadata.consume(Some(1), false).unwrap(), None);
        metadata.finish(Some(1), false).unwrap();
        metadata.unbind().unwrap();
        assert!(metadata.is_unbound());
        let mut worker = prepare(EvaluatedBytesOp::DivDecimalNative);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            worker.begin_invocation().unwrap();
            let guard = DecimalDivisionBindingGuard {
                worker: &mut worker,
            };
            guard
                .worker
                .decimal_division_metadata()
                .unwrap()
                .bind(4)
                .unwrap();
            guard
                .worker
                .decimal_division_metadata()
                .unwrap()
                .begin_kernel(Native)
                .unwrap();
            panic!("simulated kernel unwind");
        }));
        assert!(panic.is_err());
        assert!(worker.decimal_division_metadata().unwrap().is_unbound());
        assert!(
            !worker.is_healthy(),
            "unwind cleanup must not clear worker poison"
        );
    }

    #[test]
    fn modulo_value_dispatch_nonnull_zero_nonfinite_and_reuse() {
        let prepare = |operation| {
            prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap()
        };
        let args = |operation, left: Option<i64>, right: Option<i64>| match operation {
            EvaluatedBytesOp::ModInt128Legacy => {
                EvaluatedArgs::Int1282(left.map(i128::from), right.map(i128::from))
            }
            EvaluatedBytesOp::ModRealNative | EvaluatedBytesOp::ModRealLegacy => {
                EvaluatedArgs::Ieee754Bits2 {
                    left: ReadyIeee754Arg::Value(left.map(|value| (value as f64).to_bits())),
                    right: ReadyIeee754Arg::Value(right.map(|value| (value as f64).to_bits())),
                }
            }
            EvaluatedBytesOp::ModDecimalNative => EvaluatedArgs::Decimal2 {
                left: left.map(Decimal::from),
                right: right.map(Decimal::from),
            },
            _ => EvaluatedArgs::Int2(left, right),
        };
        let operations = [
            EvaluatedBytesOp::ModIntSsNative,
            EvaluatedBytesOp::ModIntSuNative,
            EvaluatedBytesOp::ModIntUsNative,
            EvaluatedBytesOp::ModIntUuNative,
            EvaluatedBytesOp::ModInt128Legacy,
            EvaluatedBytesOp::ModRealNative,
            EvaluatedBytesOp::ModRealLegacy,
            EvaluatedBytesOp::ModDecimalNative,
        ];
        for operation in operations {
            let mut worker = prepare(operation);
            let storage = worker.retained_storage().unwrap();
            for (left, right) in [(None, Some(5)), (Some(17), None), (None, None)] {
                let failure = worker
                    .eval_args_reported(args(operation, left, right))
                    .unwrap_err();
                assert!(matches!(failure.error(), LocalError::InvalidBatch(_)));
                assert_eq!(failure.sql_failure(), None);
                // The official driver's readiness check independently rejects
                // forged nullable transport, not merely the public facade.
                let (ready, arity, _) = args(operation, left, right).into_values(4096).unwrap();
                let mut sql_failure = None;
                assert!(matches!(
                    worker.eval_ready(ready, arity, &mut sql_failure),
                    Err(LocalError::InvalidSpec(_))
                ));
                assert_eq!(sql_failure, None);
                assert_eq!(worker.kernel_invocations(), 0);
            }
            for invalid in [
                EvaluatedArgs::Bytes2(Some(vec![0; 8]), Some(vec![0; 8])),
                EvaluatedArgs::NullWitness(None),
                EvaluatedArgs::NoArgs,
                EvaluatedArgs::Ieee754Bits2 {
                    left: ReadyIeee754Arg::Undemanded,
                    right: ReadyIeee754Arg::Value(Some(0)),
                },
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
                assert_eq!(worker.kernel_invocations(), 0);
            }
            for (right, expected) in [(5, Some(2)), (0, None), (5, Some(2))] {
                match worker
                    .eval_args(args(operation, Some(17), Some(right)))
                    .unwrap()
                {
                    ComputedValue::Int(value) => {
                        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
                        assert_eq!(value.into_option(), expected);
                    }
                    ComputedValue::Int128(value) => {
                        assert_eq!(value.metadata(), ComputedInt128Metadata::OwnInt128);
                        assert_eq!(value.into_option(), expected.map(i128::from));
                    }
                    ComputedValue::Ieee754Bits(value) => {
                        assert_eq!(
                            value.metadata(),
                            ComputedIeee754BitsMetadata::OwnIeee754Bits
                        );
                        assert_eq!(
                            value.into_option(),
                            expected.map(|value| (value as f64).to_bits())
                        );
                    }
                    ComputedValue::Decimal(value) => {
                        assert_eq!(value.metadata(), ComputedDecimalMetadata::OwnDecimal);
                        assert_eq!(value.into_option(), expected.map(Decimal::from));
                    }
                    _ => panic!("MOD changed its exact result carrier"),
                }
                assert!(worker.is_healthy());
                assert_eq!(worker.retained_storage().unwrap(), storage);
            }
            assert_eq!(worker.kernel_invocations(), 3);
            let mut zero = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits {
                    max_steps: 0,
                    ..ExecutionLimits::default()
                },
                usize::MAX,
            )
            .unwrap();
            let failure = zero
                .eval_args_reported(args(operation, Some(17), Some(5)))
                .unwrap_err();
            assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
            assert_eq!(failure.sql_failure(), None);
            assert!(failure.native_binary_arithmetic_error().is_none());
            assert!(failure.legacy_binary_arithmetic_error().is_none());
            assert_eq!(zero.kernel_invocations(), 0);
            assert!(zero.is_healthy());
        }
        let real = |left: f64, right: f64| EvaluatedArgs::Ieee754Bits2 {
            left: ReadyIeee754Arg::Value(Some(left.to_bits())),
            right: ReadyIeee754Arg::Value(Some(right.to_bits())),
        };
        let mut native = prepare(EvaluatedBytesOp::ModRealNative);
        let mut legacy = prepare(EvaluatedBytesOp::ModRealLegacy);
        for nonfinite in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut failure = native.eval_args_reported(real(nonfinite, 2.0)).unwrap_err();
            assert_eq!(failure.operation(), Some(EvaluatedBytesOp::ModRealNative));
            assert_eq!(
                failure.sql_failure(),
                Some(EvaluatedSqlFailureKind::BinaryArithmeticNative)
            );
            assert_eq!(
                failure.native_binary_arithmetic_error(),
                Some(&NativeBinaryArithmeticError {
                    operation: BinaryArithmeticOperation::Modulo,
                    kind: BinaryArithmeticErrorKind::FloatOverflow,
                })
            );
            assert!(failure.legacy_binary_arithmetic_error().is_none());
            for wrong in [
                EvaluatedBytesOp::AddRealNative,
                EvaluatedBytesOp::ModRealLegacy,
                EvaluatedBytesOp::ModIntSsNative,
                EvaluatedBytesOp::ModDecimalNative,
            ] {
                failure.operation = Some(wrong);
                assert!(failure.native_binary_arithmetic_error().is_none());
            }
            let ComputedValue::Ieee754Bits(value) = legacy.eval_args(real(nonfinite, 2.0)).unwrap()
            else {
                panic!("legacy MOD must retain IEEE NaN");
            };
            assert!(f64::from_bits(value.into_option().unwrap()).is_nan());
            for worker in [&mut native, &mut legacy] {
                let ComputedValue::Ieee754Bits(value) =
                    worker.eval_args(real(nonfinite, -0.0)).unwrap()
                else {
                    panic!("zero divisor must own absent IEEE bits");
                };
                assert_eq!(value.into_option(), None);
                assert!(worker.is_healthy());
            }
        }
        for (left, right, expected) in [(2.0, f64::INFINITY, 2.0_f64), (-0.0, 2.0, -0.0)] {
            for worker in [&mut native, &mut legacy] {
                let ComputedValue::Ieee754Bits(value) =
                    worker.eval_args(real(left, right)).unwrap()
                else {
                    panic!("MOD must own IEEE bits");
                };
                assert_eq!(value.into_option(), Some(expected.to_bits()));
            }
        }
        let mut decimal = prepare(EvaluatedBytesOp::ModDecimalNative);
        let (mut ready, arity, _) = args(EvaluatedBytesOp::ModDecimalNative, Some(17), Some(5))
            .into_values(4096)
            .unwrap();
        ready[2] = ScalarValue::Int(Some(0));
        let mut sql_failure = None;
        assert!(matches!(
            decimal.eval_ready(ready, arity, &mut sql_failure),
            Err(LocalError::Evaluation(_))
        ));
        assert_eq!(
            sql_failure, None,
            "Decimal resource errors are not SQL overflow"
        );
        assert_eq!(decimal.kernel_invocations(), 1);
        assert!(
            decimal
                .eval_args(args(EvaluatedBytesOp::ModDecimalNative, Some(17), Some(5)))
                .is_ok()
        );
        assert!(decimal.is_healthy());
        let wide = || Decimal::try_from_native_digits(false, &[b'9'; 90], 0, 0, 4096).unwrap();
        let (left, right) = (wide(), wide());
        let live = left.spill_capacity_bytes() + right.spill_capacity_bytes();
        let (ready, arity, _) = EvaluatedArgs::Decimal2 {
            left: Some(left),
            right: Some(right),
        }
        .into_values(live + 64)
        .unwrap();
        assert_eq!(arity, 3);
        assert_eq!(ready[2], ScalarValue::Int(Some(64)));
        for (operation, input) in [
            (
                EvaluatedBytesOp::BinaryArithmeticNullNative,
                EvaluatedArgs::NullWitness(None),
            ),
            (
                EvaluatedBytesOp::BinaryArithmeticMissingLegacy,
                EvaluatedArgs::NoArgs,
            ),
        ] {
            let ComputedValue::Int(value) = prepare(operation).eval_args(input).unwrap() else {
                panic!("existing NULL/missing witnesses retain Int carrier");
            };
            assert_eq!(value.into_option(), None);
        }
        // Adding value-only MOD does not narrow old nullable arithmetic recipes.
        for (operation, input) in [
            (
                EvaluatedBytesOp::AddIntSsNative,
                EvaluatedArgs::Int2(None, Some(1)),
            ),
            (
                EvaluatedBytesOp::AddInt128SignedLegacy,
                EvaluatedArgs::Int1282(None, Some(1)),
            ),
            (
                EvaluatedBytesOp::AddRealNative,
                EvaluatedArgs::Ieee754Bits2 {
                    left: ReadyIeee754Arg::Value(None),
                    right: ReadyIeee754Arg::Value(Some(0)),
                },
            ),
            (
                EvaluatedBytesOp::AddDecimalNative,
                EvaluatedArgs::Decimal2 {
                    left: None,
                    right: Some(Decimal::from(1i64)),
                },
            ),
        ] {
            assert!(prepare(operation).eval_args(input).is_ok());
        }
    }

    #[test]
    fn binary_arithmetic_dispatch_typed_failures_int128_and_budget() {
        let prepare = |operation| {
            prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap()
        };
        let real = |left: f64, right: f64| EvaluatedArgs::Ieee754Bits2 {
            left: ReadyIeee754Arg::Value(Some(left.to_bits())),
            right: ReadyIeee754Arg::Value(Some(right.to_bits())),
        };
        let mut add = prepare(EvaluatedBytesOp::AddIntSsNative);
        let storage = add.retained_storage().unwrap();
        let mut failure = add
            .eval_args_reported(EvaluatedArgs::Int2(Some(i64::MAX), Some(1)))
            .unwrap_err();
        assert_eq!(
            failure.sql_failure(),
            Some(EvaluatedSqlFailureKind::BinaryArithmeticNative)
        );
        assert_eq!(
            failure.native_binary_arithmetic_error(),
            Some(&NativeBinaryArithmeticError {
                operation: BinaryArithmeticOperation::Add,
                kind: BinaryArithmeticErrorKind::IntOverflow
            })
        );
        for wrong in [
            EvaluatedBytesOp::SubIntSsNative,
            EvaluatedBytesOp::AddRealNative,
            EvaluatedBytesOp::AddInt128SignedLegacy,
            EvaluatedBytesOp::AddDecimalFastNative,
        ] {
            failure.operation = Some(wrong);
            assert!(failure.native_binary_arithmetic_error().is_none());
        }
        assert_eq!(add.kernel_invocations(), 1);
        assert!(add.is_healthy());
        assert_eq!(add.retained_storage().unwrap(), storage);
        let ComputedValue::Int(value) = add
            .eval_args(EvaluatedArgs::Int2(Some(2), Some(3)))
            .unwrap()
        else {
            panic!("native integer must own Int");
        };
        assert_eq!(value.into_option(), Some(5));
        assert_eq!(add.kernel_invocations(), 2);
        let failure = prepare(EvaluatedBytesOp::MulRealNative)
            .eval_args_reported(real(f64::MAX, 2.0))
            .unwrap_err();
        assert_eq!(
            failure.native_binary_arithmetic_error(),
            Some(&NativeBinaryArithmeticError {
                operation: BinaryArithmeticOperation::Multiply,
                kind: BinaryArithmeticErrorKind::FloatOverflow
            }),
            "actual failure: {failure:?}"
        );
        let full = || Decimal::try_from_native_digits(false, &[b'9'; 81], 0, 0, 4096).unwrap();
        let failure = prepare(EvaluatedBytesOp::AddDecimalNative)
            .eval_args_reported(EvaluatedArgs::Decimal2 {
                left: Some(full()),
                right: Some(Decimal::from(1i64)),
            })
            .unwrap_err();
        assert_eq!(
            failure.native_binary_arithmetic_error(),
            Some(&NativeBinaryArithmeticError {
                operation: BinaryArithmeticOperation::Add,
                kind: BinaryArithmeticErrorKind::DecimalOverflow
            })
        );
        assert!(matches!(
            prepare(EvaluatedBytesOp::AddDecimalLegacy)
                .eval_args(EvaluatedArgs::Decimal2 {
                    left: Some(full()),
                    right: Some(Decimal::from(1i64))
                })
                .unwrap(),
            ComputedValue::Decimal(_)
        ));
        let ComputedValue::Ieee754Bits(value) = prepare(EvaluatedBytesOp::AddRealLegacy)
            .eval_args(real(f64::INFINITY, 1.0))
            .unwrap()
        else {
            panic!("legacy real must preserve IEEE bits");
        };
        assert_eq!(value.into_option(), Some(f64::INFINITY.to_bits()));

        let mut legacy = prepare(EvaluatedBytesOp::AddInt128SignedLegacy);
        assert!(matches!(
            legacy.eval_args(EvaluatedArgs::Bytes2(Some(vec![0; 16]), Some(vec![0; 16]))),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(legacy.kernel_invocations(), 0);
        let ComputedValue::Int128(value) = legacy
            .eval_args(EvaluatedArgs::Int1282(
                Some(1i128 << 100),
                Some(-(1i128 << 100) + 7),
            ))
            .unwrap()
        else {
            panic!("legacy arithmetic must not narrow input to i64");
        };
        assert_eq!(value.metadata(), ComputedInt128Metadata::OwnInt128);
        assert_eq!(value.into_option(), Some(7));
        assert_eq!(legacy.kernel_invocations(), 1);
        let mut failure = prepare(EvaluatedBytesOp::MulInt128UnsignedLegacy)
            .eval_args_reported(EvaluatedArgs::Int1282(Some(i128::from(u64::MAX)), Some(2)))
            .unwrap_err();
        assert_eq!(
            failure.sql_failure(),
            Some(EvaluatedSqlFailureKind::BinaryArithmeticLegacy)
        );
        assert_eq!(
            failure.legacy_binary_arithmetic_error(),
            Some(&LegacyBinaryArithmeticError {
                operation: BinaryArithmeticOperation::Multiply,
                unsigned: true
            })
        );
        failure.operation = Some(EvaluatedBytesOp::MulInt128SignedLegacy);
        assert!(failure.legacy_binary_arithmetic_error().is_none());
        failure.operation = Some(EvaluatedBytesOp::AddInt128UnsignedLegacy);
        assert!(failure.legacy_binary_arithmetic_error().is_none());
        assert!(failure.native_binary_arithmetic_error().is_none());

        let wide = || Decimal::try_from_native_digits(false, &[b'9'; 90], 0, 0, 4096).unwrap();
        let (left, right) = (wide(), wide());
        let live = left
            .spill_capacity_bytes()
            .checked_add(right.spill_capacity_bytes())
            .unwrap();
        assert!(left.spill_capacity_bytes() > 0 && right.spill_capacity_bytes() > 0);
        let (ready, arity, binding) = EvaluatedArgs::Decimal2 {
            left: Some(left),
            right: Some(right),
        }
        .into_values(live + 64)
        .unwrap();
        assert_eq!(arity, 3);
        assert!(binding.is_none());
        assert_eq!(ready[2], ScalarValue::Int(Some(64)));
        drop(ready);
        let (left, right) = (wide(), wide());
        let live = left
            .spill_capacity_bytes()
            .checked_add(right.spill_capacity_bytes())
            .unwrap();
        assert!(matches!(
            EvaluatedArgs::Decimal2 {
                left: Some(left),
                right: Some(right)
            }
            .into_values(live - 1),
            Err(LocalError::ResourceLimit(_))
        ));
        for (operation, args) in [
            (
                EvaluatedBytesOp::BinaryArithmeticNullNative,
                EvaluatedArgs::NullWitness(None),
            ),
            (
                EvaluatedBytesOp::BinaryArithmeticMissingLegacy,
                EvaluatedArgs::NoArgs,
            ),
        ] {
            let mut worker = prepare(operation);
            let ComputedValue::Int(value) = worker.eval_args(args).unwrap() else {
                panic!("NULL and missing keep the Int result domain");
            };
            assert_eq!(value.into_option(), None);
            assert_eq!(worker.kernel_invocations(), 1);
        }
        let mut zero = prepare_evaluated_bytes(
            EvaluatedBytesOp::AddIntSsNative,
            LocalCompileContext::default(),
            ExecutionLimits {
                max_steps: 0,
                ..ExecutionLimits::default()
            },
            usize::MAX,
        )
        .unwrap();
        let failure = zero
            .eval_args_reported(EvaluatedArgs::Int2(Some(i64::MAX), Some(1)))
            .unwrap_err();
        assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
        assert_eq!(failure.sql_failure(), None);
        assert!(failure.native_binary_arithmetic_error().is_none());
        assert!(failure.legacy_binary_arithmetic_error().is_none());
        assert_eq!(zero.kernel_invocations(), 0);
        assert!(zero.is_healthy());
    }

    #[test]
    fn binary_arithmetic_dispatch_fast_outcomes_and_vector_results() {
        use crate::{NativeDecimalFastOutcome, NativeDecimalFastValue};
        let prepare = |operation| {
            prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap()
        };
        let decimal = |coefficient, storage_scale, scale| {
            Decimal::try_from_native_fast(
                NativeDecimalFastValue {
                    coefficient,
                    storage_scale,
                    scale,
                },
                4096,
            )
            .unwrap()
        };
        // Hand-derived coefficient/scale results; the ordinary result carrier is
        // never substituted for Unsupported or for a genuine SQL NULL.
        for (operation, coefficient, storage_scale, scale) in [
            (EvaluatedBytesOp::AddDecimalFastNative, 46, 1, 1),
            (EvaluatedBytesOp::SubDecimalFastNative, -22, 1, 1),
            (EvaluatedBytesOp::MulDecimalFastNative, 408, 2, 2),
        ] {
            let mut worker = prepare(operation);
            let storage = worker.retained_storage().unwrap();
            let ComputedValue::DecimalFast(value) = worker
                .eval_args(EvaluatedArgs::Decimal2 {
                    left: Some(decimal(12, 1, 1)),
                    right: Some(decimal(34, 1, 1)),
                })
                .unwrap()
            else {
                panic!("fast result must be a distinct computed outcome");
            };
            assert_eq!(
                value.metadata(),
                ComputedDecimalFastMetadata::OwnDecimalFast
            );
            let expected = NativeDecimalFastOutcome::Value(Some(NativeDecimalFastValue {
                coefficient,
                storage_scale,
                scale,
            }));
            assert_eq!(value.outcome(), &expected);
            assert_eq!(value.into_outcome(), expected);
            assert_eq!(worker.kernel_invocations(), 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let mut fast = prepare(EvaluatedBytesOp::SubDecimalFastNative);
        let ComputedValue::DecimalFast(value) = fast
            .eval_args(EvaluatedArgs::Decimal2 {
                left: Some(decimal(i128::MIN, 0, 0)),
                right: Some(decimal(i128::MIN, 0, 0)),
            })
            .unwrap()
        else {
            panic!("fast unsupported lost its result domain");
        };
        assert_eq!(value.into_outcome(), NativeDecimalFastOutcome::Unsupported);
        assert_eq!(fast.kernel_invocations(), 1);
        // The fallback is another genuine ordinary TiKV worker, not a native
        // host subtraction or an invented zero replacing the fast outcome.
        let mut ordinary = prepare(EvaluatedBytesOp::SubDecimalNative);
        let ComputedValue::Decimal(value) = ordinary
            .eval_args(EvaluatedArgs::Decimal2 {
                left: Some(decimal(i128::MIN, 0, 0)),
                right: Some(decimal(i128::MIN, 0, 0)),
            })
            .unwrap()
        else {
            panic!("ordinary fallback must own Decimal");
        };
        assert_eq!(value.value().unwrap().to_string(), "0");
        assert_eq!(value.checked_i64_view(), None);
        assert_eq!(ordinary.kernel_invocations(), 1);
        let ComputedValue::DecimalFast(value) = fast
            .eval_args(EvaluatedArgs::Decimal2 {
                left: None,
                right: Some(decimal(1, 0, 0)),
            })
            .unwrap()
        else {
            panic!("SQL NULL lost its fast domain");
        };
        assert_eq!(value.into_outcome(), NativeDecimalFastOutcome::Value(None));
        assert_eq!(fast.kernel_invocations(), 2);
        assert!(fast.is_healthy());

        for (operation, expected) in [
            (EvaluatedBytesOp::AddVectorNative, [4.0f32, 6.0]),
            (EvaluatedBytesOp::SubVectorNative, [-2.0f32, -2.0]),
            (EvaluatedBytesOp::MulVectorNative, [3.0f32, 8.0]),
        ] {
            let mut worker = prepare(operation);
            let storage = worker.retained_storage().unwrap();
            let ComputedValue::NativeVector(value) = worker
                .eval_args(EvaluatedArgs::NativeVector2(
                    Some(NativeVectorFloat32::must_create(vec![1.0, 2.0])),
                    Some(NativeVectorFloat32::must_create(vec![3.0, 4.0])),
                ))
                .unwrap()
            else {
                panic!("vector arithmetic must use the actual LE output bridge");
            };
            assert_eq!(
                value.metadata(),
                ComputedNativeVectorMetadata::OwnNativeVector
            );
            assert_eq!(value.value().unwrap().elements(), &expected);
            assert_eq!(worker.kernel_invocations(), 1);
            let failure = worker
                .eval_args_reported(EvaluatedArgs::NativeVector2(
                    Some(NativeVectorFloat32::must_create(vec![1.0])),
                    Some(NativeVectorFloat32::must_create(vec![1.0, 2.0])),
                ))
                .unwrap_err();
            assert_eq!(
                failure.sql_failure(),
                Some(EvaluatedSqlFailureKind::VectorNative)
            );
            assert!(failure.native_vector_error().is_some());
            assert!(failure.native_binary_arithmetic_error().is_none());
            assert_eq!(worker.kernel_invocations(), 2);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }

    #[test]
    fn unary_dispatch_dynamic_constant_boundaries_and_scope_receipts() {
        let prepare = |operation| {
            prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap()
        };
        for (operation, bad, unsigned, good, expected) in [
            (
                EvaluatedBytesOp::UnaryMinusIntNative,
                i64::MIN,
                false,
                -7,
                7,
            ),
            (
                EvaluatedBytesOp::UnaryMinusUIntNative,
                -1,
                true,
                i64::MIN,
                i64::MIN,
            ),
        ] {
            let mut worker = prepare(operation);
            let storage = worker.retained_storage().unwrap();
            let mut failure = worker
                .eval_args_reported(EvaluatedArgs::Int(Some(bad)))
                .unwrap_err();
            assert_eq!(
                failure.sql_failure(),
                Some(EvaluatedSqlFailureKind::UnaryMinusNative)
            );
            let cause = failure.native_unary_minus_error().unwrap();
            assert_eq!((cause.bits, cause.unsigned), (bad as u64, unsigned));
            assert_eq!(worker.kernel_invocations(), 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            failure.operation = Some(if unsigned {
                EvaluatedBytesOp::UnaryMinusIntNative
            } else {
                EvaluatedBytesOp::UnaryMinusUIntNative
            });
            assert!(failure.native_unary_minus_error().is_none());
            failure.operation = Some(EvaluatedBytesOp::UnaryMinusIntConstantNative);
            assert!(failure.native_unary_minus_error().is_none());
            let ComputedValue::Int(value) =
                worker.eval_args(EvaluatedArgs::Int(Some(good))).unwrap()
            else {
                panic!("dynamic negation must own signed Int");
            };
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), Some(expected));
            assert_eq!(worker.kernel_invocations(), 2);
        }
        // Hand-derived signed/unsigned boundaries: both constant kernels always
        // return an actual Decimal; only its exact checked view permits packing Int.
        for (operation, raw, text, checked) in [
            (
                EvaluatedBytesOp::UnaryMinusIntConstantNative,
                i64::MIN,
                "9223372036854775808",
                None,
            ),
            (
                EvaluatedBytesOp::UnaryMinusIntConstantNative,
                7,
                "-7",
                Some(-7),
            ),
            (
                EvaluatedBytesOp::UnaryMinusUIntConstantNative,
                i64::MIN,
                "-9223372036854775808",
                Some(i64::MIN),
            ),
            (
                EvaluatedBytesOp::UnaryMinusUIntConstantNative,
                -1,
                "-18446744073709551615",
                None,
            ),
        ] {
            let mut worker = prepare(operation);
            let ComputedValue::Decimal(value) =
                worker.eval_args(EvaluatedArgs::Int(Some(raw))).unwrap()
            else {
                panic!("constant negation must own real Decimal");
            };
            assert_eq!(value.metadata(), ComputedDecimalMetadata::OwnDecimal);
            assert_eq!(value.value().unwrap().to_string(), text);
            assert_eq!(value.checked_i64_view(), checked);
            assert_eq!(worker.kernel_invocations(), 1);
            assert!(worker.is_healthy());
        }
        for (operation, text) in [
            (EvaluatedBytesOp::UnaryPlusDecimalNative, "7"),
            (EvaluatedBytesOp::UnaryMinusDecimalNative, "-7"),
        ] {
            let ComputedValue::Decimal(value) = prepare(operation)
                .eval_args(EvaluatedArgs::Decimal(Some(Decimal::from(7i64))))
                .unwrap()
            else {
                panic!("Decimal unary must use the existing budgeted carrier");
            };
            assert_eq!(value.value().unwrap().to_string(), text);
            assert_eq!(value.checked_i64_view(), None);
        }
        let mut bits = prepare(EvaluatedBytesOp::UnaryPlusBitsNative);
        assert!(matches!(
            bits.eval_args(EvaluatedArgs::Bytes(Some(vec![0; 8]))),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(bits.kernel_invocations(), 0);
        let mut null = prepare(EvaluatedBytesOp::UnaryNullNative);
        assert!(matches!(
            null.eval_args(EvaluatedArgs::NullWitness(Some(0))),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(null.kernel_invocations(), 0);
        let ComputedValue::Int(value) = null.eval_args(EvaluatedArgs::NullWitness(None)).unwrap()
        else {
            panic!("unary NULL must be genuine");
        };
        assert_eq!(value.into_option(), None);
        assert_eq!(null.kernel_invocations(), 1);
        let mut zero = prepare_evaluated_bytes(
            EvaluatedBytesOp::UnaryMinusIntNative,
            LocalCompileContext::default(),
            ExecutionLimits {
                max_steps: 0,
                ..ExecutionLimits::default()
            },
            usize::MAX,
        )
        .unwrap();
        let failure = zero
            .eval_args_reported(EvaluatedArgs::Int(Some(i64::MIN)))
            .unwrap_err();
        assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
        assert_eq!(failure.sql_failure(), None);
        assert!(failure.native_unary_minus_error().is_none());
        assert_eq!(zero.kernel_invocations(), 0);
        assert!(zero.is_healthy());
    }

    #[test]
    fn like_dispatch_cache_identity_roles_null_and_missing() {
        use crate::{NativeCompiledIlikePattern, NativeCompiledLikePattern, NativeContextCache};
        let prepare = |operation| {
            prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap()
        };
        let args = |invocation, text: &[u8], pattern: &[u8]| EvaluatedArgs::Like {
            invocation,
            text: Some(text.to_vec()),
            pattern: Some(pattern.to_vec()),
            escape: Some(92),
        };
        let assert_int = |value, expected| {
            let ComputedValue::Int(value) = value else {
                panic!("LIKE must own signed Int")
            };
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.value(), expected);
        };
        let cache = NativeContextCache::<NativeCompiledLikePattern>::new();
        let invocation = NativeLikeInvocation::like(NativeCollation::Utf8Mb4Bin, Some((&cache, 1)));
        let mut like = prepare(EvaluatedBytesOp::LikeNative);
        let storage = like.retained_storage().unwrap();
        assert!(like.program.host_catalog.is_none());
        assert!(cache.get_cache(1).is_none());
        // Preflight refuses kind, physical-role, NULL and unnormalized escapes
        // without dispatch, cache lookup/compile, or poisoning a healthy worker.
        for bad in [
            args(NativeLikeInvocation::legacy(false), b"abc", b"a%"),
            EvaluatedArgs::BytesBytesInt(Some(b"abc".to_vec()), Some(b"a%".to_vec()), Some(92)),
            EvaluatedArgs::Like {
                invocation: invocation.clone(),
                text: None,
                pattern: Some(b"a%".to_vec()),
                escape: Some(92),
            },
            EvaluatedArgs::Like {
                invocation: invocation.clone(),
                text: Some(b"abc".to_vec()),
                pattern: None,
                escape: Some(92),
            },
            EvaluatedArgs::Like {
                invocation: invocation.clone(),
                text: Some(b"abc".to_vec()),
                pattern: Some(b"a%".to_vec()),
                escape: None,
            },
            EvaluatedArgs::Like {
                invocation: invocation.clone(),
                text: Some(b"abc".to_vec()),
                pattern: Some(b"a%".to_vec()),
                escape: Some(-1),
            },
            EvaluatedArgs::Like {
                invocation: invocation.clone(),
                text: Some(b"abc".to_vec()),
                pattern: Some(b"a%".to_vec()),
                escape: Some(256),
            },
        ] {
            let failure = like.eval_args_reported(bad).unwrap_err();
            assert!(matches!(failure.error(), LocalError::InvalidBatch(_)));
            assert_eq!(failure.sql_failure(), None);
            assert_eq!(like.kernel_invocations(), 0);
            assert!(like.is_healthy());
            assert!(cache.get_cache(1).is_none());
        }
        assert_int(
            like.eval_args(args(invocation.clone(), b"abc", b"a%"))
                .unwrap(),
            Some(1),
        );
        let first = cache.get_cache(1).unwrap();
        assert_int(
            like.eval_args(args(invocation.clone(), b"abc", b"z%"))
                .unwrap(),
            Some(1),
        );
        assert!(Arc::ptr_eq(&first, &cache.get_cache(1).unwrap()));
        let cloned_owner = cache.clone();
        assert!(cloned_owner.get_cache(1).is_none());
        assert_int(
            like.eval_args(args(
                NativeLikeInvocation::like(NativeCollation::Utf8Mb4Bin, Some((&cloned_owner, 1))),
                b"abc",
                b"z%",
            ))
            .unwrap(),
            Some(0),
        );
        assert_int(
            like.eval_args(args(
                NativeLikeInvocation::like(NativeCollation::Utf8Mb4Bin, Some((&cache, 2))),
                b"abc",
                b"z%",
            ))
            .unwrap(),
            Some(0),
        );
        assert!(cache.get_cache(1).is_none());
        assert!(first.is_match(b"abc"));
        assert_int(
            like.eval_args(args(
                NativeLikeInvocation::like(NativeCollation::Utf8Mb4Bin, None),
                b"abc",
                b"a%",
            ))
            .unwrap(),
            Some(1),
        );
        assert_int(
            like.eval_args(args(
                NativeLikeInvocation::like(NativeCollation::Utf8Mb4Bin, None),
                b"abc",
                b"z%",
            ))
            .unwrap(),
            Some(0),
        );
        assert_eq!(like.kernel_invocations(), 6);
        assert!(like.like_metadata().unwrap().is_unbound());
        assert_eq!(like.retained_storage().unwrap(), storage);
        let ilike_cache = NativeContextCache::<NativeCompiledIlikePattern>::new();
        let ilike_invocation =
            NativeLikeInvocation::ilike(NativeCollation::Utf8Mb4Bin, Some((&ilike_cache, 3)));
        let mut ilike = prepare(EvaluatedBytesOp::IlikeNative);
        assert_int(
            ilike
                .eval_args(args(ilike_invocation.clone(), b"ABC", b"a%"))
                .unwrap(),
            Some(1),
        );
        let folded = ilike_cache.get_cache(3).unwrap();
        assert_int(
            ilike
                .eval_args(args(ilike_invocation, b"ABC", b"z%"))
                .unwrap(),
            Some(1),
        );
        assert!(Arc::ptr_eq(&folded, &ilike_cache.get_cache(3).unwrap()));
        assert!(ilike.like_metadata().unwrap().is_unbound());
        let mut legacy = prepare(EvaluatedBytesOp::LikeLegacyNative);
        assert_int(
            legacy
                .eval_args(args(NativeLikeInvocation::legacy(true), b"ABC", b"a%"))
                .unwrap(),
            Some(1),
        );
        assert_int(
            legacy
                .eval_args(args(NativeLikeInvocation::legacy(false), b"ABC", b"a%"))
                .unwrap(),
            Some(0),
        );
        assert!(legacy.like_metadata().unwrap().is_unbound());
        let mut null = prepare(EvaluatedBytesOp::LikeNullIntNative);
        assert!(null.eval_args(EvaluatedArgs::NullWitness(Some(0))).is_err());
        assert!(null.eval_args(EvaluatedArgs::Int(None)).is_err());
        assert_eq!(null.kernel_invocations(), 0);
        assert_int(
            null.eval_args(EvaluatedArgs::NullWitness(None)).unwrap(),
            None,
        );
        assert_eq!(null.kernel_invocations(), 1);
        assert!(null.like_metadata().is_err());
        assert!(null.is_healthy());
        let mut missing = prepare(EvaluatedBytesOp::LikeMissingLegacyNative);
        assert!(missing.eval_args(EvaluatedArgs::NullWitness(None)).is_err());
        assert_int(missing.eval_args(EvaluatedArgs::NoArgs).unwrap(), None);
        assert_eq!(missing.kernel_invocations(), 1);
        assert!(missing.like_metadata().is_err());
        assert!(missing.is_healthy());
    }

    #[test]
    fn like_dispatch_budget_binding_cleanup_and_unwind() {
        use crate::{NativeCompiledLikePattern, NativeContextCache};
        let cache = NativeContextCache::<NativeCompiledLikePattern>::new();
        let invocation = NativeLikeInvocation::like(NativeCollation::Utf8Mb4Bin, Some((&cache, 1)));
        let args = || EvaluatedArgs::Like {
            invocation: invocation.clone(),
            text: Some(b"abc".to_vec()),
            pattern: Some(b"a%".to_vec()),
            escape: Some(92),
        };
        let prepare = |limits| {
            prepare_evaluated_bytes(
                EvaluatedBytesOp::LikeNative,
                LocalCompileContext::default(),
                limits,
                usize::MAX,
            )
            .unwrap()
        };
        for limits in [
            ExecutionLimits {
                max_steps: 0,
                ..ExecutionLimits::default()
            },
            ExecutionLimits {
                max_retained_bytes: 0,
                ..ExecutionLimits::default()
            },
        ] {
            let mut zero = prepare(limits);
            let storage = zero.retained_storage().unwrap();
            let failure = zero.eval_args_reported(args()).unwrap_err();
            assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
            assert_eq!(failure.sql_failure(), None);
            assert_eq!(zero.kernel_invocations(), 0);
            assert!(zero.like_metadata().unwrap().is_unbound());
            assert!(zero.is_healthy());
            assert_eq!(zero.retained_storage().unwrap(), storage);
            assert!(cache.get_cache(1).is_none());
        }
        let mut capacity_limited = prepare(ExecutionLimits {
            max_retained_bytes: 4096,
            ..ExecutionLimits::default()
        });
        let empty_owner = Vec::with_capacity(4097);
        let failure = capacity_limited
            .eval_args_reported(EvaluatedArgs::Like {
                invocation: invocation.clone(),
                text: Some(empty_owner),
                pattern: Some(Vec::new()),
                escape: Some(0),
            })
            .unwrap_err();
        assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
        assert_eq!(capacity_limited.kernel_invocations(), 0);
        assert!(capacity_limited.is_healthy());
        assert!(cache.get_cache(1).is_none());
        let mut unlimited_empty = prepare(ExecutionLimits {
            max_retained_bytes: usize::MAX,
            ..ExecutionLimits::default()
        });
        assert!(
            unlimited_empty
                .eval_args(EvaluatedArgs::Like {
                    invocation: NativeLikeInvocation::like(NativeCollation::Binary, None),
                    text: Some(Vec::new()),
                    pattern: Some(Vec::new()),
                    escape: Some(0),
                })
                .is_ok()
        );
        assert!(unlimited_empty.like_metadata().unwrap().is_unbound());
        let payload = NativeLikeCallMetadata::new(NativeLikeKind::Like);
        assert!(payload.invocation().is_err());
        assert!(payload.record_known_cache_bytes(Some(0)).is_err());
        assert!(
            payload
                .bind(NativeLikeInvocation::legacy(false), 1)
                .is_err()
        );
        payload.bind(invocation.clone(), 1).unwrap();
        assert!(payload.bind(invocation.clone(), 1).is_err());
        payload.record_known_cache_bytes(Some(1)).unwrap();
        assert!(matches!(
            payload.record_known_cache_bytes(Some(1)),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(payload.known_cache_bytes.get(), Some(2));
        payload.unbind().unwrap();
        payload.bind(invocation.clone(), usize::MAX).unwrap();
        payload.record_known_cache_bytes(Some(usize::MAX)).unwrap();
        assert!(payload.record_known_cache_bytes(Some(1)).is_err());
        assert_eq!(payload.known_cache_bytes.get(), None);
        payload.unbind().unwrap();
        payload.bind(invocation.clone(), 1).unwrap();
        assert!(payload.record_known_cache_bytes(None).is_err());
        assert_eq!(payload.known_cache_bytes.get(), None);
        payload.unbind().unwrap();
        assert!(payload.is_unbound());
        let mut worker = prepare(ExecutionLimits::default());
        let storage = worker.retained_storage().unwrap();
        let known_owner = evaluated_ascii_owned_heap_bytes(
            worker.program.expression.capacity(),
            worker.program.schema.capacity(),
            worker
                .program
                .expression
                .retained_metadata_heap_bytes()
                .unwrap(),
            worker.ctx.warnings.warnings.capacity(),
        )
        .unwrap();
        assert_eq!(
            storage.total_bytes(),
            mem::size_of::<EvaluatedBytesWorker>()
                + known_owner
                + mem::size_of::<NativeLikeCallMetadata>()
        );
        // Force refusal at the real generated wrapper's cache observation.
        // This must be infrastructure, never a host fallback or SQL receipt.
        let (ready, arity, regexp) = args().into_values(0).unwrap();
        assert!(regexp.is_none());
        assert_eq!(arity, 3);
        worker.begin_invocation().unwrap();
        let mut sql_failure = None;
        let result = {
            let guard = LikeBindingGuard {
                worker: &mut worker,
            };
            guard
                .worker
                .like_metadata()
                .unwrap()
                .bind(invocation.clone(), 0)
                .unwrap();
            guard.worker.eval_ready(ready, arity, &mut sql_failure)
        };
        assert!(matches!(
            worker.finish_invocation(result),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(sql_failure, None);
        assert_eq!(worker.kernel_invocations(), 1);
        assert!(cache.get_cache(1).is_some());
        assert!(worker.like_metadata().unwrap().is_unbound());
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        assert!(worker.eval_args(args()).is_ok());
        assert_eq!(worker.kernel_invocations(), 2);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            worker.begin_invocation().unwrap();
            let guard = LikeBindingGuard {
                worker: &mut worker,
            };
            guard
                .worker
                .like_metadata()
                .unwrap()
                .bind(invocation.clone(), 100)
                .unwrap();
            guard
                .worker
                .like_metadata()
                .unwrap()
                .record_known_cache_bytes(Some(1))
                .unwrap();
            panic!("LIKE binding unwind probe");
        }));
        assert!(panic.is_err());
        assert!(worker.like_metadata().unwrap().is_unbound());
        assert!(!worker.is_healthy());
        assert!(worker.eval_args(args()).is_err());
        assert_eq!(worker.kernel_invocations(), 2);
        assert!(cache.get_cache(1).is_some());
    }

    #[test]
    fn regexp_dispatch_cache_identity_owner_reuse_and_typed_results() {
        use crate::{
            NativeCachedRegexp, NativeContextCache, NativeRegexpCompileError, NativeReplacementPart,
        };
        let prepare = |operation| {
            prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap()
        };
        let patterns = NativeContextCache::<NativeCachedRegexp>::default();
        let parts = NativeContextCache::<Vec<NativeReplacementPart>>::default();
        let other_patterns = NativeContextCache::<NativeCachedRegexp>::default();
        let other_parts = NativeContextCache::<Vec<NativeReplacementPart>>::default();
        let same = NativeRegexpInvocation::new(&patterns, &parts, 11, true, true);
        let next = NativeRegexpInvocation::new(&patterns, &parts, 12, true, true);
        let other = NativeRegexpInvocation::new(&other_patterns, &other_parts, 12, true, true);
        assert!(patterns.get_cache(11).is_none());
        let replacement =
            |invocation, pattern: &[u8], replacement: &[u8]| EvaluatedArgs::RegexpReplace {
                invocation,
                text: b"abc".to_vec(),
                pattern: pattern.to_vec(),
                replacement: replacement.to_vec(),
                pos: 1,
                occurrence: 0,
                match_type: Vec::new(),
            };
        let mut worker = prepare(EvaluatedBytesOp::RegexpReplaceNative);
        let storage = worker.retained_storage().unwrap();
        let mut first_pattern = None;
        let mut first_parts = None;
        // First three rows are the original native cache-lifecycle literals;
        // fourth probes a different expression owner reusing this same worker.
        for (index, (invocation, pattern, replace, expected)) in [
            (
                same.clone(),
                b"a".as_slice(),
                b"X".as_slice(),
                b"Xbc".as_slice(),
            ),
            (
                same.clone(),
                b"c".as_slice(),
                b"Y".as_slice(),
                b"Xbc".as_slice(),
            ),
            (next, b"c".as_slice(), b"Y".as_slice(), b"abY".as_slice()),
            (other, b"c".as_slice(), b"Z".as_slice(), b"abZ".as_slice()),
        ]
        .into_iter()
        .enumerate()
        {
            let ComputedValue::Bytes(value) = worker
                .eval_args(replacement(invocation, pattern, replace))
                .unwrap()
            else {
                panic!("regexp replacement must own Bytes");
            };
            assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(value.into_option().as_deref(), Some(expected));
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.regexp_metadata().unwrap().is_unbound());
            assert!(worker.regexp_metadata().unwrap().invocation().is_err());
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            if index == 0 {
                first_pattern = patterns.get_cache(11);
                first_parts = parts.get_cache(11);
            } else if index == 1 {
                assert!(Arc::ptr_eq(
                    first_pattern.as_ref().unwrap(),
                    &patterns.get_cache(11).unwrap()
                ));
                assert!(Arc::ptr_eq(
                    first_parts.as_ref().unwrap(),
                    &parts.get_cache(11).unwrap()
                ));
            }
        }
        assert!(patterns.get_cache(11).is_none());
        assert!(parts.get_cache(11).is_none());
        assert!(!Arc::ptr_eq(
            &patterns.get_cache(12).unwrap(),
            &other_patterns.get_cache(12).unwrap()
        ));
        assert!(patterns.clone().get_cache(12).is_none());
        drop(worker);
        assert_eq!(
            parts.get_cache(12).unwrap().as_ref(),
            &[NativeReplacementPart::Literal(b"Y".to_vec())]
        );

        let bad_patterns = NativeContextCache::default();
        let bad_parts = NativeContextCache::default();
        let invocation = NativeRegexpInvocation::new(&bad_patterns, &bad_parts, 7, true, true);
        let mut like = prepare(EvaluatedBytesOp::RegexpLikeNative);
        let storage = like.retained_storage().unwrap();
        assert!(matches!(
            like.eval_args(EvaluatedArgs::Bytes3([
                Some(b"a".to_vec()),
                Some(b"a".to_vec()),
                Some(Vec::new())
            ])),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(like.kernel_invocations(), 0);
        let mut old_error = None;
        for (index, (pattern, flags)) in [
            (b"(".as_slice(), b"".as_slice()),
            (b"a".as_slice(), b"x".as_slice()),
        ]
        .into_iter()
        .enumerate()
        {
            let mut failure = like
                .eval_args_reported(EvaluatedArgs::RegexpLike {
                    invocation: invocation.clone(),
                    text: b"a".to_vec(),
                    pattern: pattern.to_vec(),
                    match_type: flags.to_vec(),
                })
                .unwrap_err();
            assert_eq!(
                failure.operation(),
                Some(EvaluatedBytesOp::RegexpLikeNative)
            );
            assert_eq!(
                failure.sql_failure(),
                Some(EvaluatedSqlFailureKind::RegexpNative)
            );
            assert!(matches!(
                failure.native_regexp_error(),
                Some(NativeRegexpError::Compile(
                    NativeRegexpCompileError::InvalidPattern(_)
                ))
            ));
            let LocalError::Evaluation(error) = failure.error() else {
                panic!("lost real regexp cause");
            };
            let ErrorInner::Evaluate(EvaluateError::Caused(source)) = error.0.as_ref() else {
                panic!("lost typed regexp source");
            };
            assert!(std::ptr::eq(
                failure.native_regexp_error().unwrap(),
                source.downcast_ref::<NativeRegexpError>().unwrap()
            ));
            if index == 0 {
                old_error = bad_patterns.get_cache(7);
            }
            assert!(Arc::ptr_eq(
                old_error.as_ref().unwrap(),
                &bad_patterns.get_cache(7).unwrap()
            ));
            assert_eq!(like.kernel_invocations(), index as u64 + 1);
            assert!(like.regexp_metadata().unwrap().is_unbound());
            assert!(like.is_healthy());
            assert_eq!(like.retained_storage().unwrap(), storage);
            failure.operation = Some(EvaluatedBytesOp::RegexpLikeLegacyCiNative);
            assert!(failure.native_regexp_error().is_none());
            failure.operation = Some(EvaluatedBytesOp::RegexpLikeNative);
            failure.sql_failure = None;
            assert!(failure.native_regexp_error().is_none());
        }
        let ComputedValue::Int(value) = like
            .eval_args(EvaluatedArgs::RegexpLike {
                invocation: NativeRegexpInvocation::new(&bad_patterns, &bad_parts, 10, true, true),
                text: b"a".to_vec(),
                pattern: b"a".to_vec(),
                match_type: Vec::new(),
            })
            .unwrap()
        else {
            panic!("regexp LIKE must own Int");
        };
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), Some(1));
        assert_eq!(like.kernel_invocations(), 3);
        assert!(like.regexp_metadata().unwrap().is_unbound());
        let mut substr = prepare(EvaluatedBytesOp::RegexpSubstrNative);
        let ComputedValue::Bytes(value) = substr
            .eval_args(EvaluatedArgs::RegexpSubstr {
                invocation: NativeRegexpInvocation::new(&bad_patterns, &bad_parts, 8, true, true),
                text: b"ab".to_vec(),
                pattern: b".".to_vec(),
                pos: 2,
                occurrence: 1,
                match_type: Vec::new(),
            })
            .unwrap()
        else {
            panic!("regexp substring must own Bytes");
        };
        assert_eq!(value.into_option(), Some(b"b".to_vec()));
        assert!(substr.regexp_metadata().unwrap().is_unbound());
        assert_eq!(substr.kernel_invocations(), 1);

        let fresh_patterns = NativeContextCache::default();
        let fresh_parts = NativeContextCache::default();
        let invocation = NativeRegexpInvocation::new(&fresh_patterns, &fresh_parts, 9, true, true);
        let instr = |return_option, match_type| EvaluatedArgs::RegexpInstr {
            invocation: invocation.clone(),
            text: b"ab".to_vec(),
            pattern: b"b".to_vec(),
            pos: 1,
            occurrence: 1,
            return_option,
            match_type,
        };
        let mut position = prepare(EvaluatedBytesOp::RegexpInstrNative);
        assert!(matches!(
            position.eval_args(instr(0, ReadyBytesArg::Undemanded)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert!(matches!(
            position.eval_args(instr(0, ReadyBytesArg::Value(None))),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(position.kernel_invocations(), 0);
        let failure = position
            .eval_args_reported(instr(2, ReadyBytesArg::Undemanded))
            .unwrap_err();
        assert!(matches!(
            failure.native_regexp_error(),
            Some(NativeRegexpError::InvalidReturnOption(2))
        ));
        assert!(fresh_patterns.get_cache(9).is_none());
        assert!(fresh_parts.get_cache(9).is_none());
        assert!(position.regexp_metadata().unwrap().is_unbound());
        let ComputedValue::Int(value) = position
            .eval_args(instr(0, ReadyBytesArg::Value(Some(Vec::new()))))
            .unwrap()
        else {
            panic!("regexp position must own Int");
        };
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), Some(2));
        assert_eq!(position.kernel_invocations(), 2);
        assert!(position.is_healthy());
        for (operation, expected) in [
            (EvaluatedBytesOp::RegexpLikeLegacyCiNative, 1),
            (EvaluatedBytesOp::RegexpLikeLegacyBinNative, 0),
        ] {
            let mut legacy = prepare(operation);
            let ComputedValue::Int(value) = legacy
                .eval_args(EvaluatedArgs::Bytes2(
                    Some(b"ABC".to_vec()),
                    Some(b"abc".to_vec()),
                ))
                .unwrap()
            else {
                panic!("legacy regexp must own Int");
            };
            assert_eq!(value.into_option(), Some(expected));
            assert_eq!(legacy.kernel_invocations(), 1);
        }
        let mut missing = prepare(EvaluatedBytesOp::RegexpMissingLegacyNative);
        assert!(matches!(
            missing.eval_args(EvaluatedArgs::NullWitness(None)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(missing.kernel_invocations(), 0);
        let ComputedValue::Int(value) = missing.eval_args(EvaluatedArgs::NoArgs).unwrap() else {
            panic!("legacy missing operands must retain Int result domain");
        };
        assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
        assert_eq!(value.into_option(), None);
        assert_eq!(missing.kernel_invocations(), 1);
        assert!(missing.is_healthy());
        for operation in [
            EvaluatedBytesOp::RegexpNullIntNative,
            EvaluatedBytesOp::RegexpNullBytesNative,
        ] {
            let mut null = prepare(operation);
            assert!(matches!(
                null.eval_args(EvaluatedArgs::NullWitness(Some(0))),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(null.kernel_invocations(), 0);
            match null.eval_args(EvaluatedArgs::NullWitness(None)).unwrap() {
                ComputedValue::Int(value) => assert_eq!(value.into_option(), None),
                ComputedValue::Bytes(value) => assert_eq!(value.into_option(), None),
                _ => panic!("wrong genuine-NULL result domain"),
            }
            assert_eq!(null.kernel_invocations(), 1);
            assert!(null.is_healthy());
        }
    }

    #[test]
    fn regexp_dispatch_binding_guard_scope_accounting_and_unwind() {
        use crate::{NativeCachedRegexp, NativeContextCache, NativeReplacementPart};
        let patterns = NativeContextCache::<NativeCachedRegexp>::default();
        let parts = NativeContextCache::<Vec<NativeReplacementPart>>::default();
        let invocation = NativeRegexpInvocation::new(&patterns, &parts, 1, true, true);
        let payload = NativeRegexpCallMetadata::new(NativeRegexpKind::Like);
        assert!(matches!(
            payload.record_known_cache_bytes(Some(1)),
            Err(LocalError::InvalidSpec(_))
        ));
        payload.bind(invocation.clone(), 30).unwrap();
        payload.record_known_cache_bytes(Some(10)).unwrap();
        payload.record_known_cache_bytes(Some(20)).unwrap();
        assert_eq!(payload.known_cache_bytes.get(), Some(30));
        assert!(matches!(
            payload.record_known_cache_bytes(Some(1)),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(payload.known_cache_bytes.get(), Some(31));
        payload.unbind().unwrap();
        assert!(payload.is_unbound());
        payload.bind(invocation.clone(), usize::MAX).unwrap();
        payload.record_known_cache_bytes(Some(usize::MAX)).unwrap();
        assert!(matches!(
            payload.record_known_cache_bytes(Some(1)),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(payload.known_cache_bytes.get(), None);
        payload.unbind().unwrap();
        payload.bind(invocation.clone(), usize::MAX).unwrap();
        assert!(matches!(
            payload.record_known_cache_bytes(None),
            Err(LocalError::ResourceLimit(_))
        ));
        payload.unbind().unwrap();
        assert!(payload.is_unbound());
        assert!(patterns.get_cache(1).is_none());
        let args = || EvaluatedArgs::RegexpLike {
            invocation: invocation.clone(),
            text: b"a".to_vec(),
            pattern: b"a".to_vec(),
            match_type: Vec::new(),
        };
        let mut zero = prepare_evaluated_bytes(
            EvaluatedBytesOp::RegexpLikeNative,
            LocalCompileContext::default(),
            ExecutionLimits {
                max_steps: 0,
                ..ExecutionLimits::default()
            },
            usize::MAX,
        )
        .unwrap();
        let storage = zero.retained_storage().unwrap();
        let failure = zero.eval_args_reported(args()).unwrap_err();
        assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
        assert_eq!(failure.sql_failure(), None);
        assert!(failure.native_regexp_error().is_none());
        assert_eq!(zero.kernel_invocations(), 0);
        assert!(zero.regexp_metadata().unwrap().is_unbound());
        assert!(zero.is_healthy());
        assert_eq!(zero.retained_storage().unwrap(), storage);
        assert!(patterns.get_cache(1).is_none());
        let mut worker = prepare_evaluated_bytes(
            EvaluatedBytesOp::RegexpLikeNative,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        let storage = worker.retained_storage().unwrap();
        let metadata_bytes = worker
            .program
            .expression
            .retained_metadata_heap_bytes()
            .unwrap();
        let old_known = evaluated_ascii_owned_heap_bytes(
            worker.program.expression.capacity(),
            worker.program.schema.capacity(),
            metadata_bytes,
            worker.ctx.warnings.warnings.capacity(),
        )
        .unwrap();
        assert_eq!(
            storage.total_bytes(),
            mem::size_of::<EvaluatedBytesWorker>()
                + old_known
                + mem::size_of::<NativeRegexpCallMetadata>()
        );
        // Private cap fault injection: the actual generated kernel observes a
        // real cache Arc, then record refusal must escape as infrastructure.
        let (ready, arity, bound) = args().into_values(0).unwrap();
        let mut sql_failure = None;
        worker.begin_invocation().unwrap();
        let result = {
            let guard = RegexpBindingGuard {
                worker: &mut worker,
            };
            guard
                .worker
                .regexp_metadata()
                .unwrap()
                .bind(bound.unwrap(), 0)
                .unwrap();
            guard.worker.eval_ready(ready, arity, &mut sql_failure)
        };
        let error = worker.finish_invocation(result).unwrap_err();
        assert!(matches!(error, LocalError::ResourceLimit(_)));
        assert_eq!(sql_failure, None);
        assert_eq!(worker.kernel_invocations(), 1);
        assert!(patterns.get_cache(1).is_some());
        assert!(worker.regexp_metadata().unwrap().is_unbound());
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        // An unwind clears handles and observations, but never heals poison or
        // empties the statement's cache. This is not a callback-based kernel.
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            worker.begin_invocation().unwrap();
            let guard = RegexpBindingGuard {
                worker: &mut worker,
            };
            guard
                .worker
                .regexp_metadata()
                .unwrap()
                .bind(invocation.clone(), usize::MAX)
                .unwrap();
            guard
                .worker
                .regexp_metadata()
                .unwrap()
                .record_known_cache_bytes(Some(9))
                .unwrap();
            panic!("regexp binding unwind probe");
        }));
        assert!(panic.is_err());
        assert!(worker.regexp_metadata().unwrap().is_unbound());
        assert!(worker.poisoned);
        assert!(!worker.is_healthy());
        assert_eq!(worker.kernel_invocations(), 1);
        drop(worker);
        assert!(patterns.get_cache(1).is_some());
    }

    #[test]
    fn vector_dispatch_owned_values_typed_failures_and_storage() {
        let prepare = |operation| {
            prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap()
        };
        let vector = |values: &[f32]| NativeVectorFloat32::must_create(values.to_vec());
        // Fixed native builtin_ext/vec.rs scalar rows, not provider-generated goldens.
        let mut parser = prepare(EvaluatedBytesOp::VecFromTextNative);
        let storage = parser.retained_storage().unwrap();
        let ComputedValue::NativeVector(null) =
            parser.eval_args(EvaluatedArgs::Bytes(None)).unwrap()
        else {
            panic!("FROM_TEXT NULL lost its vector result domain");
        };
        assert_eq!(
            null.metadata(),
            ComputedNativeVectorMetadata::OwnNativeVector
        );
        assert!(null.value().is_none());
        assert!(null.into_value().is_none());
        let ComputedValue::NativeVector(value) = parser
            .eval_args(EvaluatedArgs::Bytes(Some(b"[1,2]".to_vec())))
            .unwrap()
        else {
            panic!("FROM_TEXT must own a native vector");
        };
        assert_eq!(
            value.metadata(),
            ComputedNativeVectorMetadata::OwnNativeVector
        );
        assert_eq!(value.value().unwrap().elements(), &[1.0, 2.0]);
        let pointer = value.value().unwrap().elements().as_ptr();
        let owned = value.into_value().unwrap();
        assert_eq!(owned.elements().as_ptr(), pointer);
        let failure = parser
            .eval_args_reported(EvaluatedArgs::Bytes(Some(b"not vector".to_vec())))
            .unwrap_err();
        assert_eq!(
            failure.operation(),
            Some(EvaluatedBytesOp::VecFromTextNative)
        );
        assert_eq!(
            failure.sql_failure(),
            Some(EvaluatedSqlFailureKind::VectorNative)
        );
        assert!(failure.native_vector_error().is_some());
        assert_eq!(parser.kernel_invocations(), 3);
        assert!(parser.is_healthy());
        assert_eq!(parser.retained_storage().unwrap(), storage);
        drop(parser);
        assert_eq!(owned.elements(), &[1.0, 2.0]);

        let mut text = prepare(EvaluatedBytesOp::VecAsTextNative);
        let ComputedValue::Bytes(value) = text
            .eval_args(EvaluatedArgs::NativeVector(Some(owned)))
            .unwrap()
        else {
            panic!("AS_TEXT must own Bytes");
        };
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        let owned = value.into_option().unwrap();
        assert_eq!(owned, b"[1,2]");
        assert_eq!(text.kernel_invocations(), 1);
        assert!(text.is_healthy());
        drop(text);
        assert_eq!(owned, b"[1,2]");
        let mut dims = prepare(EvaluatedBytesOp::VecDimsNative);
        assert!(matches!(
            dims.eval_args(EvaluatedArgs::Bytes(None)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(dims.kernel_invocations(), 0);
        for (index, (value, expected)) in [(None, None), (Some(vector(&[1.0, 2.0])), Some(2))]
            .into_iter()
            .enumerate()
        {
            let ComputedValue::Int(value) =
                dims.eval_args(EvaluatedArgs::NativeVector(value)).unwrap()
            else {
                panic!("DIMS must own Int");
            };
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), expected);
            assert_eq!(dims.kernel_invocations(), index as u64 + 1);
            assert!(dims.is_healthy());
        }
        let metrics: [(EvaluatedBytesOp, &[f32], &[f32], Option<f64>); 4] = [
            (
                EvaluatedBytesOp::VecL1DistanceNative,
                &[1.0, 2.0],
                &[3.0, 5.0],
                Some(5.0),
            ),
            (
                EvaluatedBytesOp::VecL2DistanceNative,
                &[0.0, 0.0],
                &[3.0, 4.0],
                Some(5.0),
            ),
            (
                EvaluatedBytesOp::VecNegativeInnerProductNative,
                &[1.0, 2.0],
                &[3.0, 4.0],
                Some(-11.0),
            ),
            (
                EvaluatedBytesOp::VecCosineDistanceNative,
                &[0.0],
                &[1.0],
                None,
            ),
        ];
        for (operation, left, right, expected) in metrics {
            let mut worker = prepare(operation);
            let storage = worker.retained_storage().unwrap();
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::Bytes2(None, None)),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            let ComputedValue::Ieee754Bits(value) = worker
                .eval_args(EvaluatedArgs::NativeVector2(
                    Some(vector(left)),
                    Some(vector(right)),
                ))
                .unwrap()
            else {
                panic!("vector metric must own IEEE754 bits");
            };
            assert_eq!(
                value.metadata(),
                ComputedIeee754BitsMetadata::OwnIeee754Bits
            );
            assert_eq!(value.into_option(), expected.map(f64::to_bits));
            let mut failure = worker
                .eval_args_reported(EvaluatedArgs::NativeVector2(
                    Some(vector(&[1.0])),
                    Some(vector(&[1.0, 2.0])),
                ))
                .unwrap_err();
            assert_eq!(failure.operation(), Some(operation));
            assert_eq!(
                failure.sql_failure(),
                Some(EvaluatedSqlFailureKind::VectorNative)
            );
            let cause = failure.native_vector_error().unwrap();
            assert_eq!(
                cause.to_string(),
                "vectors have different dimensions: 1 and 2"
            );
            let LocalError::Evaluation(error) = failure.error() else {
                panic!("lost actual evaluation cause");
            };
            let ErrorInner::Evaluate(EvaluateError::Caused(source)) = error.0.as_ref() else {
                panic!("lost typed source");
            };
            assert!(std::ptr::eq(
                cause,
                source.downcast_ref::<NativeVectorError>().unwrap()
            ));
            assert_eq!(worker.kernel_invocations(), 2);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            // Private negative receipts only: no production forging constructor.
            failure.operation = Some(EvaluatedBytesOp::VecDimsNative);
            assert!(failure.native_vector_error().is_none());
            failure.operation = Some(operation);
            failure.sql_failure = None;
            assert!(failure.native_vector_error().is_none());
            failure.sql_failure = Some(EvaluatedSqlFailureKind::VectorNative);
            failure.error = LocalError::Evaluation(
                EvaluateError::Other("vectors have different dimensions: 1 and 2".into()).into(),
            );
            assert!(failure.native_vector_error().is_none());
        }
        let mut norm = prepare(EvaluatedBytesOp::VecL2NormNative);
        let mut nan = NativeVectorFloat32::init(1);
        nan.elements_mut()[0] = f32::from_bits(0x7fc0_0042);
        let mut inf = NativeVectorFloat32::init(1);
        inf.elements_mut()[0] = f32::INFINITY;
        // The finite row is original; the mutable raw rows probe the locked bit domain.
        for (index, (input, expected)) in [
            (Some(vector(&[3.0, 4.0])), Some(5.0_f64.to_bits())),
            (Some(nan), None),
            (Some(inf), Some(f64::INFINITY.to_bits())),
            (None, None),
        ]
        .into_iter()
        .enumerate()
        {
            let ComputedValue::Ieee754Bits(value) =
                norm.eval_args(EvaluatedArgs::NativeVector(input)).unwrap()
            else {
                panic!("norm must retain its IEEE754 result");
            };
            assert_eq!(value.into_option(), expected);
            assert_eq!(norm.kernel_invocations(), index as u64 + 1);
            assert!(norm.is_healthy());
        }
        let mut null = prepare(EvaluatedBytesOp::VecRealNullNative);
        for invalid in [
            EvaluatedArgs::NullWitness(Some(0)),
            EvaluatedArgs::NativeVector2(None, None),
            EvaluatedArgs::Int(None),
        ] {
            assert!(matches!(
                null.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(null.kernel_invocations(), 0);
        }
        let ComputedValue::Ieee754Bits(value) =
            null.eval_args(EvaluatedArgs::NullWitness(None)).unwrap()
        else {
            panic!("real NULL witness must retain IEEE754 result metadata");
        };
        assert_eq!(
            value.metadata(),
            ComputedIeee754BitsMetadata::OwnIeee754Bits
        );
        assert_eq!(value.into_option(), None);
        assert_eq!(null.kernel_invocations(), 1);
        assert!(null.is_healthy());
        let mut bounded = prepare_evaluated_bytes(
            EvaluatedBytesOp::VecRealNullNative,
            LocalCompileContext::default(),
            ExecutionLimits {
                max_steps: 0,
                ..ExecutionLimits::default()
            },
            usize::MAX,
        )
        .unwrap();
        let failure = bounded
            .eval_args_reported(EvaluatedArgs::NullWitness(None))
            .unwrap_err();
        assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
        assert_eq!(bounded.kernel_invocations(), 0);
        assert_eq!(failure.sql_failure(), None);
        assert!(failure.native_vector_error().is_none());
        assert!(bounded.is_healthy());

        // Charge source capacity, not just the one live f32, before conversion.
        let mut elements = Vec::with_capacity(2048);
        elements.push(1.0);
        let source = NativeVectorFloat32::must_create(elements);
        let source_bytes = source.elements_capacity() * mem::size_of::<f32>();
        let base = dims.retained_storage().unwrap().total_bytes();
        let mut bounded = prepare_evaluated_bytes(
            EvaluatedBytesOp::VecDimsNative,
            LocalCompileContext::default(),
            ExecutionLimits {
                max_retained_bytes: base + source_bytes - 1,
                ..ExecutionLimits::default()
            },
            usize::MAX,
        )
        .unwrap();
        assert!(matches!(
            bounded.eval_args(EvaluatedArgs::NativeVector(Some(source))),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(bounded.kernel_invocations(), 0);
        assert!(bounded.is_healthy());
        // Layout-only materialization keeps raw bits; transport Eq must not
        // change the underlying vector's NaN/zero floating-point PartialEq.
        let mut raw = NativeVectorFloat32::init(2);
        raw.elements_mut()
            .copy_from_slice(&[f32::from_bits(0x7fc0_0042), -0.0]);
        assert_ne!(raw, raw);
        let image = raw.serialize();
        let budget = EvalBudget::exact(ExecutionLimits::default()).unwrap();
        let result = ComputedNativeVector {
            value: Some(materialize_native_vector(&image, image.capacity(), 0, &budget).unwrap()),
        };
        let same = ComputedNativeVector {
            value: Some(raw.clone()),
        };
        assert_eq!(result, same);
        assert_eq!(result, result);
        raw.elements_mut()[1] = 0.0;
        assert_ne!(result, ComputedNativeVector { value: Some(raw) });
        let mut suffix = image.clone();
        suffix.push(0);
        assert!(matches!(
            materialize_native_vector(&suffix, suffix.capacity(), 0, &budget),
            Err(LocalError::InvalidBatch(_))
        ));
        assert!(matches!(
            materialize_native_vector(&[], 0, 0, &budget),
            Err(LocalError::InvalidBatch(_))
        ));
        let bounded = EvalBudget::exact(ExecutionLimits {
            max_retained_bytes: image.capacity() + 2 * mem::size_of::<f32>() - 1,
            ..ExecutionLimits::default()
        })
        .unwrap();
        assert!(matches!(
            materialize_native_vector(&image, image.capacity(), 0, &bounded),
            Err(LocalError::ResourceLimit(_))
        ));
    }

    #[test]
    fn uuid_translate_dispatch_authenticates_five_causes_and_payload() {
        let input = vec![0xff, 0, b'x'];
        let cases = [
            (
                EvaluatedBytesOp::UuidToBinParseNative,
                EvaluatedArgs::Bytes(Some(b" 6ccd780c-baba-1026-9564-5b8c656024db".to_vec())),
                EvaluatedSqlFailureKind::UuidToBinWhitespace,
            ),
            (
                EvaluatedBytesOp::UuidToBinParseNative,
                EvaluatedArgs::Bytes(Some(b"abc".to_vec())),
                EvaluatedSqlFailureKind::UuidToBinInvalid,
            ),
            (
                EvaluatedBytesOp::UuidVersionNative,
                EvaluatedArgs::Bytes(Some(b"abc".to_vec())),
                EvaluatedSqlFailureKind::UuidVersionInvalid,
            ),
            (
                EvaluatedBytesOp::UuidTimestampNative,
                EvaluatedArgs::Bytes(Some(b"abc".to_vec())),
                EvaluatedSqlFailureKind::UuidTimestampInvalid,
            ),
            (
                EvaluatedBytesOp::BinToUuidNative,
                EvaluatedArgs::BytesInt(Some(input.clone()), Some(0)),
                EvaluatedSqlFailureKind::BinToUuidInvalidLength,
            ),
        ];
        for (operation, args, kind) in cases {
            let mut worker = prepare_evaluated_bytes(
                operation,
                LocalCompileContext::default(),
                ExecutionLimits::default(),
                usize::MAX,
            )
            .unwrap();
            let storage = worker.retained_storage().unwrap();
            let rejected = worker
                .eval_args_reported(EvaluatedArgs::NoArgs)
                .unwrap_err();
            assert!(matches!(rejected.error(), LocalError::InvalidBatch(_)));
            assert_eq!(rejected.operation(), None);
            assert_eq!(rejected.sql_failure(), None);
            assert_eq!(rejected.bin_to_uuid_input(), None);
            assert_eq!(worker.kernel_invocations(), 0);
            let failure = worker.eval_args_reported(args).unwrap_err();
            assert_eq!(failure.operation(), Some(operation));
            assert_eq!(failure.sql_failure(), Some(kind));
            assert_eq!(worker.kernel_invocations(), 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            let LocalError::Evaluation(error) = failure.error() else {
                panic!("UUID SQL failure lost its typed evaluation cause");
            };
            assert!(matches!(
                (kind, error.0.as_ref()),
                (
                    EvaluatedSqlFailureKind::UuidToBinWhitespace,
                    ErrorInner::Evaluate(EvaluateError::UuidToBinWhitespace)
                ) | (
                    EvaluatedSqlFailureKind::UuidToBinInvalid,
                    ErrorInner::Evaluate(EvaluateError::UuidToBinInvalid)
                ) | (
                    EvaluatedSqlFailureKind::UuidVersionInvalid,
                    ErrorInner::Evaluate(EvaluateError::UuidVersionInvalid)
                ) | (
                    EvaluatedSqlFailureKind::UuidTimestampInvalid,
                    ErrorInner::Evaluate(EvaluateError::UuidTimestampInvalid)
                ) | (
                    EvaluatedSqlFailureKind::BinToUuidInvalidLength,
                    ErrorInner::Evaluate(EvaluateError::BinToUuidInvalidLength { .. })
                )
            ));
            if let ErrorInner::Evaluate(EvaluateError::BinToUuidInvalidLength { input: actual }) =
                error.0.as_ref()
            {
                let borrowed = failure.bin_to_uuid_input().unwrap();
                assert_eq!(borrowed, input.as_slice());
                assert_eq!(
                    borrowed.as_ptr(),
                    actual.as_ptr(),
                    "borrow the owned cause, not a saved/reparsed input"
                );
            } else {
                assert_eq!(failure.bin_to_uuid_input(), None);
            }
            drop(worker);
            assert_eq!(
                failure.bin_to_uuid_input(),
                if kind == EvaluatedSqlFailureKind::BinToUuidInvalidLength {
                    Some(input.as_slice())
                } else {
                    None
                }
            );
            let before = failure.error().to_string();
            assert_eq!(failure.into_error().to_string(), before);
        }

        // A missing prepared flag is a defensive kernel failure, not the SQL
        // invalid-length cause. Other BytesInt recipes were not narrowed to Swap.
        let mut foreign = prepare_evaluated_bytes(
            EvaluatedBytesOp::BinToUuidNative,
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap();
        let failure = foreign
            .eval_args_reported(EvaluatedArgs::BytesInt(Some(input.clone()), None))
            .unwrap_err();
        assert!(matches!(failure.error(), LocalError::Evaluation(_)));
        assert_eq!(foreign.kernel_invocations(), 1);
        assert_eq!(failure.operation(), None);
        assert_eq!(failure.sql_failure(), None);
        assert_eq!(failure.bin_to_uuid_input(), None);
        assert!(foreign.is_healthy());
        let mut bounded = prepare_evaluated_bytes(
            EvaluatedBytesOp::BinToUuidNative,
            LocalCompileContext::default(),
            ExecutionLimits {
                max_steps: 0,
                ..ExecutionLimits::default()
            },
            usize::MAX,
        )
        .unwrap();
        let failure = bounded
            .eval_args_reported(EvaluatedArgs::BytesInt(Some(input.clone()), Some(0)))
            .unwrap_err();
        assert!(matches!(failure.error(), LocalError::ResourceLimit(_)));
        assert_eq!(bounded.kernel_invocations(), 0);
        assert_eq!(failure.operation(), None);
        assert_eq!(failure.sql_failure(), None);
        assert_eq!(failure.bin_to_uuid_input(), None);
        assert!(bounded.is_healthy());

        // Private negative receipts exercise the getter's independent seals;
        // no production constructor or additional report DTO is exposed.
        let error = || {
            LocalError::Evaluation(
                EvaluateError::BinToUuidInvalidLength {
                    input: input.clone(),
                }
                .into(),
            )
        };
        let unreported = ReportedEvaluatedFailure::unreported(error());
        assert_eq!(unreported.bin_to_uuid_input(), None);
        let mut unrelated = ReportedEvaluatedFailure {
            error: error(),
            operation: Some(EvaluatedBytesOp::UuidVersionNative),
            sql_failure: Some(EvaluatedSqlFailureKind::BinToUuidInvalidLength),
        };
        assert_eq!(unrelated.bin_to_uuid_input(), None);
        unrelated.operation = Some(EvaluatedBytesOp::BinToUuidNative);
        unrelated.sql_failure = Some(EvaluatedSqlFailureKind::UuidVersionInvalid);
        assert_eq!(unrelated.bin_to_uuid_input(), None);
        unrelated.sql_failure = Some(EvaluatedSqlFailureKind::BinToUuidInvalidLength);
        unrelated.error = LocalError::Evaluation(EvaluateError::UuidVersionInvalid.into());
        assert_eq!(unrelated.bin_to_uuid_input(), None);
    }

    #[test]
    fn uncompress_result_envelope_is_canonical() {
        assert_eq!(
            decode_uncompress_frame(None).unwrap(),
            UncompressFrame::Null
        );
        assert_eq!(
            decode_uncompress_frame(Some(&[0])).unwrap(),
            UncompressFrame::Value(&[])
        );
        assert_eq!(
            decode_uncompress_frame(Some(&[0, 0, 1, 2, 255])).unwrap(),
            UncompressFrame::Value(&[0, 1, 2, 255])
        );
        assert_eq!(
            decode_uncompress_frame(Some(&[1])).unwrap(),
            UncompressFrame::Corrupt
        );
        assert_eq!(
            decode_uncompress_frame(Some(&[2])).unwrap(),
            UncompressFrame::OutputLimit
        );
        let malformed: &[&[u8]] = &[&[], &[3], &[255], &[1, 0], &[2, 0]];
        for encoded in malformed {
            assert!(matches!(
                decode_uncompress_frame(Some(*encoded)),
                Err(LocalError::InvalidBatch(_))
            ));
        }
    }

    #[test]
    fn json_report_envelope_is_recipe_specific_and_exact() {
        use EvaluatedBytesOp::{
            JsonDepthNative, JsonStorageFreeNative, JsonStorageSizeNative, JsonTypeBinaryNative,
            JsonTypeTextNative,
        };
        for operation in [
            JsonTypeTextNative,
            JsonTypeBinaryNative,
            JsonDepthNative,
            JsonStorageFreeNative,
            JsonStorageSizeNative,
        ] {
            assert_eq!(
                decode_json_report_frame(operation, None).unwrap(),
                JsonReportFrame::Null
            );
            assert_eq!(
                decode_json_report_frame(operation, Some(&[2])).unwrap(),
                JsonReportFrame::InvalidText
            );
            let malformed: &[&[u8]] = &[&[], &[3], &[255], &[1, 0], &[2, 0]];
            for encoded in malformed {
                assert!(matches!(
                    decode_json_report_frame(operation, Some(*encoded)),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
        }
        for operation in [JsonTypeTextNative, JsonTypeBinaryNative] {
            assert_eq!(
                decode_json_report_frame(operation, Some(b"\0OBJECT")).unwrap(),
                JsonReportFrame::Bytes(b"OBJECT")
            );
            assert_eq!(
                decode_json_report_frame(operation, Some(&[0])).unwrap(),
                JsonReportFrame::Bytes(&[])
            );
        }
        for operation in [
            JsonTypeTextNative,
            JsonDepthNative,
            JsonStorageFreeNative,
            JsonStorageSizeNative,
        ] {
            assert_eq!(
                decode_json_report_frame(operation, Some(&[1])).unwrap(),
                JsonReportFrame::EmptyText
            );
        }
        assert!(matches!(
            decode_json_report_frame(JsonTypeBinaryNative, Some(&[1])),
            Err(LocalError::InvalidBatch(_))
        ));
        for operation in [
            JsonDepthNative,
            JsonStorageFreeNative,
            JsonStorageSizeNative,
        ] {
            assert_eq!(
                decode_json_report_frame(operation, Some(&[0, 3, 0, 0, 0, 0, 0, 0, 0])).unwrap(),
                JsonReportFrame::Int(3)
            );
            for encoded in [&[0][..], &[0; 8][..], &[0; 10][..]] {
                assert!(matches!(
                    decode_json_report_frame(operation, Some(encoded)),
                    Err(LocalError::InvalidBatch(_))
                ));
            }
        }
        for operation in [
            EvaluatedBytesOp::JsonValidTextNative,
            EvaluatedBytesOp::UncompressNative,
            EvaluatedBytesOp::JsonQuoteNative,
        ] {
            assert!(matches!(
                decode_json_report_frame(operation, None),
                Err(LocalError::InvalidBatch(_))
            ));
        }
    }

    fn new_worker() -> EvaluatedAsciiWorker {
        prepare_evaluated_ascii(
            LocalCompileContext::default(),
            ExecutionLimits::default(),
            usize::MAX,
        )
        .unwrap()
    }

    #[test]
    fn test_evaluated_ascii_worker_ownership_traits() {
        static_assertions::assert_impl_all!(EvaluatedAsciiWorker: Send);
        static_assertions::assert_not_impl_any!(EvaluatedAsciiWorker: Sync, Clone);
        static_assertions::assert_impl_all!(ComputedInt: Copy, Send, Sync);
        static_assertions::assert_impl_all!(WorkerStorage: Copy, Send, Sync);
        type Owner = Arc<Mutex<Vec<EvaluatedAsciiWorker>>>;
        static_assertions::assert_impl_all!(Owner: Send, Sync);
    }

    #[test]
    fn test_evaluated_ascii_factory_fully_prewarms_without_invocation() {
        let worker = new_worker();
        let storage = worker.retained_storage().unwrap();
        assert!(worker.is_healthy());
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(
            worker
                .inner
                .program
                .expression
                .retained_metadata_heap_bytes()
                .unwrap()
                > 0
        );
        assert_eq!(worker.inner.program.expression.node_count(), 2);
        assert_eq!(worker.inner.program.expression.work_count(), 2);
        assert_eq!(worker.inner.program.expression.column_ref_count(), 1);
        assert_eq!(
            worker.inner.program.expression.referenced_column_offsets(),
            &[0]
        );
        assert_eq!(worker.retained_storage().unwrap(), storage);
        assert_eq!(worker.inner.accepted_storage, storage);
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(evaluated_ascii_context_is_sealed(&worker.inner.ctx));
        assert_eq!(worker.inner.ctx.cfg.max_warning_cnt, 0);
        assert_eq!(worker.inner.ctx.warnings.warning_cnt, 0);
        assert!(worker.inner.ctx.warnings.warnings.is_empty());
        assert_eq!(
            storage.inline_bytes(),
            mem::size_of::<EvaluatedAsciiWorker>()
        );
        assert_eq!(
            storage.total_bytes(),
            storage.inline_bytes() + storage.owned_heap_bytes()
        );
    }

    #[test]
    fn test_evaluated_ascii_observer_does_not_warm_a_cold_cache() {
        let mut worker = new_worker();
        // Private fault injection invalidates metadata without changing nodes.
        // An observation must refuse this cold published worker, not allocate.
        let _: &mut [RpnExpressionNode] = worker.inner.program.expression.as_mut();
        assert_eq!(
            worker
                .inner
                .program
                .expression
                .retained_metadata_heap_bytes(),
            Some(0)
        );
        assert!(matches!(
            worker.retained_storage(),
            Err(LocalError::InvalidSpec(_))
        ));
        assert_eq!(
            worker
                .inner
                .program
                .expression
                .retained_metadata_heap_bytes(),
            Some(0)
        );
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(!worker.is_healthy());
    }

    #[test]
    fn test_evaluated_ascii_observer_measures_actual_spare_capacities() {
        let mut worker = new_worker();
        let before = worker.retained_storage().unwrap();
        worker.inner.program.expression.reserve(17);
        worker.inner.program.schema.reserve(11);
        worker.inner.ctx.warnings.warnings.reserve(13);
        // Mutation above intentionally invalidated the cache. Rewarm explicitly
        // in this private test, never inside the observation API.
        assert_eq!(worker.inner.program.expression.node_count(), 2);
        assert_eq!(worker.inner.program.expression.work_count(), 2);
        assert_eq!(
            worker.inner.program.expression.referenced_column_offsets(),
            &[0]
        );
        let storage = worker.retained_storage().unwrap();
        let expected = worker.inner.program.expression.capacity()
            * mem::size_of::<RpnExpressionNode>()
            + worker.inner.program.schema.capacity() * mem::size_of::<tipb::FieldType>()
            + worker
                .inner
                .program
                .expression
                .retained_metadata_heap_bytes()
                .unwrap()
            + mem::size_of::<EvaluatedAsciiConfigAllocation>()
            + worker.inner.ctx.warnings.warnings.capacity() * mem::size_of::<tipb::Error>();
        assert_eq!(storage.owned_heap_bytes(), expected);
        assert!(storage.owned_heap_bytes() > before.owned_heap_bytes());
        assert_eq!(worker.inner.program.expression.len(), 2);
        assert_eq!(worker.inner.program.schema.len(), 1);
        assert!(worker.inner.ctx.warnings.warnings.is_empty());
        assert_eq!(worker.inner.ctx.warnings.warning_cnt, 0);
        assert_eq!(worker.inner.accepted_storage, before);
        assert!(!worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }

    #[test]
    fn test_evaluated_ascii_owner_limit_is_separate_from_execution() {
        let baseline = new_worker().retained_storage().unwrap();
        for limit in [0, baseline.total_bytes() - 1] {
            assert!(matches!(
                prepare_evaluated_ascii(
                    LocalCompileContext::default(),
                    ExecutionLimits::default(),
                    limit,
                ),
                Err(LocalError::ResourceLimit(_))
            ));
        }
        let mut worker = prepare_evaluated_ascii(
            LocalCompileContext::default(),
            ExecutionLimits {
                max_retained_bytes: 0,
                ..ExecutionLimits::default()
            },
            baseline.total_bytes(),
        )
        .unwrap();
        assert_eq!(worker.retained_storage().unwrap(), baseline);
        assert!(matches!(
            worker.eval_one(None),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        // An execution refusal did not change owner storage or rebuild context.
        assert_eq!(worker.retained_storage().unwrap(), baseline);
    }

    #[test]
    fn test_evaluated_ascii_storage_checked_overflow_without_allocations() {
        let storage = WorkerStorage::new(7, 9).unwrap();
        assert_eq!(storage.inline_bytes(), 7);
        assert_eq!(storage.owned_heap_bytes(), 9);
        assert_eq!(storage.total_bytes(), 16);
        for (inline, heap) in [(usize::MAX, 1), (1, usize::MAX)] {
            assert!(matches!(
                WorkerStorage::new(inline, heap),
                Err(LocalError::ResourceLimit(_))
            ));
        }
        for (nodes, schema, metadata, warnings) in [
            (usize::MAX, 0, 0, 0),
            (0, usize::MAX, 0, 0),
            (0, 0, usize::MAX, 0),
            (0, 0, 0, usize::MAX),
        ] {
            assert!(matches!(
                evaluated_ascii_owned_heap_bytes(nodes, schema, metadata, warnings),
                Err(LocalError::ResourceLimit(_))
            ));
        }
    }

    #[test]
    fn test_evaluated_ascii_reuses_program_context_and_computed_identity() {
        let mut worker = new_worker();
        let storage = worker.retained_storage().unwrap();
        let cfg = Arc::as_ptr(&worker.inner.ctx.cfg);
        let nodes = worker.inner.program.expression.as_ptr();
        let refs = worker
            .inner
            .program
            .expression
            .referenced_column_offsets()
            .as_ptr();
        let cases = [
            (None, None),
            (Some(vec![]), Some(0)),
            (Some(vec![0, b'x']), Some(0)),
            (Some(vec![0xff, 0xfe]), Some(255)),
            (Some("你好".as_bytes().to_vec()), Some(228)),
            (Some(b"2".to_vec()), Some(50)),
        ];
        for (index, (input, expected)) in cases.into_iter().enumerate() {
            worker.inner.state.row = [99];
            let value = worker.eval_one(input).unwrap();
            assert_eq!(value.value(), expected);
            assert_eq!(value.metadata(), ComputedIntMetadata::OwnSignedInt);
            assert_eq!(value.into_option(), expected);
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert_eq!(worker.inner.state.row, [0]);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            assert_eq!(Arc::as_ptr(&worker.inner.ctx.cfg), cfg);
            assert_eq!(worker.inner.program.expression.as_ptr(), nodes);
            assert_eq!(
                worker
                    .inner
                    .program
                    .expression
                    .referenced_column_offsets()
                    .as_ptr(),
                refs
            );
        }
    }

    #[test]
    fn test_evaluated_ascii_count_only_warning_poison_is_not_reset() {
        let mut worker = new_worker();
        worker.inner.ctx.warnings.warning_cnt = 1;
        assert!(worker.inner.ctx.warnings.warnings.is_empty());
        assert!(!worker.is_healthy());
        assert!(worker.retained_storage().is_err());
        assert!(matches!(
            worker.eval_one(None),
            Err(LocalError::InvalidSpec(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert_eq!(worker.inner.ctx.warnings.warning_cnt, 1);
        assert!(worker.inner.ctx.warnings.warnings.is_empty());
        assert!(worker.inner.poisoned);
        // The caller must now drop this worker, never reset/recycle it.
    }

    #[test]
    fn test_evaluated_ascii_detail_only_warning_poison_is_not_reset() {
        let mut worker = new_worker();
        let mut detail = tipb::Error::default();
        detail.set_msg("unexpected private warning".into());
        worker.inner.ctx.warnings.warnings.push(detail);
        assert_eq!(worker.inner.ctx.warnings.warning_cnt, 0);
        assert!(!worker.is_healthy());
        assert!(worker.retained_storage().is_err());
        assert!(matches!(
            worker.eval_one(Some(b"x".to_vec())),
            Err(LocalError::InvalidSpec(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert_eq!(worker.inner.ctx.warnings.warning_cnt, 0);
        assert_eq!(worker.inner.ctx.warnings.warnings.len(), 1);
        assert_eq!(
            worker.inner.ctx.warnings.warnings[0].get_msg(),
            "unexpected private warning"
        );
        assert!(worker.inner.poisoned);
    }

    #[test]
    fn test_evaluated_ascii_config_seal_and_exclusive_arc() {
        let mutations: [fn(&mut EvalConfig); 8] = [
            |cfg| cfg.tz = Tz::from_offset(0).unwrap(),
            |cfg| cfg.flag = Flag::TRUNCATE_AS_WARNING,
            |cfg| cfg.sql_mode = SqlMode::STRICT_ALL_TABLES,
            |cfg| cfg.max_warning_cnt = 1,
            |cfg| cfg.paging_size = Some(1),
            |cfg| cfg.max_keys_read = Some(1),
            |cfg| cfg.div_precision_increment ^= 1,
            |cfg| cfg.is_test = true,
        ];
        for mutate in mutations {
            let mut worker = new_worker();
            mutate(Arc::get_mut(&mut worker.inner.ctx.cfg).unwrap());
            assert!(!worker.is_healthy());
            assert!(worker.retained_storage().is_err());
            assert!(matches!(
                worker.eval_one(None),
                Err(LocalError::InvalidSpec(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.inner.poisoned);
        }
        let mut worker = new_worker();
        let alias = Arc::clone(&worker.inner.ctx.cfg);
        assert!(!worker.is_healthy());
        assert!(worker.retained_storage().is_err());
        assert!(matches!(
            worker.eval_one(None),
            Err(LocalError::InvalidSpec(_))
        ));
        drop(alias);
        assert!(worker.inner.poisoned);
        assert!(!worker.is_healthy());
        assert_eq!(worker.kernel_invocations(), 0);
    }

    #[test]
    fn test_evaluated_ascii_empty_ready_buffer_charges_capacity() {
        let input = Vec::<u8>::with_capacity(4096);
        assert!(input.is_empty());
        let limit = input.capacity() - 1;
        let mut worker = prepare_evaluated_ascii(
            LocalCompileContext::default(),
            ExecutionLimits {
                max_retained_bytes: limit,
                ..ExecutionLimits::default()
            },
            usize::MAX,
        )
        .unwrap();
        let storage = worker.retained_storage().unwrap();
        assert!(matches!(
            worker.eval_one(Some(input)),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.is_healthy());
        assert_eq!(worker.retained_storage().unwrap(), storage);
        // No large input buffer stayed in the worker after refusal.
        worker.inner.state.limits = ExecutionLimits::default();
        assert_eq!(worker.eval_one(Some(vec![])).unwrap().value(), Some(0));
        assert_eq!(worker.kernel_invocations(), 1);
        assert_eq!(worker.retained_storage().unwrap(), storage);
    }

    #[test]
    fn test_evaluated_ascii_caught_unwind_keeps_sticky_poison() {
        let mut worker = new_worker();
        // Exercise the exact admission/poison boundary used by eval_one, without
        // replacing a kernel or installing a production failure callback.
        let panic = catch_unwind(AssertUnwindSafe(|| {
            worker.inner.begin_invocation().unwrap();
            panic!("unwind after evaluated ASCII invocation admission");
        }));
        assert!(panic.is_err());
        assert!(worker.inner.poisoned);
        assert!(evaluated_ascii_context_is_sealed(&worker.inner.ctx));
        assert!(!worker.is_healthy());
        assert!(worker.retained_storage().is_err());
        assert!(matches!(
            worker.eval_one(None),
            Err(LocalError::InvalidSpec(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
    }

    #[test]
    fn test_evaluated_ascii_postflight_preserves_owned_primary_error() {
        let faults: [fn(&mut EvaluatedAsciiWorker); 2] = [
            |worker| worker.inner.ctx.warnings.warning_cnt = 1,
            |worker| worker.inner.ctx.warnings.warnings.reserve(1),
        ];
        for fault in faults {
            let mut worker = new_worker();
            worker.inner.begin_invocation().unwrap();
            let primary: tidb_query_common::Error =
                other_err!("original evaluated ASCII primary error");
            let identity = primary.0.as_ref() as *const _;
            fault(&mut worker);
            let error = worker
                .inner
                .finish_invocation::<ComputedInt>(Err(LocalError::Evaluation(primary)))
                .unwrap_err();
            let LocalError::Evaluation(primary) = error else {
                panic!("postflight replaced the original evaluation error");
            };
            assert!(std::ptr::eq(primary.0.as_ref(), identity));
            assert!(worker.inner.poisoned);
            assert!(!worker.is_healthy());
            assert!(worker.retained_storage().is_err());
            assert_eq!(worker.kernel_invocations(), 0);

            let mut success = new_worker();
            success.inner.begin_invocation().unwrap();
            fault(&mut success);
            assert!(matches!(
                success
                    .inner
                    .finish_invocation(Ok(ComputedInt { value: Some(7) })),
                Err(LocalError::InvalidSpec(_))
            ));
            assert!(success.inner.poisoned);
            assert!(!success.is_healthy());
            assert_eq!(success.kernel_invocations(), 0);
        }
    }

    #[test]
    fn test_evaluated_ascii_owner_growth_refused_even_below_maximum() {
        let mut worker = new_worker();
        let accepted = worker.retained_storage().unwrap();
        worker.inner.ctx.warnings.warnings.reserve(1);
        let actual = worker.retained_storage().unwrap();
        assert!(actual.owned_heap_bytes() > accepted.owned_heap_bytes());
        assert!(actual.total_bytes() < worker.inner.max_worker_retained_bytes);
        assert_eq!(worker.inner.accepted_storage, accepted);
        assert!(!worker.is_healthy());
        assert!(matches!(
            worker.eval_one(None),
            Err(LocalError::InvalidSpec(_))
        ));
        assert!(worker.inner.poisoned);
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.inner.ctx.warnings.warnings.is_empty());
        assert_eq!(worker.inner.ctx.warnings.warning_cnt, 0);
        assert!(worker.inner.ctx.warnings.warnings.capacity() > 0);
    }

    #[test]
    fn test_evaluated_ascii_wrong_compiled_entry_refused_before_invocation() {
        let mut worker = new_worker();
        worker.inner.program = compile_local(
            &LocalExpr::Constant {
                value: ScalarValue::Int(Some(7)),
                field_type: evaluated_ascii_int_type(),
                literal_kind: LiteralKind::Typed,
            },
            &[],
            LocalCompileContext::default(),
        )
        .unwrap();
        assert!(matches!(
            worker.eval_one(None),
            Err(LocalError::InvalidSpec(_))
        ));
        assert_eq!(worker.kernel_invocations(), 0);
        assert!(worker.inner.poisoned);
    }

    #[test]
    fn test_evaluated_ascii_idle_slots_and_active_owner_charged_once() {
        let mut idle = Vec::with_capacity(3);
        idle.push(new_worker());
        let storage = idle[0].retained_storage().unwrap();
        let slots = idle.capacity() * mem::size_of::<EvaluatedAsciiWorker>();
        let parked = slots + storage.owned_heap_bytes();
        let mut active = idle.pop().unwrap();
        assert!(idle.is_empty());
        assert_eq!(idle.capacity() * storage.inline_bytes(), slots);
        // Popping leaves the Vec's slots allocated. The active worker now
        // occupies separate inline storage as well as its owned heap.
        assert_eq!(
            slots
                + idle
                    .iter()
                    .map(|w| w.retained_storage().unwrap().owned_heap_bytes())
                    .sum::<usize>()
                + active.retained_storage().unwrap().total_bytes(),
            parked + storage.inline_bytes()
        );
        assert_eq!(
            active.eval_one(Some(vec![0xff])).unwrap().value(),
            Some(255)
        );
        assert_eq!(active.retained_storage().unwrap(), storage);
        assert_eq!(active.kernel_invocations(), 1);
        idle.push(active);
        assert_eq!(
            idle.capacity() * storage.inline_bytes()
                + idle
                    .iter()
                    .map(|w| w.retained_storage().unwrap().owned_heap_bytes())
                    .sum::<usize>(),
            parked
        );
    }

    #[test]
    #[ignore = "global body delta: run this exact test alone, never with other ASCII tests"]
    fn test_evaluated_ascii_wrapper_body_origin_isolated() {
        let before = crate::impl_string::ascii_test_body_invocations();
        let mut oversized = Vec::with_capacity(4096);
        oversized.push(b'x');
        let mut refused = prepare_evaluated_ascii(
            LocalCompileContext::default(),
            ExecutionLimits {
                max_retained_bytes: oversized.capacity() - 1,
                ..ExecutionLimits::default()
            },
            usize::MAX,
        )
        .unwrap();
        assert!(matches!(
            refused.eval_one(Some(oversized)),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(refused.kernel_invocations(), 0);
        assert_eq!(crate::impl_string::ascii_test_body_invocations(), before);
        assert!(refused.is_healthy());
        drop(refused);

        let mut worker = new_worker();
        assert_eq!(worker.kernel_invocations(), 0);
        assert_eq!(crate::impl_string::ascii_test_body_invocations(), before);
        assert_eq!(worker.eval_one(None).unwrap().value(), None);
        assert_eq!(worker.kernel_invocations(), 1);
        // The actual nullable wrapper ran; only its non-null body was skipped.
        assert_eq!(crate::impl_string::ascii_test_body_invocations(), before);
        for (input, expected) in [(vec![], 0), (vec![0], 0), (vec![0xff], 255)] {
            assert_eq!(
                worker.eval_one(Some(input)).unwrap().value(),
                Some(expected)
            );
        }
        assert_eq!(worker.kernel_invocations(), 4);
        let threads: Vec<_> = (0..2)
            .map(|_| {
                // Each thread owns its worker; no Arc<non-Sync runtime> or
                // input reference crosses an invocation or enters an idle slot.
                thread::spawn(|| {
                    let mut worker = new_worker();
                    let storage = worker.retained_storage().unwrap();
                    assert_eq!(worker.eval_one(None).unwrap().value(), None);
                    assert_eq!(worker.eval_one(Some(vec![b'A'])).unwrap().value(), Some(65));
                    assert_eq!(worker.kernel_invocations(), 2);
                    assert_eq!(worker.retained_storage().unwrap(), storage);
                    worker
                })
            })
            .collect();
        for thread in threads {
            assert!(thread.join().unwrap().is_healthy());
        }
        assert_eq!(
            crate::impl_string::ascii_test_body_invocations().checked_sub(before),
            Some(5)
        );
    }
}
