// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    mem,
    sync::{Arc, atomic::AtomicUsize},
};

use tidb_query_common::error::{ErrorInner, EvaluateError};
use tidb_query_datatype::{
    EvalType,
    codec::{
        batch::LazyBatchColumnVec,
        collation::native::NativeCollation,
        data_type::{BATCH_MAX_SIZE, ChunkedVecBytes, ScalarValue, ScalarValueRef, VectorValue},
        mysql::{DEFAULT_DIV_FRAC_INCR, Decimal, Tz, decimal::NativeDecimalError},
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
    },
    runtime::{EvalBudget, bytes_min_storage_bytes, int_min_storage_bytes, vector_storage_bytes},
};
use crate::{
    RpnExpressionNode, RpnStackNode, RpnStackNodeVectorValue,
    impl_string::{
        ConcatKind, FieldKind, PreparedCharArgs, PreparedConcatArgs, PreparedExportSetArgs,
        PreparedFieldArgs, PreparedFindInSetKeys, PreparedMakeSetArgs,
    },
    types::expr_eval::{EvalInput, EvaluatedAsciiWitness, FrameResult, evaluated_bytes_shape},
};

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
}

/// A private recipe identity, never a consumer-provided function descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EvaluatedKernelKind {
    Wire(tipb::ScalarFuncSig),
    ClosedPrivate(crate::LocalFunctionId),
}

/// Logical admission remains distinct even when transport uses the same Bytes
/// storage. In particular, ordinary Bytes cannot impersonate IEEE754 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EvaluatedArgsRole {
    Values,
    Ieee754Bits,
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
        };
        EvaluatedKernelKind::Wire(signature)
    }

    pub(crate) fn function_ref(self) -> crate::FunctionRef {
        match self.kernel_kind() {
            EvaluatedKernelKind::Wire(signature) => crate::FunctionRef::TiPb(signature),
            EvaluatedKernelKind::ClosedPrivate(id) => crate::FunctionRef::Local(id),
        }
    }

    pub(crate) fn input_role(self) -> EvaluatedArgsRole {
        // A private identity does not determine its carrier or packet policy.
        // In particular, value-only FROM_BASE64 keeps the ordinary Bytes role.
        match self {
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
            | Self::RoundDecimalLegacy => EvaluatedArgsRole::DecimalUnary,
            Self::RoundDecimalNative | Self::TruncateDecimalNative => EvaluatedArgsRole::DecimalInt,
            Self::RoundRealNative | Self::TruncateRealNative => EvaluatedArgsRole::Ieee754Int,
            Self::RoundInt128Legacy => EvaluatedArgsRole::Int128,
            Self::MathNullWitnessNative => EvaluatedArgsRole::NullWitness,
            Self::CharNative => EvaluatedArgsRole::CharReady,
            Self::ConvNative | Self::ConvBinaryLiteralNative => EvaluatedArgsRole::ConvNative,
            Self::ConvLegacy => EvaluatedArgsRole::ConvLegacy,
            Self::AbsRealNative
            | Self::CeilRealNative
            | Self::FloorRealNative
            | Self::RoundRealLegacy => EvaluatedArgsRole::Ieee754Bits,
            Self::StrcmpNative | Self::FindInSetNative => EvaluatedArgsRole::CollatedBytes2,
            Self::Locate2Native | Self::Locate3Native => EvaluatedArgsRole::NativeSearch,
            Self::FindInSetPreparedNative => EvaluatedArgsRole::FindInSetPrepared,
            Self::PiRaw => EvaluatedArgsRole::NoArgs,
            Self::Sha2Native => EvaluatedArgsRole::ReadyBytesInt,
            Self::LogNative | Self::PowNative => EvaluatedArgsRole::Ieee754Bits2,
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
            | Self::Log2Native => EvaluatedArgsRole::Ieee754Bits,
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

    fn returns_ieee754_bits(self) -> bool {
        matches!(
            self,
            Self::AsinRaw
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
        )
    }

    pub(crate) fn fn_meta(self) -> crate::RpnFnMeta {
        // Fixed identity witnesses for common preparation. Only the closed
        // factory also uses the private getters to select a non-wire call;
        // no caller-supplied metadata or alternative algorithm is accepted.
        match self {
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
            | Self::MathNullWitnessNative => EvalType::Int,
            Self::AbsDecimalNative
            | Self::CeilDecimalNative
            | Self::FloorDecimalNative
            | Self::RoundDecimalNative
            | Self::TruncateDecimalNative => EvalType::Decimal,
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
            | Self::ConvLegacy => EvalType::Bytes,
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
            Self::PiRaw => &[],
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
            _ => unreachable!("the operation has only Int/Bytes/Decimal inputs"),
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
/// it is not a claim that either original argument was SQL NULL.
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

/// Owned ready arguments and explicit demand markers for closed recipes. Int
/// carries the original 64-bit pattern: callers may pass a u64 as i64 without
/// numeric narrowing. Coercion, diagnostics, argument demand and text
/// normalization belong to the original frontend. For logical AND(false, _)
/// or OR(true, _), only its validated undemanded-RHS marker may authorize an
/// irrelevant representative; this boundary does not claim RHS evaluated to
/// NULL.
#[derive(Debug)]
pub enum EvaluatedArgs {
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
            Self::Decimal(_) => EvaluatedArgsRole::DecimalUnary,
            Self::DecimalIntReady { .. } => EvaluatedArgsRole::DecimalInt,
            Self::Ieee754BitsInt { .. } => EvaluatedArgsRole::Ieee754Int,
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
            Self::Ieee754Bits(_) => EvaluatedArgsRole::Ieee754Bits,
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
            Self::NoArgs => &[],
            Self::Decimal(_) => &[EvalType::Decimal, EvalType::Int],
            Self::DecimalIntReady { .. } => &[EvalType::Decimal, EvalType::Int, EvalType::Int],
            Self::Ieee754BitsInt { .. } => &[EvalType::Bytes, EvalType::Int],
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
            Self::Bytes(_) | Self::Ieee754Bits(_) => &[EvalType::Bytes],
            Self::Bytes2(..) | Self::Ieee754Bits2 { .. } => &[EvalType::Bytes, EvalType::Bytes],
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
        match self {
            Self::NullWitness(value) => {
                operation == EvaluatedBytesOp::MathNullWitnessNative && value.is_none()
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
                (ReadyIeee754Arg::Undemanded, ReadyIeee754Arg::Value(None))
                | (ReadyIeee754Arg::Value(None), ReadyIeee754Arg::Undemanded) => {
                    operation == EvaluatedBytesOp::PowNative
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

    fn into_values(self, decimal_available: usize) -> LocalResult<([ScalarValue; 4], usize)> {
        // The fixed owner stays inline. Only four PAD, two INSERT and native
        // LOCATE3 recipes publish all slots; unused slots never enter the driver.
        // IEEE754's physical Byte8 allocation is charged like any Bytes owner.
        use ScalarValue::{Bytes, Int};
        Ok(match self {
            Self::NoArgs => ([Int(None), Int(None), Int(None), Int(None)], 0),
            Self::Decimal(value) => {
                let limit =
                    Self::decimal_materialization_budget(value.as_ref(), decimal_available)?;
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
                let limit =
                    Self::decimal_materialization_budget(value.as_ref(), decimal_available)?;
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
            Self::Ieee754Bits2 { left, right } => {
                // Only validated POW + a truly NULL opposite operand permits
                // this irrelevant +0 bit pattern; it is not a fake SQL NULL.
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
        })
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

    fn ieee754_value(value: Option<u64>) -> LocalResult<ScalarValue> {
        let value = value
            .map(|bits| -> LocalResult<Vec<u8>> {
                let mut bytes = Vec::new();
                bytes.try_reserve_exact(8).map_err(|_| {
                    LocalError::ResourceLimit("IEEE754 input allocation failed".into())
                })?;
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
    /// Available only for integral CEIL/FLOOR results that fit exactly in i64.
    /// None leaves the original Decimal intact, including out-of-range results.
    pub fn checked_i64_view(&self) -> Option<i64> {
        self.checked_i64_view
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

/// Only a semantic cause returned by the matching sealed invocation can
/// authorize this view. Neither numeric error codes nor messages classify it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvaluatedSqlFailureKind {
    AbsSignedOverflow,
    ConvUnsignedOverflow,
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
    Int(ComputedInt),
    Bytes(ComputedBytes),
    Ieee754Bits(ComputedIeee754Bits),
    Decimal(ComputedDecimal),
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
/// invocation borrow is cached.
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
/// retained. The admitted official kernels are context-free on ready Int/Bytes;
/// the private UTC context disables warning storage and still checks that no
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
            | ComputedValue::Ieee754Bits(_)
            | ComputedValue::Decimal(_)
            | ComputedValue::Int128(_) => {
                self.inner.poisoned = true;
                Err(LocalError::InvalidBatch(
                    "evaluated ASCII requires an owned canonical Int result".into(),
                ))
            }
        }
    }
}

impl EvaluatedBytesWorker {
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
        // Fresh scalar-only canonical descriptors are heap-free BY CONSTRUCTION,
        // not because their values compare equal or serialize to a small size.
        // There is no parse/clear/reuse/mutable descriptor escape. Unit Any owns
        // no allocation. State and witness contain only inline scalar counters.
        let owned_heap_bytes = evaluated_ascii_owned_heap_bytes(
            self.program.expression.capacity(),
            self.program.schema.capacity(),
            metadata_bytes,
            self.ctx.warnings.warnings.capacity(),
        )?;
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

    /// The same single evaluation with a narrow, owned ABS/CONV failure
    /// receipt. Preparation, resource and output failures remain the
    /// original LocalError.
    pub fn eval_args_reported(
        &mut self,
        args: EvaluatedArgs,
    ) -> Result<ComputedValue, ReportedEvaluatedFailure> {
        // Preserve the semantic tag until after refusal. In particular, even
        // NULL or an eight-byte ordinary Bytes value cannot enter raw math.
        if args.role() != self.operation.input_role()
            || args.input_types() != self.operation.input_types()
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
            let decimal_available = if matches!(
                args.role(),
                EvaluatedArgsRole::DecimalUnary | EvaluatedArgsRole::DecimalInt
            ) {
                // A real retained owner is already present even for inline or
                // NULL Decimal input. Subtract it, not a guessed packet/scale
                // cap; a caller's usize::MAX limit still leaves finite room.
                self.state
                    .limits
                    .max_retained_bytes
                    .checked_sub(self.observe_storage()?.total_bytes())
                    .ok_or_else(evaluated_ascii_storage_overflow)?
            } else {
                0
            };
            let (ready, arity) = args.into_values(decimal_available)?;
            self.eval_ready(ready, arity, &mut sql_failure)
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
        ready: [ScalarValue; 4],
        arity: usize,
        sql_failure: &mut Option<EvaluatedSqlFailureKind>,
    ) -> LocalResult<ComputedValue> {
        let input_bytes = ready[..arity].iter().try_fold(0usize, |total, value| {
            let bytes = match value {
                ScalarValue::Bytes(Some(bytes)) => bytes.capacity(),
                ScalarValue::Decimal(Some(value)) => value.spill_capacity_bytes(),
                _ => 0,
            };
            total
                .checked_add(bytes)
                .ok_or_else(evaluated_ascii_storage_overflow)
        })?;
        self.state.row = [0];
        let mut budget = EvalBudget::exact(self.state.limits)?;
        let calls_before = self.witness.invocations();
        let result = self
            .program
            .expression
            .eval_with_ready_args(
                self.operation,
                &mut self.ctx,
                &self.program.schema,
                &ready[..arity],
                self.operation.input_role(),
                &self.state.row,
                &mut self.witness,
                &mut budget,
            )
            .map_err(|error| {
                // This exact closed recipe has one canonical generated wrapper.
                // Capture only its just-returned typed failure, not a later output
                // or cleanup failure, an input error, or an overflow-looking code.
                if calls_before.checked_add(1) == Some(self.witness.invocations()) {
                    if let LocalError::Evaluation(cause) = &error {
                        *sql_failure = match (self.operation, cause.0.as_ref()) {
                            (
                                EvaluatedBytesOp::AbsIntNative,
                                ErrorInner::Evaluate(EvaluateError::AbsSignedOverflow { .. }),
                            ) => Some(EvaluatedSqlFailureKind::AbsSignedOverflow),
                            (
                                EvaluatedBytesOp::ConvNative
                                | EvaluatedBytesOp::ConvBinaryLiteralNative,
                                ErrorInner::Evaluate(EvaluateError::ConvUnsignedOverflow {
                                    ..
                                }),
                            ) => Some(EvaluatedSqlFailureKind::ConvUnsignedOverflow),
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
                    EvaluatedBytesOp::CeilDecimalNative | EvaluatedBytesOp::FloorDecimalNative
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
                (
                    ComputedValue::Decimal(ComputedDecimal {
                        value,
                        checked_i64_view,
                    }),
                    retained,
                )
            }
            ScalarValueRef::Bytes(value)
                if self.operation == EvaluatedBytesOp::RoundInt128Legacy =>
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
            ScalarValueRef::Bytes(value) => {
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
