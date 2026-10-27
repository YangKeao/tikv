// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use tidb_query_datatype::{
    EvalType, FieldTypeAccessor, codec::data_type::VectorValue, expr::EvalContext,
};
use tipb::FieldType;

use super::{InputRow, LocalError, LocalResult, registry};

static NEXT_CATALOG_KEY: AtomicU64 = AtomicU64::new(1);

/// Publication carrier for actual temporal source results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqlTemporalOutput {
    /// Authenticated native value capability.
    Value,
    /// Existing typed ETString parent.
    Bytes,
    /// Existing typed Date/Datetime parent.
    DateTime,
    /// Existing typed Duration parent.
    Duration,
}
impl SqlTemporalOutput {
    /// Exact SDK carrier published by the temporal source.
    pub fn eval_type(self) -> EvalType {
        match self {
            Self::Value | Self::Bytes => EvalType::Bytes,
            Self::DateTime => EvalType::DateTime,
            Self::Duration => EvalType::Duration,
        }
    }
}

/// Process-local catalog identity, not a digest of its signatures.
/// Cloning a catalog preserves this identity; constructing another does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostCatalogKey(u64);

fn allocate_catalog_key(next: &AtomicU64) -> LocalResult<HostCatalogKey> {
    next.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        value.checked_add(1)
    })
    .map(HostCatalogKey)
    .map_err(|_| LocalError::ResourceLimit("host catalog identity space exhausted".into()))
}

/// A typed host boundary. The legacy row factory admits signed LongLong;
/// the explicit SQL batch factory admits only its closed operation contracts.
/// All FieldType details remain significant in both domains.
#[derive(Clone, Debug)]
pub struct HostSignature {
    pub arg_types: Box<[FieldType]>,
    pub return_type: FieldType,
}

/// Closed SQL batch host operations. Argument evaluation stays in the shared
/// driver; implementations receive only demanded values and immutable metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqlBatchHostOperation {
    Concat,
    ConcatWs,
    RegexpLike {
        case_insensitive: bool,
    },
    CastCharInt {
        len: Option<u32>,
    },
    CastCharNative {
        len: Option<u32>,
    },
    /// One native value argument and one native value result. Kernel target
    /// and producer/consumer provenance are immutable source metadata.
    CastNative,
    /// Row-scoped IN over actual source values; typed cast barriers and
    /// exhaustive/prepared demand are immutable provider metadata.
    InNative,
    /// Metadata-selected linear/binary INTERVAL probes over real source values.
    IntervalNative,
    /// Eager GREATEST/LEAST sources followed by genuine typed value reducers.
    ExtremumNative,
    /// Exact-metadata charset conversion with actual NULL/target source demand.
    CharsetNative(crate::NativeCharsetOperation),
    /// Actual TIMESTAMPDIFF inputs; PB stops at the first actual NULL.
    TimestampDiffNative {
        typed: bool,
        stop_on_null: bool,
    },
    /// Timestamp conversion over original native source capabilities. Both
    /// polymorphic numeric and temporal results retain their actual Datum view.
    TsoNative(crate::NativeTsoOperation),
    /// Actual duration/construction policy with its original fixed arity.
    DurationConstructionNative(crate::NativeDurationConstructionOperation),
    /// Vector SQL over actual native capabilities.
    VectorNative(crate::NativeVectorOperation),
    /// Lazy actual LIKE operands with original collation/cache metadata.
    LikeNative {
        ilike: bool,
    },
    /// Actual grouping id and optional immutable planner metadata.
    GroupingNative,
    /// Actual deterministic crypto operands, with original arity/warning
    /// policy.
    CryptoNative(crate::NativeCryptoOperation),
    /// Actual byte-unary argument; output is an authenticated native value.
    ByteUnaryNative(crate::NativeByteUnaryOperation),
    /// Actual compression input and completed decoder/warning report.
    CompressionNative(crate::NativeCompressionOperation),
    /// Eager original scalar value body over authenticated actual operands.
    EagerValuesNative,
    /// Genuine AST nodes with staged native-value children in this batch frame.
    /// Source-specific demand and metadata stay with the pinned AST provider.
    AstNative,
    /// Genuine fieldless Unistore consumer ports. Full-width typed values and
    /// original errors are authenticated capabilities owned by that source;
    /// no SQL declaration is inferred from this Bytes transport.
    LegacyNative,
    /// Actual UUID/shard/Vitess inputs with the original optional flag arity.
    MiscNative {
        operation: crate::NativeMiscOperation,
        arity: u8,
    },
    /// Eager identity selection over actual values, with the original arity.
    ValueIdentityNative {
        name_const: bool,
    },
    /// Calendar over actual native capabilities.
    CalendarNative {
        /// Original calendar policy.
        operation: crate::NativeCalendarOperation,
        /// Original eager argument count.
        arity: u8,
    },
    /// Actual statement/live clock policy, with original zero/one-arg shape.
    ClockNative {
        operation: crate::NativeClockOperation,
        arity: u8,
    },
    EpochNative {
        from: bool,
        arity: u8,
    },
    /// Ordinary eager EXTRACT or its distinct one-argument composite policy.
    ExtractNative {
        typed: bool,
        composite: bool,
    },
    /// Ordinary date arithmetic over actual eager operands.
    DateArithmeticNative {
        output: SqlTemporalOutput,
    },
    /// Eager typed endpoints followed by statement-owned SafeTS.
    BoundedStalenessNative {
        typed: bool,
    },
    /// Eager actual date-text sources, with explicit result carrier.
    StrToDateNative {
        output: SqlTemporalOutput,
    },
    /// Actual JSON document checksum, with native capability or typed Int
    /// output.
    JsonSumCrc32Native {
        typed: bool,
    },
    /// Native nullable selection over actual value handles.
    CoalesceNative,
    /// Exactly two native nullable arguments.
    IfNullNative,
    /// One native truth predicate and exactly one selected branch.
    IfNative,
    /// Flattened native condition/result pairs with optional ELSE.
    CaseNative,
    /// Nullable logical AND over demanded native values.
    AndNative,
    /// Nullable logical OR over demanded native values.
    OrNative,
    /// Truth normalization that preserves genuine SQL NULL.
    IsTrueWithNullNative,
    UnaryNotNative,
    IsNullNative,
    IsTrueNative,
    IsFalseNative,
    IsNotTrueNative,
    IsNotFalseNative,
    IsUnknownNative,
    IsNotUnknownNative,
    /// Comparison of two demanded native values under immutable source facts.
    CompareNative(crate::ComparisonOp),
    /// Original nullable equality policy over two demanded native values.
    NullSafeEqNative,
    /// Value-level NULLIF with its original literal comparison policy.
    NullIfNative,
    /// Native ABS over the actual demanded value kind.
    AbsNative,
    /// Row demand over raw native operands, followed by shared bit computation.
    BitwiseNative(crate::impl_op::NativeBitwiseOp),
    /// Both raw operands precede warning probes and nullable truth conversion.
    XorNative,
    /// Raw unary sign with immutable constant/unsigned operand facts.
    UnarySignNative(crate::impl_op::NativeUnarySign),
    /// Native additive arithmetic with signature-specific argument demand.
    AddNative,
    SubtractNative,
    MultiplyNative,
    /// Native quotient families retain their own SQL operation identities.
    ModuloNative,
    DivideNative,
    IntegerDivideNative,
    /// Arithmetic with complete operand-window demand.
    AddWindowNative,
    SubtractWindowNative,
    MultiplyWindowNative,
    ModuloWindowNative,
    DivideWindowNative,
    IntegerDivideWindowNative,
    /// Whole-child integer ABS, independent of row ABS value dispatch.
    AbsIntWindowNative,
    /// Whole-child native Decimal and JSON casts.
    CastDecimalWindowNative,
    CastJsonWindowNative,
    /// Whole-source phases followed by fresh single-occurrence diagnostics.
    /// SQL kernel and original argument roles remain caller-owned metadata.
    EagerValuesWindowNative,
}

impl SqlBatchHostOperation {
    /// Only explicit window operations use the vector callback protocol.
    pub fn is_windowed(self) -> bool {
        matches!(
            self,
            Self::AddWindowNative
                | Self::SubtractWindowNative
                | Self::MultiplyWindowNative
                | Self::ModuloWindowNative
                | Self::DivideWindowNative
                | Self::IntegerDivideWindowNative
                | Self::AbsIntWindowNative
                | Self::CastDecimalWindowNative
                | Self::CastJsonWindowNative
                | Self::EagerValuesWindowNative
        )
    }
    pub fn accepts_arity(self, arity: usize) -> bool {
        match self {
            Self::CalendarNative {
                operation,
                arity: expected,
            } => arity == usize::from(expected) && operation.accepts_arity(arity),
            Self::ClockNative {
                operation,
                arity: expected,
            } => arity == usize::from(expected) && operation.accepts_arity(arity),
            Self::TsoNative(op) => arity == op.arity(),
            Self::DurationConstructionNative(op) => arity == op.arity(),
            Self::VectorNative(op) => arity == op.arity(),
            Self::LikeNative { .. } => arity == 3,
            Self::CryptoNative(op) => arity == op.arity(),
            Self::EagerValuesNative | Self::AstNative | Self::LegacyNative => true,
            Self::EagerValuesWindowNative => arity >= 1,
            Self::GroupingNative | Self::ByteUnaryNative(_) | Self::CompressionNative(_) => {
                arity == 1
            }
            Self::MiscNative {
                operation,
                arity: expected,
            } => arity == usize::from(expected) && operation.accepts_arity(arity),
            Self::ValueIdentityNative { name_const } => arity == 1 + usize::from(name_const),
            Self::EpochNative {
                from,
                arity: expected,
            } => {
                arity == usize::from(expected)
                    && if from {
                        (1..=2).contains(&arity)
                    } else {
                        arity <= 1
                    }
            }
            Self::ExtractNative { composite, .. } => arity == if composite { 1 } else { 2 },
            Self::TimestampDiffNative { .. } => arity == 3,
            Self::StrToDateNative { .. }
            | Self::BoundedStalenessNative { .. }
            | Self::DateArithmeticNative { .. } => arity == 2,
            Self::JsonSumCrc32Native { .. } => arity == 1,
            Self::BitwiseNative(op) => arity == op.arity(),
            Self::CharsetNative(op) => arity == op.arity(),
            Self::Concat | Self::CoalesceNative | Self::CaseNative | Self::ExtremumNative => {
                arity >= 1
            }
            Self::ConcatWs | Self::InNative | Self::IntervalNative => arity >= 2,
            Self::IfNullNative
            | Self::AndNative
            | Self::XorNative
            | Self::OrNative
            | Self::CompareNative(_)
            | Self::NullSafeEqNative
            | Self::NullIfNative
            | Self::AddNative
            | Self::SubtractNative
            | Self::MultiplyNative
            | Self::ModuloNative
            | Self::DivideNative
            | Self::IntegerDivideNative
            | Self::AddWindowNative
            | Self::SubtractWindowNative
            | Self::MultiplyWindowNative
            | Self::ModuloWindowNative
            | Self::DivideWindowNative
            | Self::IntegerDivideWindowNative => arity == 2,
            Self::IfNative => arity == 3,
            Self::IsTrueWithNullNative
            | Self::UnaryNotNative
            | Self::IsNullNative
            | Self::IsTrueNative
            | Self::IsFalseNative
            | Self::IsNotTrueNative
            | Self::IsNotFalseNative
            | Self::IsUnknownNative
            | Self::IsNotUnknownNative
            | Self::AbsNative
            | Self::UnarySignNative(_)
            | Self::AbsIntWindowNative
            | Self::CastDecimalWindowNative
            | Self::CastJsonWindowNative => arity == 1,
            Self::RegexpLike { .. } => matches!(arity, 2 | 3),
            Self::CastCharInt { .. } | Self::CastCharNative { .. } | Self::CastNative => arity == 1,
        }
    }
    pub fn argument_type(self) -> EvalType {
        match self {
            Self::CastCharInt { .. } => EvalType::Int,
            Self::CalendarNative { .. }
            | Self::ClockNative { .. }
            | Self::DurationConstructionNative(_)
            | Self::VectorNative(_)
            | Self::GroupingNative
            | Self::CryptoNative(_)
            | Self::EagerValuesNative
            | Self::AstNative
            | Self::LegacyNative
            | Self::EagerValuesWindowNative
            | Self::CompressionNative(_)
            | Self::ByteUnaryNative(_)
            | Self::MiscNative { .. }
            | Self::ValueIdentityNative { .. }
            | Self::LikeNative { .. }
            | Self::TsoNative(_)
            | Self::EpochNative { .. }
            | Self::ExtractNative { .. }
            | Self::TimestampDiffNative { .. }
            | Self::DateArithmeticNative { .. }
            | Self::BoundedStalenessNative { .. }
            | Self::StrToDateNative { .. }
            | Self::JsonSumCrc32Native { .. }
            | Self::Concat
            | Self::ConcatWs
            | Self::RegexpLike { .. }
            | Self::CastCharNative { .. }
            | Self::CastNative
            | Self::InNative
            | Self::IntervalNative
            | Self::ExtremumNative
            | Self::CharsetNative(_)
            | Self::CoalesceNative
            | Self::IfNullNative
            | Self::IfNative
            | Self::CaseNative
            | Self::AndNative
            | Self::XorNative
            | Self::OrNative
            | Self::IsTrueWithNullNative
            | Self::UnaryNotNative
            | Self::IsNullNative
            | Self::IsTrueNative
            | Self::IsFalseNative
            | Self::IsNotTrueNative
            | Self::IsNotFalseNative
            | Self::IsUnknownNative
            | Self::IsNotUnknownNative
            | Self::CompareNative(_)
            | Self::NullSafeEqNative
            | Self::NullIfNative
            | Self::AbsNative
            | Self::UnarySignNative(_)
            | Self::BitwiseNative(_)
            | Self::AddNative
            | Self::SubtractNative
            | Self::MultiplyNative
            | Self::ModuloNative
            | Self::DivideNative
            | Self::IntegerDivideNative
            | Self::AddWindowNative
            | Self::SubtractWindowNative
            | Self::MultiplyWindowNative
            | Self::ModuloWindowNative
            | Self::DivideWindowNative
            | Self::IntegerDivideWindowNative
            | Self::AbsIntWindowNative
            | Self::CastDecimalWindowNative
            | Self::CastJsonWindowNative => EvalType::Bytes,
        }
    }
    pub fn output_type(self) -> EvalType {
        match self {
            Self::StrToDateNative { output } | Self::DateArithmeticNative { output } => {
                output.eval_type()
            }
            Self::BoundedStalenessNative { typed: true } => EvalType::DateTime,
            Self::CalendarNative { .. }
            | Self::ClockNative { .. }
            | Self::DurationConstructionNative(_)
            | Self::VectorNative(_)
            | Self::GroupingNative
            | Self::CryptoNative(_)
            | Self::EagerValuesNative
            | Self::AstNative
            | Self::LegacyNative
            | Self::EagerValuesWindowNative
            | Self::CompressionNative(_)
            | Self::ByteUnaryNative(_)
            | Self::MiscNative { .. }
            | Self::ValueIdentityNative { .. }
            | Self::LikeNative { .. }
            | Self::TsoNative(_)
            | Self::EpochNative { .. } => EvalType::Bytes,
            Self::ExtractNative { typed: true, .. } => EvalType::Int,
            Self::ExtractNative { typed: false, .. } => EvalType::Bytes,
            Self::TimestampDiffNative { typed: true, .. }
            | Self::JsonSumCrc32Native { typed: true } => EvalType::Int,
            Self::TimestampDiffNative { typed: false, .. }
            | Self::BoundedStalenessNative { typed: false }
            | Self::JsonSumCrc32Native { typed: false }
            | Self::Concat
            | Self::ConcatWs
            | Self::CastCharInt { .. }
            | Self::CastCharNative { .. }
            | Self::CastNative
            | Self::InNative
            | Self::IntervalNative
            | Self::ExtremumNative
            | Self::CharsetNative(_)
            | Self::CoalesceNative
            | Self::IfNullNative
            | Self::IfNative
            | Self::CaseNative
            | Self::AndNative
            | Self::XorNative
            | Self::OrNative
            | Self::IsTrueWithNullNative
            | Self::UnaryNotNative
            | Self::IsNullNative
            | Self::IsTrueNative
            | Self::IsFalseNative
            | Self::IsNotTrueNative
            | Self::IsNotFalseNative
            | Self::IsUnknownNative
            | Self::IsNotUnknownNative
            | Self::CompareNative(_)
            | Self::NullSafeEqNative
            | Self::NullIfNative
            | Self::AbsNative
            | Self::UnarySignNative(_)
            | Self::BitwiseNative(_)
            | Self::AddNative
            | Self::SubtractNative
            | Self::MultiplyNative
            | Self::ModuloNative
            | Self::DivideNative
            | Self::IntegerDivideNative
            | Self::AddWindowNative
            | Self::SubtractWindowNative
            | Self::MultiplyWindowNative
            | Self::ModuloWindowNative
            | Self::DivideWindowNative
            | Self::IntegerDivideWindowNative
            | Self::AbsIntWindowNative
            | Self::CastDecimalWindowNative
            | Self::CastJsonWindowNative => EvalType::Bytes,
            Self::RegexpLike { .. } => EvalType::Int,
        }
    }
}

/// Immutable signatures shared by compiled calls. There are no callbacks or
/// native expression objects in this catalog.
#[derive(Clone, Debug)]
pub struct HostCatalog {
    key: HostCatalogKey,
    signatures: Arc<[HostSignature]>,
    sql_batch_operations: Option<Arc<[SqlBatchHostOperation]>>,
}

impl HostCatalog {
    pub fn new(signatures: Vec<HostSignature>) -> LocalResult<Self> {
        for (index, signature) in signatures.iter().enumerate() {
            for (argument, field_type) in signature.arg_types.iter().enumerate() {
                registry::check_signed_int_type(field_type).map_err(|error| {
                    LocalError::InvalidSpec(format!(
                        "host slot {} argument {}: {}",
                        index, argument, error
                    ))
                })?;
            }
            registry::check_signed_int_type(&signature.return_type).map_err(|error| {
                LocalError::InvalidSpec(format!("host slot {} result: {}", index, error))
            })?;
        }
        Ok(Self {
            key: allocate_catalog_key(&NEXT_CATALOG_KEY)?,
            signatures: signatures.into(),
            sql_batch_operations: None,
        })
    }

    /// A separate checked domain; this does not widen the legacy row factory.
    pub fn for_sql_batch(
        entries: Vec<(SqlBatchHostOperation, HostSignature)>,
    ) -> LocalResult<Self> {
        let mut operations = Vec::new();
        let mut signatures = Vec::new();
        operations
            .try_reserve(entries.len())
            .map_err(|_| LocalError::ResourceLimit("SQL host catalog allocation failed".into()))?;
        signatures.try_reserve(entries.len()).map_err(|_| {
            LocalError::ResourceLimit("SQL host signature allocation failed".into())
        })?;
        for (operation, signature) in entries {
            if !operation.accepts_arity(signature.arg_types.len()) {
                return Err(LocalError::InvalidSpec(
                    "SQL host operation arity differs from its contract".into(),
                ));
            }
            for field in signature.arg_types.iter() {
                super::profile::check_batch_type(field)?;
                if EvalType::try_from(field.as_accessor().tp()).ok()
                    != Some(operation.argument_type())
                {
                    return Err(LocalError::InvalidSpec(
                        "SQL batch host argument differs from its operation".into(),
                    ));
                }
            }
            super::profile::check_batch_type(&signature.return_type)?;
            if EvalType::try_from(signature.return_type.as_accessor().tp()).ok()
                != Some(operation.output_type())
            {
                return Err(LocalError::InvalidSpec(
                    "SQL host result differs from its operation".into(),
                ));
            }
            if operation.output_type() == EvalType::Int {
                registry::check_signed_int_type(&signature.return_type)
                    .map_err(|error| LocalError::InvalidSpec(error.to_string()))?;
            }
            operations.push(operation);
            signatures.push(signature);
        }
        Ok(Self {
            key: allocate_catalog_key(&NEXT_CATALOG_KEY)?,
            signatures: signatures.into(),
            sql_batch_operations: Some(operations.into()),
        })
    }

    pub fn sql_batch_operation(&self, slot: HostSlot) -> LocalResult<SqlBatchHostOperation> {
        self.signature(slot)?;
        self.sql_batch_operations
            .as_ref()
            .and_then(|ops| ops.get(slot.index))
            .copied()
            .ok_or_else(|| {
                LocalError::InvalidSpec("host slot is outside the SQL batch domain".into())
            })
    }

    pub(crate) fn is_sql_batch(&self) -> bool {
        self.sql_batch_operations.is_some()
    }

    pub fn signature(&self, slot: HostSlot) -> LocalResult<&HostSignature> {
        if slot.key != self.key {
            return Err(LocalError::InvalidSpec(
                "host slot belongs to another catalog".into(),
            ));
        }
        self.signatures
            .get(slot.index)
            .ok_or_else(|| LocalError::InvalidSpec("host slot is outside its catalog".into()))
    }

    pub fn key(&self) -> &HostCatalogKey {
        &self.key
    }

    /// Resolves a catalog-owned slot without allowing callers to forge one.
    pub fn slot(&self, index: usize) -> LocalResult<HostSlot> {
        if index >= self.signatures.len() {
            return Err(LocalError::InvalidSpec(format!(
                "host slot {} is outside its catalog",
                index
            )));
        }
        Ok(HostSlot {
            key: self.key,
            index,
        })
    }
}

/// Opaque catalog-bound identity. The index alone is only an adapter dispatch
/// coordinate and must not be used to establish catalog compatibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostSlot {
    key: HostCatalogKey,
    index: usize,
}

impl HostSlot {
    pub fn index(self) -> usize {
        self.index
    }
}

/// A checked call retaining its immutable catalog, with no caller-supplied
/// metadata or native evaluator. Only local compilation can prepare this value.
#[derive(Clone, Debug)]
pub struct PreparedHostCall {
    catalog: HostCatalog,
    slot: HostSlot,
}

impl PreparedHostCall {
    pub(crate) fn prepare(
        catalog: &HostCatalog,
        slot: HostSlot,
        arg_types: &[FieldType],
        return_type: &FieldType,
    ) -> LocalResult<Self> {
        if slot.key != catalog.key {
            return Err(LocalError::InvalidSpec(
                "host slot belongs to a different catalog".into(),
            ));
        }
        let signature = catalog
            .signatures
            .get(slot.index)
            .ok_or_else(|| LocalError::InvalidSpec("host slot is outside its catalog".into()))?;
        if signature.arg_types.as_ref() != arg_types {
            return Err(LocalError::InvalidSpec(
                "host argument field types differ from its catalog signature".into(),
            ));
        }
        if &signature.return_type != return_type {
            return Err(LocalError::InvalidSpec(
                "host result field type differs from its catalog signature".into(),
            ));
        }
        Ok(Self {
            catalog: catalog.clone(),
            slot,
        })
    }

    pub(crate) fn is_sql_batch(&self) -> bool {
        self.catalog.is_sql_batch()
    }

    pub(crate) fn is_windowed(&self) -> bool {
        self.catalog
            .sql_batch_operations
            .as_ref()
            .is_some_and(|operations| operations[self.slot.index].is_windowed())
    }
    pub fn slot(&self) -> HostSlot {
        self.slot
    }

    pub fn catalog_key(&self) -> &HostCatalogKey {
        self.catalog.key()
    }

    pub fn arg_types(&self) -> &[FieldType] {
        &self.catalog.signatures[self.slot.index].arg_types
    }

    pub fn return_type(&self) -> &FieldType {
        &self.catalog.signatures[self.slot.index].return_type
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgMode {
    /// Evaluate the requested argument for this occurrence again.
    Fresh,
    /// Fresh row demand whose failure is delivered to `resume_error` after
    /// descendant tasks are cancelled. No previous success is reused and no
    /// error becomes a SQL NULL. Global execution budgets remain in force.
    FreshResult,
    /// Reuse the last successful value for this argument and invocation; a
    /// cache miss evaluates the child once and retains that successful result.
    Reuse,
}

/// Borrows are valid only during `start`; tasks retain owned state and IDs, not
/// the caller's program, FieldTypes, or EvalContext.
#[derive(Clone, Copy, Debug)]
pub struct HostInvocation<'a> {
    pub slot: HostSlot,
    pub row: InputRow,
    pub arg_types: &'a [FieldType],
    pub return_type: &'a FieldType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostArgRequest {
    pub index: usize,
    pub mode: ArgMode,
}

/// A checked, width-one argument value borrowed only for the `resume` call.
#[derive(Clone, Copy, Debug)]
pub struct HostArgReply<'a> {
    pub index: usize,
    pub field_type: &'a FieldType,
    pub values: &'a VectorValue,
}

/// One actual demanded child failure, moved by the official driver only.
/// The receipt owns its cause, never a borrowed reply, synthesized SQL error,
/// or a cached successful argument. It is intentionally neither Clone nor Copy.
#[derive(Debug)]
pub struct HostArgFailure {
    index: usize,
    error: LocalError,
}
impl HostArgFailure {
    pub(crate) fn new(index: usize, error: LocalError) -> Self {
        Self { index, error }
    }
    /// The actual argument requested by the suspended parent host.
    pub fn index(&self) -> usize {
        self.index
    }
    /// Move the original owned cause without cloning or translating it.
    pub fn into_error(self) -> LocalError {
        self.error
    }
    pub(crate) fn retained_bytes(&self) -> usize {
        match &self.error {
            LocalError::InvalidSpec(s)
            | LocalError::InvalidBatch(s)
            | LocalError::BindingContract(s)
            | LocalError::HostContract(s)
            | LocalError::ResourceLimit(s) => s.capacity(),
            // The foreign error payload is opaque and externally allocated,
            // like existing runtime/adapter causes; this is no total-heap cap.
            LocalError::Evaluation(_) => 0,
        }
    }
}

/// The caller's complete checked occurrence window. Physical rows may repeat;
/// each position has its own occurrence identity. Borrowed only during start.
#[derive(Clone, Copy, Debug)]
pub struct HostWindowInvocation<'a> {
    pub slot: HostSlot,
    /// Original occurrence index of position zero, including a scoped row.
    pub occurrence_start: usize,
    pub physical_rows: &'a [usize],
    pub arg_types: &'a [FieldType],
    pub return_type: &'a FieldType,
}

/// One complete argument phase, in the unchanged selection order. The vector
/// is owned by the driver and borrowed only for this resume call.
#[derive(Clone, Copy, Debug)]
pub struct HostWindowArgReply<'a> {
    pub index: usize,
    /// Same occurrence origin as this invocation's start callback.
    pub occurrence_start: usize,
    pub field_type: &'a FieldType,
    pub physical_rows: &'a [usize],
    pub values: &'a VectorValue,
}

/// Adapter task identity. Generations distinguish reuse of a task slot and must
/// never be zero. A driver receiving generation zero must reject it and attempt
/// cancellation, rather than treating it as a ready or absent task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostTaskId {
    pub slot: usize,
    pub generation: u64,
}

impl HostTaskId {
    /// Validation does not perform cleanup; the driver must still best-effort
    /// cancel a malformed ID returned by a service.
    pub(crate) fn validate_generation(&self) -> LocalResult<()> {
        if self.generation == 0 {
            return Err(LocalError::HostContract(
                "host task generation must be nonzero".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum HostStart {
    Ready(VectorValue),
    Pending {
        task: HostTaskId,
        request: HostArgRequest,
    },
}

#[derive(Debug)]
pub enum HostStep {
    NeedArg(HostArgRequest),
    /// Freshly evaluate one original occurrence of an argument of a window
    /// host. This neither consumes nor replaces its whole-vector cache.
    NeedArgRow {
        index: usize,
        position: usize,
    },
    Ready(VectorValue),
}

/// Demand protocol for catalog-bound hosts, not native Expr evaluation or an
/// arbitrary evaluation callback. Argument expressions are evaluated only by
/// the official driver in response to explicit requests.
///
/// Implementations must keep their catalog identity and task namespace stable
/// across service reborrows. Invocation/reply references and EvalContext may
/// not be retained across calls; suspended tasks contain only owned state.
/// `start`/`start_window` release partial state before Err or immediate Ready.
/// `resume`/`resume_window` finish tasks before Ready; the driver retains the
/// token until that result is validated and may still issue idempotent
/// cancellation. Start/resume bodies must be bounded, and adapters must meter
/// their own opaque task allocations: RPN limits are not a total host-heap or
/// external-stop hook. Cleanup guarantees require a stable, contract-compliant
/// provider; they cannot recover resources after the provider disappears,
/// changes identity or panics.
pub trait LocalHostServices {
    fn catalog_key(&self) -> &HostCatalogKey;

    fn start(
        &mut self,
        ctx: &mut EvalContext,
        invocation: HostInvocation<'_>,
    ) -> LocalResult<HostStart>;

    fn resume(
        &mut self,
        ctx: &mut EvalContext,
        task: &HostTaskId,
        reply: HostArgReply<'_>,
    ) -> LocalResult<HostStep>;

    /// Owned error reply for an explicit FreshResult row request. Descendant
    /// tasks are already cancelled. Default providers retain immediate failure.
    /// Do not retain invocation/reply references or reset execution budgets.
    fn resume_error(
        &mut self,
        _ctx: &mut EvalContext,
        _task: &HostTaskId,
        failure: HostArgFailure,
    ) -> LocalResult<HostStep> {
        Err(failure.into_error())
    }

    /// Whole-window counterpart of start, only for registered window
    /// operations. An unimplemented protocol is a contract error, never a
    /// row-loop fallback.
    fn start_window(
        &mut self,
        _ctx: &mut EvalContext,
        _invocation: HostWindowInvocation<'_>,
    ) -> LocalResult<HostStart> {
        Err(LocalError::HostContract(
            "host does not implement window start".into(),
        ))
    }

    /// A request evaluates the complete child window before this callback.
    /// Fresh/Reuse applies to that entire successful vector and invocation.
    fn resume_window(
        &mut self,
        _ctx: &mut EvalContext,
        _task: &HostTaskId,
        _reply: HostWindowArgReply<'_>,
    ) -> LocalResult<HostStep> {
        Err(LocalError::HostContract(
            "host does not implement window resume".into(),
        ))
    }

    /// Infallible, nonpanicking, idempotent cleanup, including malformed,
    /// stale, or already completed IDs. Do not evaluate expressions, append
    /// diagnostics, or replace the error that caused cancellation. There is
    /// no default: every host implementation must explicitly provide its
    /// cleanup contract.
    fn cancel(&mut self, task: &HostTaskId);
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tidb_query_datatype::{FieldTypeAccessor, FieldTypeFlag, FieldTypeTp};

    use super::*;

    fn int_type() -> FieldType {
        FieldTypeTp::LongLong.into()
    }

    fn signature() -> HostSignature {
        HostSignature {
            arg_types: vec![int_type()].into_boxed_slice(),
            return_type: int_type(),
        }
    }

    #[test]
    fn catalog_identity_is_not_signature_identity() {
        let catalog = HostCatalog::new(vec![signature()]).unwrap();
        let cloned = catalog.clone();
        let separate = HostCatalog::new(vec![signature()]).unwrap();
        assert_eq!(catalog.key(), cloned.key());
        assert!(Arc::ptr_eq(&catalog.signatures, &cloned.signatures));
        assert_ne!(catalog.key(), separate.key());
        assert_eq!(catalog.slot(0).unwrap(), cloned.slot(0).unwrap());
        assert_ne!(catalog.slot(0).unwrap(), separate.slot(0).unwrap());
        let keys: HashSet<_> = [*catalog.key(), *cloned.key(), *separate.key()]
            .into_iter()
            .collect();
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn catalog_keys_are_unique_across_threads() {
        let workers: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| *HostCatalog::new(vec![signature()]).unwrap().key()))
            .collect();
        let keys: HashSet<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(keys.len(), 8);
    }

    #[test]
    fn catalog_key_exhaustion_does_not_wrap() {
        let next = AtomicU64::new(u64::MAX - 1);
        assert_eq!(
            allocate_catalog_key(&next).unwrap(),
            HostCatalogKey(u64::MAX - 1)
        );
        assert!(matches!(
            allocate_catalog_key(&next),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(next.load(Ordering::Relaxed), u64::MAX);
        assert!(allocate_catalog_key(&next).is_err());
    }

    #[test]
    fn catalog_rejects_non_signed_longlong_signatures() {
        let mut unsigned = int_type();
        unsigned.as_mut_accessor().set_flag(FieldTypeFlag::UNSIGNED);
        for invalid in [
            FieldTypeTp::Double.into(),
            FieldTypeTp::String.into(),
            unsigned,
        ] {
            let bad_argument = HostSignature {
                arg_types: vec![invalid.clone()].into_boxed_slice(),
                return_type: int_type(),
            };
            assert!(matches!(
                HostCatalog::new(vec![bad_argument]),
                Err(LocalError::InvalidSpec(_))
            ));
            let bad_result = HostSignature {
                arg_types: Box::new([]),
                return_type: invalid,
            };
            assert!(matches!(
                HostCatalog::new(vec![bad_result]),
                Err(LocalError::InvalidSpec(_))
            ));
        }
    }

    #[test]
    fn catalog_slot_bounds_are_checked() {
        assert!(HostCatalog::new(vec![]).unwrap().slot(0).is_err());
        let catalog = HostCatalog::new(vec![signature()]).unwrap();
        assert_eq!(catalog.slot(0).unwrap().index(), 0);
        assert!(catalog.slot(1).is_err());
        assert!(catalog.slot(usize::MAX).is_err());
    }

    #[test]
    fn preparation_checks_catalog_and_complete_signature() {
        let catalog = HostCatalog::new(vec![signature()]).unwrap();
        let separate = HostCatalog::new(vec![signature()]).unwrap();
        let slot = catalog.slot(0).unwrap();
        let prepared =
            PreparedHostCall::prepare(&catalog, slot, &[int_type()], &int_type()).unwrap();
        assert_eq!(prepared.slot(), slot);
        assert_eq!(prepared.catalog_key(), catalog.key());
        assert_eq!(prepared.arg_types(), &[int_type()]);
        assert_eq!(prepared.return_type(), &int_type());
        assert!(
            PreparedHostCall::prepare(&catalog.clone(), slot, &[int_type()], &int_type()).is_ok()
        );
        assert!(PreparedHostCall::prepare(&separate, slot, &[int_type()], &int_type()).is_err());
        assert!(PreparedHostCall::prepare(&catalog, slot, &[], &int_type()).is_err());
        let mut different = int_type();
        different.set_flen(7);
        assert!(
            PreparedHostCall::prepare(&catalog, slot, &[different.clone()], &int_type()).is_err()
        );
        assert!(PreparedHostCall::prepare(&catalog, slot, &[int_type()], &different).is_err());
        let invalid_slot = HostSlot {
            key: *catalog.key(),
            index: usize::MAX,
        };
        assert!(
            PreparedHostCall::prepare(&catalog, invalid_slot, &[int_type()], &int_type()).is_err()
        );
    }

    #[test]
    fn prepared_call_keeps_its_catalog_alive() {
        let prepared = {
            let catalog = HostCatalog::new(vec![signature()]).unwrap();
            PreparedHostCall::prepare(
                &catalog,
                catalog.slot(0).unwrap(),
                &[int_type()],
                &int_type(),
            )
            .unwrap()
        };
        assert_eq!(prepared.arg_types(), &[int_type()]);
        assert_eq!(prepared.return_type(), &int_type());
    }

    #[test]
    fn existing_input_services_need_no_host_implementation() {
        struct Inputs;
        impl super::super::LocalRuntimeServices for Inputs {
            fn binding_schema(&self) -> &[FieldType] {
                &[]
            }
            fn read_input(
                &mut self,
                _ctx: &mut EvalContext,
                _slot: usize,
                _row: InputRow,
                _expected: &FieldType,
            ) -> LocalResult<VectorValue> {
                Err(LocalError::InvalidBatch("no input slots".into()))
            }
        }
        let mut inputs = Inputs;
        assert!(super::super::LocalRuntimeServices::host_services(&mut inputs).is_none());
    }

    #[test]
    fn task_budget_reserves_before_start() {
        use super::super::runtime::{EvalBudget, ExecutionLimits};
        let budget = EvalBudget::local(
            ExecutionLimits {
                max_active_tasks: 1,
                ..ExecutionLimits::default()
            },
            0,
        )
        .unwrap();
        assert!(budget.task_count(0).is_ok());
        assert!(budget.task_count(1).is_ok());
        assert!(matches!(
            budget.task_count(2),
            Err(LocalError::ResourceLimit(_))
        ));
        assert!(EvalBudget::legacy().task_count(usize::MAX).is_ok());
    }

    #[test]
    fn task_generation_zero_is_a_contract_error() {
        let invalid = HostTaskId {
            slot: 0,
            generation: 0,
        };
        assert!(matches!(
            invalid.validate_generation(),
            Err(LocalError::HostContract(_))
        ));
        let valid = HostTaskId {
            slot: 0,
            generation: 1,
        };
        assert!(valid.validate_generation().is_ok());
        let maximum = HostTaskId {
            slot: 0,
            generation: u64::MAX,
        };
        assert!(maximum.validate_generation().is_ok());
        let identities: HashSet<_> = [invalid, valid, maximum].into_iter().collect();
        assert_eq!(identities.len(), 3);
    }
}

#[cfg(test)]
mod sql_batch_tests {
    use tidb_query_datatype::FieldTypeTp;

    use super::*;
    #[test]
    fn sql_context_operation_factory_is_closed_and_does_not_grant_row_calls() {
        let bytes: FieldType = FieldTypeTp::VarString.into();
        let signature = |arity| HostSignature {
            arg_types: vec![bytes.clone(); arity].into_boxed_slice(),
            return_type: bytes.clone(),
        };
        assert!(
            HostCatalog::for_sql_batch(vec![(SqlBatchHostOperation::Concat, signature(0))])
                .is_err()
        );
        assert!(
            HostCatalog::for_sql_batch(vec![(SqlBatchHostOperation::ConcatWs, signature(1))])
                .is_err()
        );
        let mut wrong = signature(2);
        wrong.arg_types[1] = FieldTypeTp::LongLong.into();
        assert!(HostCatalog::for_sql_batch(vec![(SqlBatchHostOperation::Concat, wrong)]).is_err());
        assert!(HostCatalog::new(vec![signature(2)]).is_err());
        let catalog =
            HostCatalog::for_sql_batch(vec![(SqlBatchHostOperation::Concat, signature(2))])
                .unwrap();
        let foreign =
            HostCatalog::for_sql_batch(vec![(SqlBatchHostOperation::Concat, signature(2))])
                .unwrap();
        assert_eq!(
            catalog
                .sql_batch_operation(catalog.slot(0).unwrap())
                .unwrap(),
            SqlBatchHostOperation::Concat
        );
        assert!(
            catalog
                .sql_batch_operation(foreign.slot(0).unwrap())
                .is_err()
        );
        assert!(
            PreparedHostCall::prepare(
                &catalog,
                catalog.slot(0).unwrap(),
                &[bytes.clone(), FieldTypeTp::Blob.into()],
                &bytes
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod regexp_batch_tests {
    use tidb_query_datatype::FieldTypeTp;

    use super::*;
    #[test]
    fn regexp_catalog_requires_exact_arity_bytes_and_signed_int_result() {
        let operation = SqlBatchHostOperation::RegexpLike {
            case_insensitive: true,
        };
        for arity in [0, 1, 2, 3, 4] {
            let signature = HostSignature {
                arg_types: vec![FieldTypeTp::VarString.into(); arity].into_boxed_slice(),
                return_type: FieldTypeTp::LongLong.into(),
            };
            assert_eq!(
                HostCatalog::for_sql_batch(vec![(operation, signature)]).is_ok(),
                matches!(arity, 2 | 3)
            );
        }
        let signature = |result| HostSignature {
            arg_types: vec![FieldTypeTp::VarString.into(); 2].into_boxed_slice(),
            return_type: result,
        };
        assert!(
            HostCatalog::for_sql_batch(vec![(operation, signature(FieldTypeTp::VarString.into()))])
                .is_err()
        );
        assert!(
            HostCatalog::for_sql_batch(vec![(operation, signature(FieldTypeTp::Tiny.into()))])
                .is_err()
        );
        let mut unsigned: FieldType = FieldTypeTp::LongLong.into();
        unsigned
            .as_mut_accessor()
            .set_flag(tidb_query_datatype::FieldTypeFlag::UNSIGNED);
        assert!(HostCatalog::for_sql_batch(vec![(operation, signature(unsigned))]).is_err());
    }
}

#[cfg(test)]
mod integer_char_cast_tests {
    use tidb_query_datatype::{FieldTypeFlag, FieldTypeTp};

    use super::*;
    #[test]
    fn integer_char_cast_catalog_retains_input_signedness_and_has_one_argument() {
        let operation = SqlBatchHostOperation::CastCharInt { len: Some(1) };
        let mut integer: FieldType = FieldTypeTp::LongLong.into();
        integer.as_mut_accessor().set_flag(FieldTypeFlag::UNSIGNED);
        let signature = HostSignature {
            arg_types: vec![integer.clone()].into_boxed_slice(),
            return_type: FieldTypeTp::VarString.into(),
        };
        let catalog = HostCatalog::for_sql_batch(vec![(operation, signature)]).unwrap();
        assert_eq!(
            catalog
                .signature(catalog.slot(0).unwrap())
                .unwrap()
                .arg_types[0],
            integer
        );
        assert_eq!(
            super::super::BatchCallSignature::for_host(operation, 1)
                .unwrap()
                .argument_type(0),
            Some(EvalType::Int)
        );
        assert!(super::super::BatchCallSignature::for_host(operation, 2).is_none());
        let wrong = HostSignature {
            arg_types: vec![FieldTypeTp::VarString.into()].into_boxed_slice(),
            return_type: FieldTypeTp::VarString.into(),
        };
        assert!(HostCatalog::for_sql_batch(vec![(operation, wrong)]).is_err());
    }
}

#[cfg(test)]
mod ast_source_tests {
    use tidb_query_datatype::FieldTypeTp;

    use super::*;

    #[test]
    fn legacy_catalog_keeps_fieldless_outcomes_in_its_native_capability_domain() {
        let bytes: FieldType = FieldTypeTp::Blob.into();
        for arity in [0, 1, 2, 17] {
            let signature = HostSignature {
                arg_types: vec![bytes.clone(); arity].into_boxed_slice(),
                return_type: bytes.clone(),
            };
            let catalog = HostCatalog::for_sql_batch(vec![(
                SqlBatchHostOperation::LegacyNative,
                signature.clone(),
            )])
            .unwrap();
            let slot = catalog.slot(0).unwrap();
            assert_eq!(
                catalog.sql_batch_operation(slot).unwrap(),
                SqlBatchHostOperation::LegacyNative
            );
            assert!(!SqlBatchHostOperation::LegacyNative.is_windowed());
            assert!(
                PreparedHostCall::prepare(&catalog, slot, &signature.arg_types, &bytes).is_ok()
            );
            let foreign =
                HostCatalog::for_sql_batch(vec![(SqlBatchHostOperation::LegacyNative, signature)])
                    .unwrap();
            assert!(
                PreparedHostCall::prepare(&foreign, slot, &vec![bytes.clone(); arity], &bytes)
                    .is_err()
            );
        }
        for (arguments, output) in [
            (vec![FieldTypeTp::LongLong.into()], bytes.clone()),
            (vec![bytes], FieldTypeTp::LongLong.into()),
        ] {
            assert!(
                HostCatalog::for_sql_batch(vec![(
                    SqlBatchHostOperation::LegacyNative,
                    HostSignature {
                        arg_types: arguments.into_boxed_slice(),
                        return_type: output
                    }
                )])
                .is_err()
            );
        }
    }

    #[test]
    fn ast_catalog_preserves_native_carriers_and_real_catalog_identity() {
        let bytes: FieldType = FieldTypeTp::Blob.into();
        for arity in [0, 1, 2, 17] {
            let signature = HostSignature {
                arg_types: vec![bytes.clone(); arity].into_boxed_slice(),
                return_type: bytes.clone(),
            };
            let catalog = HostCatalog::for_sql_batch(vec![(
                SqlBatchHostOperation::AstNative,
                signature.clone(),
            )])
            .unwrap();
            let slot = catalog.slot(0).unwrap();
            assert_eq!(
                catalog.sql_batch_operation(slot).unwrap(),
                SqlBatchHostOperation::AstNative
            );
            assert!(!SqlBatchHostOperation::AstNative.is_windowed());
            assert!(
                PreparedHostCall::prepare(&catalog, slot, &signature.arg_types, &bytes).is_ok()
            );
            let foreign =
                HostCatalog::for_sql_batch(vec![(SqlBatchHostOperation::AstNative, signature)])
                    .unwrap();
            assert!(foreign.sql_batch_operation(slot).is_err());
            assert!(
                PreparedHostCall::prepare(&foreign, slot, &vec![bytes.clone(); arity], &bytes)
                    .is_err()
            );
        }
        for (args, output) in [
            (vec![FieldTypeTp::LongLong.into()], bytes.clone()),
            (vec![bytes], FieldTypeTp::LongLong.into()),
        ] {
            assert!(
                HostCatalog::for_sql_batch(vec![(
                    SqlBatchHostOperation::AstNative,
                    HostSignature {
                        arg_types: args.into_boxed_slice(),
                        return_type: output
                    }
                )])
                .is_err()
            );
        }
    }
}
