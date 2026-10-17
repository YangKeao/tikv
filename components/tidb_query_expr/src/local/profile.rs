// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Closed ordinary-call demand facts, not a second function selector or graph.
//!
//! C3a admits identity-domain signed Int PLUS under the two row profiles. C3c
//! separately admits SQL native batch demand with stricter type facts;
//! its facts cannot be passed to the row-only constructor or compiler.
//! Consumer/origin labels are producer assertions. This layer validates the
//! closed source shape, not native consumer selection or protobuf ingestion.

use tidb_query_datatype::{
    EvalType, FieldTypeAccessor, FieldTypeTp, codec::data_type::ScalarValue,
};
use tipb::{FieldType, ScalarFuncSig};

use super::{
    CompileLimits, HostCatalog, HostSlot, LocalError, LocalExpr, LocalResult,
    SqlBatchHostOperation, registry,
};
use crate::{
    CallMetadata, FunctionRef, LiteralKind, LocalFunctionId, types::function::ControlKind,
};

const INT: &[EvalType] = &[EvalType::Int];
const INT_INT: &[EvalType] = &[EvalType::Int, EvalType::Int];
const REAL: &[EvalType] = &[EvalType::Real];
const REAL_REAL: &[EvalType] = &[EvalType::Real, EvalType::Real];
const DECIMAL: &[EvalType] = &[EvalType::Decimal];
const DECIMAL_DECIMAL: &[EvalType] = &[EvalType::Decimal, EvalType::Decimal];
const BYTES: &[EvalType] = &[EvalType::Bytes];
const DATETIME: &[EvalType] = &[EvalType::DateTime];
const DURATION: &[EvalType] = &[EvalType::Duration];
const JSON: &[EvalType] = &[EvalType::Json];
const VECTOR_FLOAT32: &[EvalType] = &[EvalType::VectorFloat32];
const BYTES_INT: &[EvalType] = &[EvalType::Bytes, EvalType::Int];
const BYTES_INT_INT: &[EvalType] = &[EvalType::Bytes, EvalType::Int, EvalType::Int];

// Only these native policy kernels may compose in a checked SQL batch graph.
// Other private operations retain their existing opaque ready-value entry.
// The operation supplies the same arity and metadata constructor used there.
pub(super) fn batch_native_operation(function: FunctionRef) -> Option<super::EvaluatedBytesOp> {
    match function {
        FunctionRef::Local(LocalFunctionId::StrcmpNative) => {
            Some(super::EvaluatedBytesOp::StrcmpNative)
        }
        _ => None,
    }
}

// Result flags are independent of operand carrier permission. Integer
// identities retain the first argument's flag; bit operations always set it.
#[derive(Clone, Copy, Debug)]
enum ResultSignedness {
    Signed,
    Unsigned,
    FirstArgument,
}

/// Exact pure, eager function metadata admitted by the shared batch
/// driver. This is the single capability registry used by admission,
/// compilation, and execution; storage width does not select a driver.
pub fn batch_signature(function: FunctionRef) -> Option<(&'static [EvalType], EvalType)> {
    batch_signature_contract(function).map(|(inputs, output, ..)| (inputs, output))
}

/// Native integer result kind for admitted exact-carrier calls. Identities
/// preserve the actual input kind, independently of its declaration's flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchIntKind {
    Signed,
    Unsigned,
    FirstArgument,
}

/// Shares the registry's result policy instead of another frontend function
/// map.
pub fn batch_int_kind(function: FunctionRef) -> Option<BatchIntKind> {
    if matches!(batch_control(function), Some((_, EvalType::Int))) {
        return Some(BatchIntKind::Signed);
    }
    let (_, output, _, kind) = batch_signature_contract(function)?;
    if output != EvalType::Int {
        return None;
    }
    Some(match kind {
        ResultSignedness::Signed => BatchIntKind::Signed,
        ResultSignedness::Unsigned => BatchIntKind::Unsigned,
        ResultSignedness::FirstArgument => BatchIntKind::FirstArgument,
    })
}

/// Complete call contract, including variable-arity demanded controls.
/// A singleton control stays in the same compiled frame driver; it does not
/// grant an alternate evaluator or demand a skipped argument.
#[derive(Clone, Copy, Debug)]
pub struct BatchCallSignature {
    fixed: &'static [EvalType],
    output: EvalType,
    arity: usize,
    control: Option<ControlKind>,
    host: Option<SqlBatchHostOperation>,
}

impl BatchCallSignature {
    /// Value-only signature of a closed context operation, not a native
    /// callback.
    pub fn for_host(operation: SqlBatchHostOperation, arity: usize) -> Option<Self> {
        operation.accepts_arity(arity).then_some(Self {
            fixed: &[],
            output: operation.output_type(),
            arity,
            control: None,
            host: Some(operation),
        })
    }
    pub fn arity(self) -> usize {
        self.arity
    }
    pub fn output_type(self) -> EvalType {
        self.output
    }
    /// Inputs come from checked native value bindings instead of frontend wire
    /// carrier coercion. Actual SQL argument declarations remain in the owner.
    pub fn uses_native_value_inputs(self) -> bool {
        matches!(
            self.host,
            Some(
                SqlBatchHostOperation::CastCharNative { .. }
                    | SqlBatchHostOperation::CastNative
                    | SqlBatchHostOperation::CoalesceNative
                    | SqlBatchHostOperation::IfNullNative
                    | SqlBatchHostOperation::IfNative
                    | SqlBatchHostOperation::CaseNative
                    | SqlBatchHostOperation::AndNative
                    | SqlBatchHostOperation::OrNative
                    | SqlBatchHostOperation::IsTrueWithNullNative
                    | SqlBatchHostOperation::UnaryNotNative
                    | SqlBatchHostOperation::IsNullNative
                    | SqlBatchHostOperation::IsTrueNative
                    | SqlBatchHostOperation::IsFalseNative
                    | SqlBatchHostOperation::IsNotTrueNative
                    | SqlBatchHostOperation::IsNotFalseNative
                    | SqlBatchHostOperation::IsUnknownNative
                    | SqlBatchHostOperation::IsNotUnknownNative
                    | SqlBatchHostOperation::CompareNative(_)
                    | SqlBatchHostOperation::NullSafeEqNative
                    | SqlBatchHostOperation::NullIfNative
                    | SqlBatchHostOperation::AbsNative
            )
        )
    }
    pub fn requires_singleton(self) -> bool {
        self.control.is_some() || self.host.is_some()
    }
    pub fn argument_type(self, index: usize) -> Option<EvalType> {
        if index >= self.arity {
            return None;
        }
        Some(match self.control {
            Some(ControlKind::If) if index == 0 => EvalType::Int,
            Some(ControlKind::CaseWhen) if index % 2 == 0 && index + 1 < self.arity => {
                EvalType::Int
            }
            Some(ControlKind::And | ControlKind::Or) => EvalType::Int,
            Some(_) => self.output,
            None if self.host.is_some() => self.host.expect("checked host").argument_type(),
            None => self.fixed[index],
        })
    }
}

pub(super) fn batch_control(function: FunctionRef) -> Option<(ControlKind, EvalType)> {
    use tipb::ScalarFuncSig::*;
    let FunctionRef::TiPb(sig) = function else {
        return None;
    };
    let (kind, output) = match sig {
        LogicalAnd => (ControlKind::And, EvalType::Int),
        LogicalOr => (ControlKind::Or, EvalType::Int),
        IfInt => (ControlKind::If, EvalType::Int),
        IfReal => (ControlKind::If, EvalType::Real),
        IfDecimal => (ControlKind::If, EvalType::Decimal),
        IfString => (ControlKind::If, EvalType::Bytes),
        IfTime => (ControlKind::If, EvalType::DateTime),
        IfDuration => (ControlKind::If, EvalType::Duration),
        IfJson => (ControlKind::If, EvalType::Json),
        IfNullInt => (ControlKind::IfNull, EvalType::Int),
        IfNullReal => (ControlKind::IfNull, EvalType::Real),
        IfNullDecimal => (ControlKind::IfNull, EvalType::Decimal),
        IfNullString => (ControlKind::IfNull, EvalType::Bytes),
        IfNullTime => (ControlKind::IfNull, EvalType::DateTime),
        IfNullDuration => (ControlKind::IfNull, EvalType::Duration),
        IfNullJson => (ControlKind::IfNull, EvalType::Json),
        CoalesceInt => (ControlKind::Coalesce, EvalType::Int),
        CoalesceReal => (ControlKind::Coalesce, EvalType::Real),
        CoalesceDecimal => (ControlKind::Coalesce, EvalType::Decimal),
        CoalesceString => (ControlKind::Coalesce, EvalType::Bytes),
        CoalesceTime => (ControlKind::Coalesce, EvalType::DateTime),
        CoalesceDuration => (ControlKind::Coalesce, EvalType::Duration),
        CoalesceJson => (ControlKind::Coalesce, EvalType::Json),
        CaseWhenInt => (ControlKind::CaseWhen, EvalType::Int),
        CaseWhenReal => (ControlKind::CaseWhen, EvalType::Real),
        CaseWhenDecimal => (ControlKind::CaseWhen, EvalType::Decimal),
        CaseWhenString => (ControlKind::CaseWhen, EvalType::Bytes),
        CaseWhenTime => (ControlKind::CaseWhen, EvalType::DateTime),
        CaseWhenDuration => (ControlKind::CaseWhen, EvalType::Duration),
        CaseWhenJson => (ControlKind::CaseWhen, EvalType::Json),
        _ => return None,
    };
    Some((kind, output))
}

/// The canonical checked signature for an actual call occurrence. The older
/// fixed signature API intentionally continues to describe eager calls only.
pub fn batch_signature_for_arity(
    function: FunctionRef,
    arity: usize,
) -> Option<BatchCallSignature> {
    if let Some((kind, output)) = batch_control(function) {
        let valid = match kind {
            ControlKind::If => arity == 3,
            ControlKind::IfNull | ControlKind::And | ControlKind::Or => arity == 2,
            ControlKind::Coalesce => arity >= 1,
            ControlKind::CaseWhen => arity >= 2,
        };
        return valid.then_some(BatchCallSignature {
            fixed: &[],
            output,
            arity,
            control: Some(kind),
            host: None,
        });
    }
    let (fixed, output) = batch_signature(function)?;
    (fixed.len() == arity).then_some(BatchCallSignature {
        fixed,
        output,
        arity,
        control: None,
        host: None,
    })
}

// The third fact admits unsigned Int operands. Their carrier preserves all
// bits; the official comparison selector uses each operand's FieldType.
// The fourth fact declares the result flag policy, including integer
// identities. Other families retain their current signed-only result contract.
fn batch_signature_contract(
    function: FunctionRef,
) -> Option<(&'static [EvalType], EvalType, bool, ResultSignedness)> {
    if let Some(operation) = batch_native_operation(function) {
        let output = EvalType::try_from(operation.return_type().as_accessor().tp()).ok()?;
        return Some((
            operation.input_types(),
            output,
            false,
            ResultSignedness::Signed,
        ));
    }
    if matches!(
        function,
        FunctionRef::Local(
            LocalFunctionId::JsonTypeTextSqlNative | LocalFunctionId::JsonTypeBinarySqlNative
        )
    ) {
        return Some((BYTES, EvalType::Bytes, false, ResultSignedness::Signed));
    }
    if function == FunctionRef::Local(LocalFunctionId::NullIfIntSignedSigned) {
        return Some((INT_INT, EvalType::Int, false, ResultSignedness::Signed));
    }
    let FunctionRef::TiPb(signature) = function else {
        return None;
    };
    match signature {
        ScalarFuncSig::Pi => Some((&[], EvalType::Real, false, ResultSignedness::Signed)),
        ScalarFuncSig::UnaryNotInt => Some((INT, EvalType::Int, true, ResultSignedness::Signed)),
        ScalarFuncSig::Asin | ScalarFuncSig::Acos => {
            Some((REAL, EvalType::Real, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::IntIsNull => Some((INT, EvalType::Int, true, ResultSignedness::Signed)),
        ScalarFuncSig::RealIsNull => Some((REAL, EvalType::Int, false, ResultSignedness::Signed)),
        ScalarFuncSig::DecimalIsNull => {
            Some((DECIMAL, EvalType::Int, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::StringIsNull => {
            Some((BYTES, EvalType::Int, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::TimeIsNull => {
            Some((DATETIME, EvalType::Int, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::DurationIsNull => {
            Some((DURATION, EvalType::Int, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::VectorFloat32IsNull => Some((
            VECTOR_FLOAT32,
            EvalType::Int,
            false,
            ResultSignedness::Signed,
        )),
        ScalarFuncSig::AbsInt => Some((INT, EvalType::Int, false, ResultSignedness::Signed)),
        ScalarFuncSig::IntAnyValue | ScalarFuncSig::RoundInt => {
            Some((INT, EvalType::Int, true, ResultSignedness::FirstArgument))
        }
        ScalarFuncSig::PlusInt | ScalarFuncSig::MinusInt | ScalarFuncSig::MultiplyInt => {
            Some((INT_INT, EvalType::Int, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::BitAndSig
        | ScalarFuncSig::BitOrSig
        | ScalarFuncSig::BitXorSig
        | ScalarFuncSig::LeftShift
        | ScalarFuncSig::RightShift => {
            Some((INT_INT, EvalType::Int, true, ResultSignedness::Unsigned))
        }
        ScalarFuncSig::EqInt
        | ScalarFuncSig::GtInt
        | ScalarFuncSig::LtInt
        | ScalarFuncSig::LeInt
        | ScalarFuncSig::GeInt
        | ScalarFuncSig::NeInt
        | ScalarFuncSig::NullEqInt => {
            Some((INT_INT, EvalType::Int, true, ResultSignedness::Signed))
        }
        ScalarFuncSig::LtReal
        | ScalarFuncSig::LeReal
        | ScalarFuncSig::GtReal
        | ScalarFuncSig::GeReal
        | ScalarFuncSig::EqReal
        | ScalarFuncSig::NeReal
        | ScalarFuncSig::NullEqReal => {
            Some((REAL_REAL, EvalType::Int, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::LtDecimal
        | ScalarFuncSig::LeDecimal
        | ScalarFuncSig::GtDecimal
        | ScalarFuncSig::GeDecimal
        | ScalarFuncSig::EqDecimal
        | ScalarFuncSig::NeDecimal
        | ScalarFuncSig::NullEqDecimal => Some((
            DECIMAL_DECIMAL,
            EvalType::Int,
            false,
            ResultSignedness::Signed,
        )),
        ScalarFuncSig::AbsReal => Some((REAL, EvalType::Real, false, ResultSignedness::Signed)),
        ScalarFuncSig::RealAnyValue => {
            Some((REAL, EvalType::Real, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::PlusReal | ScalarFuncSig::MinusReal | ScalarFuncSig::MultiplyReal => {
            Some((REAL_REAL, EvalType::Real, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::AbsDecimal => {
            Some((DECIMAL, EvalType::Decimal, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::DecimalAnyValue => {
            Some((DECIMAL, EvalType::Decimal, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::PlusDecimal
        | ScalarFuncSig::MinusDecimal
        | ScalarFuncSig::MultiplyDecimal => Some((
            DECIMAL_DECIMAL,
            EvalType::Decimal,
            false,
            ResultSignedness::Signed,
        )),
        ScalarFuncSig::CharLength | ScalarFuncSig::CharLengthUtf8 => {
            Some((BYTES, EvalType::Int, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::StringAnyValue => {
            Some((BYTES, EvalType::Bytes, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::Lower
        | ScalarFuncSig::LowerUtf8
        | ScalarFuncSig::Upper
        | ScalarFuncSig::UpperUtf8 => {
            Some((BYTES, EvalType::Bytes, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::Substring2Args | ScalarFuncSig::Substring2ArgsUtf8 => {
            Some((BYTES_INT, EvalType::Bytes, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::Substring3Args | ScalarFuncSig::Substring3ArgsUtf8 => Some((
            BYTES_INT_INT,
            EvalType::Bytes,
            false,
            ResultSignedness::Signed,
        )),
        ScalarFuncSig::TimeAnyValue => Some((
            DATETIME,
            EvalType::DateTime,
            false,
            ResultSignedness::Signed,
        )),
        ScalarFuncSig::DurationAnyValue => Some((
            DURATION,
            EvalType::Duration,
            false,
            ResultSignedness::Signed,
        )),
        ScalarFuncSig::JsonAnyValue => {
            Some((JSON, EvalType::Json, false, ResultSignedness::Signed))
        }
        ScalarFuncSig::VectorFloat32AnyValue => Some((
            VECTOR_FLOAT32,
            EvalType::VectorFloat32,
            false,
            ResultSignedness::Signed,
        )),
        _ => None,
    }
}

/// The consumer's execution contract, not a property inferred from a function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrdinaryProfile {
    TypedRow,
    PbRow,
    /// Representable for explicit rejection; not admitted by C3a.
    AstValueScalar,
    /// Requires an operand-major scheduler; not admitted by C3a.
    NativeBatch,
}

impl OrdinaryProfile {
    fn check_admitted(self) -> LocalResult<()> {
        match self {
            Self::TypedRow | Self::PbRow => Ok(()),
            Self::AstValueScalar | Self::NativeBatch => Err(LocalError::InvalidSpec(
                "ordinary consumer profile is outside the C3a row domain".into(),
            )),
        }
    }
}

/// Detached source identity. Equal IDs do not merge expression occurrences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrdinarySourceId {
    unit: u64,
    node: u64,
}

impl OrdinarySourceId {
    pub const fn new(unit: u64, node: u64) -> Self {
        Self { unit, node }
    }

    pub const fn unit(self) -> u64 {
        self.unit
    }

    pub const fn node(self) -> u64 {
        self.node
    }
}

/// Facts for one call occurrence in the source tree.
///
/// `ordinal` counts ALL nodes in source preorder: root 0, then every argument
/// left to right, including constants and input slots. It is neither a
/// call-only index nor a physical-row index. The admitted row profiles
/// explicitly demand the typed left operand, stop on its NULL, and only then
/// demand the right.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrdinaryCallSite {
    ordinal: usize,
    source: OrdinarySourceId,
    profile: OrdinaryProfile,
    original_pb_signature: Option<i32>,
}

impl OrdinaryCallSite {
    pub const fn typed_row(ordinal: usize, source: OrdinarySourceId) -> Self {
        Self {
            ordinal,
            source,
            profile: OrdinaryProfile::TypedRow,
            original_pb_signature: None,
        }
    }

    pub const fn pb_row(
        ordinal: usize,
        source: OrdinarySourceId,
        original_pb_signature: i32,
    ) -> Self {
        Self {
            ordinal,
            source,
            profile: OrdinaryProfile::PbRow,
            original_pb_signature: Some(original_pb_signature),
        }
    }

    /// Asserts SQL native batch demand, without a PB-origin assertion.
    /// Only `BatchFacts` admits this label; row facts still reject it.
    pub const fn sql_native_batch(ordinal: usize, source: OrdinarySourceId) -> Self {
        Self {
            ordinal,
            source,
            profile: OrdinaryProfile::NativeBatch,
            original_pb_signature: None,
        }
    }

    pub const fn ordinal(&self) -> usize {
        self.ordinal
    }

    pub const fn source(&self) -> OrdinarySourceId {
        self.source
    }

    pub const fn profile(&self) -> OrdinaryProfile {
        self.profile
    }

    pub const fn original_pb_signature(&self) -> Option<i32> {
        self.original_pb_signature
    }
}

#[derive(Clone, Debug, PartialEq)]
enum NodeKind {
    Constant {
        value: ScalarValue,
        literal_kind: LiteralKind,
    },
    InputSlot {
        slot: usize,
    },
    /// One admitted wire or native-policy kernel with exact arity and no
    /// metadata. The complete function identity is snapshotted and revalidated.
    BatchCall {
        signature: FunctionRef,
        arity: usize,
    },
    BatchHost {
        slot: HostSlot,
        operation: SqlBatchHostOperation,
        arity: usize,
    },
}

#[derive(Clone, Debug, PartialEq)]
struct NodeSnapshot {
    kind: NodeKind,
    field_type: FieldType,
}

/// Immutable row-only admission facts. Mixed TypedRow/PbRow children retain
/// their individual row demand; neither the consumer nor a child may use the
/// batch label. There is no conversion from BatchFacts.
#[derive(Clone, Debug)]
pub struct OrdinaryProfileSpec {
    consumer: OrdinaryProfile,
    snapshot: ClosedSourceSnapshot,
    sites: Box<[OrdinaryCallSite]>,
}

impl OrdinaryProfileSpec {
    pub fn new(
        spec: &LocalExpr,
        schema: &[FieldType],
        consumer: OrdinaryProfile,
        sites: Vec<OrdinaryCallSite>,
        limits: CompileLimits,
    ) -> LocalResult<Self> {
        consumer.check_admitted()?;
        if sites.len() > limits.max_nodes {
            return Err(resource("ordinary call-site count exceeds node budget"));
        }
        if sites
            .windows(2)
            .any(|pair| pair[0].ordinal >= pair[1].ordinal)
        {
            return Err(invalid(
                "ordinary call sites must have strictly increasing source ordinals",
            ));
        }
        let snapshot = ClosedSourceSnapshot::capture(
            spec,
            schema,
            limits,
            TypePolicy::RowBaseline,
            |nodes| check_sites(nodes, consumer, &sites),
        )?;
        Ok(Self {
            consumer,
            snapshot,
            sites: sites.into_boxed_slice(),
        })
    }

    pub const fn consumer(&self) -> OrdinaryProfile {
        self.consumer
    }

    pub fn node_count(&self) -> usize {
        self.snapshot.nodes.len()
    }

    pub fn call_sites(&self) -> &[OrdinaryCallSite] {
        &self.sites
    }

    pub(crate) fn validate(
        &self,
        spec: &LocalExpr,
        schema: &[FieldType],
        limits: CompileLimits,
    ) -> LocalResult<()> {
        self.snapshot
            .validate(spec, schema, limits, TypePolicy::RowBaseline)
    }

    pub(crate) fn site(&self, ordinal: usize) -> Option<&OrdinaryCallSite> {
        self.sites
            .binary_search_by_key(&ordinal, |site| site.ordinal)
            .ok()
            .map(|index| &self.sites[index])
    }
}

/// Immutable SQL batch admission facts for closed typed trees with demanded
/// controls.
///
/// Every call has a batch-only site; all nodes and schema fields satisfy the
/// strict batch type policy. Full source/schema snapshots are rechecked by the
/// batch compiler. The label asserts demand, not a proven native consumer path.
/// Leaf roots are allowed without sites; this does not claim that a native
/// batch entry is selected for every standalone constant or column.
/// These facts contain no executable graph, evaluator, cache or row-facts
/// escape.
#[derive(Clone, Debug)]
pub struct BatchFacts {
    snapshot: ClosedSourceSnapshot,
    sites: Box<[OrdinaryCallSite]>,
    hosts: Option<HostCatalog>,
}

impl BatchFacts {
    pub fn sql_native_batch(
        spec: &LocalExpr,
        schema: &[FieldType],
        sites: Vec<OrdinaryCallSite>,
        limits: CompileLimits,
    ) -> LocalResult<Self> {
        Self::capture(spec, schema, sites, limits, None)
    }

    /// Explicit SQL host catalog; every host site and descriptor is rechecked.
    pub fn sql_native_batch_with_hosts(
        spec: &LocalExpr,
        schema: &[FieldType],
        sites: Vec<OrdinaryCallSite>,
        limits: CompileLimits,
        hosts: &HostCatalog,
    ) -> LocalResult<Self> {
        if !hosts.is_sql_batch() {
            return Err(invalid(
                "row host catalog cannot authorize SQL batch execution",
            ));
        }
        Self::capture(spec, schema, sites, limits, Some(hosts))
    }

    fn capture(
        spec: &LocalExpr,
        schema: &[FieldType],
        sites: Vec<OrdinaryCallSite>,
        limits: CompileLimits,
        hosts: Option<&HostCatalog>,
    ) -> LocalResult<Self> {
        if sites.len() > limits.max_nodes {
            return Err(resource("batch call-site count exceeds node budget"));
        }
        if sites
            .windows(2)
            .any(|pair| pair[0].ordinal >= pair[1].ordinal)
        {
            return Err(invalid(
                "batch call sites must have strictly increasing source ordinals",
            ));
        }
        let snapshot = ClosedSourceSnapshot::capture(
            spec,
            schema,
            limits,
            TypePolicy::SqlNativeBatch(hosts),
            |nodes| check_batch_sites(nodes, &sites),
        )?;
        Ok(Self {
            snapshot,
            sites: sites.into_boxed_slice(),
            hosts: hosts.cloned(),
        })
    }

    pub fn node_count(&self) -> usize {
        self.snapshot.nodes.len()
    }

    pub fn call_sites(&self) -> &[OrdinaryCallSite] {
        &self.sites
    }

    pub(super) fn max_batch_size(&self) -> usize {
        if self.snapshot.nodes.iter().any(|node| {
            matches!(node.kind,
            NodeKind::BatchCall { signature, .. } if batch_control(signature).is_some())
                || matches!(node.kind, NodeKind::BatchHost { .. })
        }) {
            1
        } else {
            crate::types::BATCH_MAX_SIZE
        }
    }

    pub(super) fn hosts(&self) -> Option<&HostCatalog> {
        self.hosts.as_ref()
    }

    pub(crate) fn validate(
        &self,
        spec: &LocalExpr,
        schema: &[FieldType],
        limits: CompileLimits,
    ) -> LocalResult<()> {
        self.snapshot.validate(
            spec,
            schema,
            limits,
            TypePolicy::SqlNativeBatch(self.hosts.as_ref()),
        )
    }

    pub(crate) fn site(&self, ordinal: usize) -> Option<&OrdinaryCallSite> {
        self.sites
            .binary_search_by_key(&ordinal, |site| site.ordinal)
            .ok()
            .map(|index| &self.sites[index])
    }
}

/// The shared representation is an exact flat source snapshot; preorder plus
/// snapshotted arities preserves shape. Clone/Debug/Drop never recurse through
/// a LocalExpr. Entry factories retain their separate admission domains.
#[derive(Clone, Debug)]
struct ClosedSourceSnapshot {
    nodes: Box<[NodeSnapshot]>,
    schema: Box<[FieldType]>,
}

impl ClosedSourceSnapshot {
    fn capture(
        spec: &LocalExpr,
        schema: &[FieldType],
        limits: CompileLimits,
        policy: TypePolicy<'_>,
        check_sites: impl FnOnce(&[NodeSnapshot]) -> LocalResult<()>,
    ) -> LocalResult<Self> {
        if matches!(policy, TypePolicy::SqlNativeBatch(_)) {
            // Reject an invalid descendant/schema or exceeded budget before
            // copying any descriptors. Row construction keeps its old policy.
            let mut preflight = Walk::new(spec, schema, limits, policy)?;
            while preflight.next()?.is_some() {}
        }
        let mut walk = Walk::new(spec, schema, limits, policy)?;
        let mut nodes = Vec::new();
        while let Some((kind, field_type)) = walk.next()? {
            // Scheduling is bounded before a descriptor is cloned, including
            // the descendants newly scheduled by this node.
            nodes
                .try_reserve(1)
                .map_err(|_| resource("ordinary snapshot allocation failed"))?;
            nodes.push(NodeSnapshot {
                kind,
                field_type: field_type.clone(),
            });
        }
        check_sites(&nodes)?;
        let mut copied_schema = Vec::new();
        copied_schema
            .try_reserve(schema.len())
            .map_err(|_| resource("ordinary schema allocation failed"))?;
        copied_schema.extend_from_slice(schema);
        Ok(Self {
            nodes: nodes.into_boxed_slice(),
            schema: copied_schema.into_boxed_slice(),
        })
    }

    /// Recheck current limits, type policy and exact source/schema without
    /// cloning the source or creating a second descriptor snapshot.
    fn validate(
        &self,
        spec: &LocalExpr,
        schema: &[FieldType],
        limits: CompileLimits,
        policy: TypePolicy<'_>,
    ) -> LocalResult<()> {
        if self.schema.as_ref() != schema {
            return Err(invalid(
                "ordinary profile schema differs from its exact snapshot",
            ));
        }
        let mut walk = Walk::new(spec, schema, limits, policy)?;
        let mut ordinal = 0;
        while let Some((kind, field_type)) = walk.next()? {
            let Some(expected) = self.nodes.get(ordinal) else {
                return Err(invalid("ordinary profile source has additional nodes"));
            };
            if expected.kind != kind || &expected.field_type != field_type {
                return Err(invalid(
                    "ordinary profile source differs from its exact snapshot",
                ));
            }
            ordinal += 1;
        }
        if ordinal != self.nodes.len() {
            return Err(invalid("ordinary profile source has missing nodes"));
        }
        Ok(())
    }
}

fn check_sites(
    nodes: &[NodeSnapshot],
    consumer: OrdinaryProfile,
    sites: &[OrdinaryCallSite],
) -> LocalResult<()> {
    let mut next = 0;
    for (ordinal, node) in nodes.iter().enumerate() {
        if !matches!(
            node.kind,
            NodeKind::BatchCall { .. } | NodeKind::BatchHost { .. }
        ) {
            continue;
        }
        let site = sites
            .get(next)
            .ok_or_else(|| invalid("ordinary profile is missing a call site"))?;
        if site.ordinal != ordinal {
            return Err(invalid(
                "ordinary call site does not identify the next source call",
            ));
        }
        site.profile.check_admitted()?;
        match (site.profile, site.original_pb_signature) {
            (OrdinaryProfile::TypedRow, None) => {}
            (OrdinaryProfile::PbRow, Some(signature))
                if signature == ScalarFuncSig::PlusInt as i32 => {}
            _ => {
                return Err(invalid(
                    "ordinary call site does not preserve exact PlusInt=203 provenance",
                ));
            }
        }
        if ordinal == 0 && site.profile != consumer {
            return Err(invalid(
                "ordinary root call profile differs from its consumer",
            ));
        }
        next += 1;
    }
    if next != sites.len() {
        return Err(invalid("ordinary profile contains an extra call site"));
    }
    Ok(())
}

fn check_batch_sites(nodes: &[NodeSnapshot], sites: &[OrdinaryCallSite]) -> LocalResult<()> {
    let mut next = 0;
    for (ordinal, node) in nodes.iter().enumerate() {
        if !matches!(
            node.kind,
            NodeKind::BatchCall { .. } | NodeKind::BatchHost { .. }
        ) {
            continue;
        }
        let site = sites
            .get(next)
            .ok_or_else(|| invalid("batch is missing a call site"))?;
        if site.ordinal != ordinal {
            return Err(invalid(
                "batch call site does not identify the next source call",
            ));
        }
        if site.profile != OrdinaryProfile::NativeBatch || site.original_pb_signature.is_some() {
            return Err(invalid(
                "batch calls require SQL batch sites without PB provenance",
            ));
        }
        next += 1;
    }
    if next != sites.len() {
        return Err(invalid("batch contains an extra call site"));
    }
    Ok(())
}

fn invalid(message: &str) -> LocalError {
    LocalError::InvalidSpec(message.into())
}

fn resource(message: &str) -> LocalError {
    LocalError::ResourceLimit(message.into())
}

#[derive(Clone, Copy)]
enum TypePolicy<'a> {
    RowBaseline,
    SqlNativeBatch(Option<&'a HostCatalog>),
}

// Keep raw protobuf flags: TiKV's FieldTypeFlag accessor names only a subset.
// The native TiDB flag inventory is rust/crates/tidb-datatype/src/field_type/
// mod.rs:91-149. C3c rejects ENUM, SET, PARSE_TO_JSON,
// ENUM_SET_AS_INT and undocumented bits; the remaining documented bits are
// retained verbatim.
const BATCH_EXCLUDED_FLAGS: u32 = (1 << 8) | (1 << 11) | (1 << 18) | (1 << 21);
const DOCUMENTED_FLAGS: u32 = (1 << 25) - 1;

pub(super) fn check_batch_type(field: &FieldType) -> LocalResult<()> {
    check_type(field, TypePolicy::SqlNativeBatch(None))
}

fn check_type(field_type: &FieldType, policy: TypePolicy<'_>) -> LocalResult<()> {
    match policy {
        TypePolicy::RowBaseline => registry::check_signed_int_type(field_type)
            .map_err(|error| LocalError::InvalidSpec(error.to_string())),
        TypePolicy::SqlNativeBatch(_) => {
            let eval_type = EvalType::try_from(field_type.as_accessor().tp()).ok();
            let raw_type = field_type.get_tp();
            let raw_matches = match eval_type {
                Some(EvalType::Int) => {
                    matches!(raw_type, value if value == FieldTypeTp::Tiny as i32
                    || value == FieldTypeTp::Short as i32 || value == FieldTypeTp::Int24 as i32
                    || value == FieldTypeTp::Long as i32 || value == FieldTypeTp::LongLong as i32)
                }
                Some(EvalType::Real) => raw_type == FieldTypeTp::Double as i32,
                Some(EvalType::Decimal) => raw_type == FieldTypeTp::NewDecimal as i32,
                Some(EvalType::Bytes) => matches!(
                    raw_type,
                    value if value == FieldTypeTp::VarChar as i32
                        || value == FieldTypeTp::VarString as i32
                        || value == FieldTypeTp::String as i32
                        || value == FieldTypeTp::Blob as i32
                        || value == FieldTypeTp::TinyBlob as i32
                        || value == FieldTypeTp::MediumBlob as i32
                        || value == FieldTypeTp::LongBlob as i32
                ),
                Some(EvalType::DateTime) => matches!(
                    raw_type,
                    value if value == FieldTypeTp::Date as i32
                        || value == FieldTypeTp::DateTime as i32
                        || value == FieldTypeTp::Timestamp as i32
                ),
                Some(EvalType::Duration) => raw_type == FieldTypeTp::Duration as i32,
                Some(EvalType::Json) => raw_type == FieldTypeTp::Json as i32,
                Some(EvalType::VectorFloat32) => raw_type == FieldTypeTp::TiDbVectorFloat32 as i32,
                _ => false,
            };
            if !raw_matches
                || field_type.get_array()
                || (field_type.get_flag() & (1 << 5) != 0 && eval_type != Some(EvalType::Int))
                || field_type.get_flag() & (BATCH_EXCLUDED_FLAGS | !DOCUMENTED_FLAGS) != 0
            {
                return Err(invalid(
                    "batch requires a supported non-array native EvalType and raw flags",
                ));
            }
            Ok(())
        }
    }
}

fn describe(
    expr: &LocalExpr,
    schema: &[FieldType],
    policy: TypePolicy<'_>,
) -> LocalResult<NodeKind> {
    check_type(expr.field_type(), policy)?;
    match expr {
        LocalExpr::Constant {
            value,
            literal_kind,
            ..
        } => {
            if matches!(policy, TypePolicy::RowBaseline) && !matches!(value, ScalarValue::Int(_)) {
                return Err(invalid("ordinary constants require the Int carrier"));
            }
            if matches!(policy, TypePolicy::SqlNativeBatch(_))
                && (!matches!(
                    value,
                    ScalarValue::Int(_)
                        | ScalarValue::Real(_)
                        | ScalarValue::Decimal(_)
                        | ScalarValue::Bytes(_)
                        | ScalarValue::DateTime(_)
                        | ScalarValue::Duration(_)
                        | ScalarValue::Json(_)
                        | ScalarValue::VectorFloat32(_)
                ) || EvalType::try_from(expr.field_type().as_accessor().tp()).ok()
                    != Some(value.eval_type()))
            {
                return Err(invalid(
                    "batch constants must match their declared native EvalType carrier",
                ));
            }
            if *literal_kind != LiteralKind::Typed {
                return Err(invalid(
                    "ordinary constants require Typed literal provenance",
                ));
            }
            Ok(NodeKind::Constant {
                value: value.clone(),
                literal_kind: *literal_kind,
            })
        }
        LocalExpr::InputSlot { slot, field_type } => {
            if schema.get(*slot) != Some(field_type) {
                return Err(invalid(
                    "ordinary input slot differs from the complete schema",
                ));
            }
            Ok(NodeKind::InputSlot { slot: *slot })
        }
        LocalExpr::Call {
            function,
            args,
            metadata,
            ..
        } => {
            let signature = *function;
            if matches!(signature, FunctionRef::Local(LocalFunctionId::StrcmpNative)) {
                use tidb_query_datatype::codec::collation::native::NativeCollation;
                if !matches!(args.get(2), Some(LocalExpr::Constant {
                    value: ScalarValue::Int(Some(tag)), ..
                }) if NativeCollation::from_tag(*tag).is_some())
                {
                    return Err(invalid(
                        "native batch comparison requires a fixed known collation policy",
                    ));
                }
            }
            if matches!(policy, TypePolicy::SqlNativeBatch(_)) && batch_control(signature).is_some()
            {
                let contract = batch_signature_for_arity(signature, args.len())
                    .ok_or_else(|| invalid("invalid batch control arity"))?;
                if !matches!(metadata, CallMetadata::None)
                    || EvalType::try_from(expr.field_type().as_accessor().tp()).ok() != Some(contract.output_type())
                    || args.iter().enumerate().any(|(index, arg)| {
                        EvalType::try_from(arg.field_type().as_accessor().tp()).ok() != contract.argument_type(index)
                    })
                    // Coercion and mixed unsigned branches are not implicit in
                    // a control node. Keep this initial closure signed-only.
                    || expr.field_type().get_flag() & (1 << 5) != 0
                    || args.iter().any(|arg| arg.field_type().get_flag() & (1 << 5) != 0)
                {
                    return Err(invalid(
                        "batch control requires exact carriers and no unsigned coercion/metadata",
                    ));
                }
                return Ok(NodeKind::BatchCall {
                    signature,
                    arity: args.len(),
                });
            }
            if signature == FunctionRef::Local(LocalFunctionId::NullIfIntSignedSigned)
                && (args.len() != 2
                    || args
                        .iter()
                        .any(|arg| arg.field_type().get_tp() != FieldTypeTp::LongLong as i32)
                    || args.first().map(LocalExpr::field_type) != Some(expr.field_type()))
            {
                return Err(invalid(
                    "NULLIF requires its exact signed LongLong first argument declaration",
                ));
            }
            let admitted = match policy {
                TypePolicy::RowBaseline
                    if signature == FunctionRef::TiPb(ScalarFuncSig::PlusInt) =>
                {
                    Some((INT_INT, EvalType::Int, false, ResultSignedness::Signed))
                }
                TypePolicy::RowBaseline => None,
                TypePolicy::SqlNativeBatch(_) => batch_signature_contract(signature),
            };
            let (input_types, output_type, unsigned_inputs, result_signedness) =
                admitted.unwrap_or((&[], EvalType::Int, false, ResultSignedness::Signed));
            let actual_output = EvalType::try_from(expr.field_type().as_accessor().tp()).ok();
            let inputs_match = args.iter().zip(input_types).all(|(arg, input_type)| {
                EvalType::try_from(arg.field_type().as_accessor().tp()).ok() == Some(*input_type)
            });
            if admitted.is_none()
                || args.len() != input_types.len()
                || !matches!(metadata, CallMetadata::None)
            {
                return Err(invalid(
                    "ordinary calls require an admitted batch signature, exact arity and no metadata",
                ));
            }
            let unsigned_result = match result_signedness {
                ResultSignedness::Signed => false,
                ResultSignedness::Unsigned => true,
                ResultSignedness::FirstArgument => args
                    .first()
                    .is_some_and(|arg| arg.field_type().get_flag() & (1 << 5) != 0),
            };
            if (expr.field_type().get_flag() & (1 << 5) != 0) != unsigned_result
                || (!unsigned_inputs
                    && args
                        .iter()
                        .any(|arg| arg.field_type().get_flag() & (1 << 5) != 0))
            {
                return Err(invalid(
                    "batch signature does not admit unsigned operands or result",
                ));
            }
            if actual_output != Some(output_type) || !inputs_match {
                return Err(invalid(
                    "batch call input/output carriers differ from its exact signature",
                ));
            }
            Ok(NodeKind::BatchCall {
                signature,
                arity: input_types.len(),
            })
        }
        LocalExpr::HostCall {
            slot,
            args,
            return_type,
        } => {
            let TypePolicy::SqlNativeBatch(Some(hosts)) = policy else {
                return Err(invalid("host call requires an explicit SQL batch catalog"));
            };
            if !hosts.is_sql_batch() {
                return Err(invalid("SQL batch cannot use a row host catalog"));
            }
            let signature = hosts.signature(*slot)?;
            if &signature.return_type != return_type
                || signature.arg_types.len() != args.len()
                || signature
                    .arg_types
                    .iter()
                    .zip(args)
                    .any(|(field, arg)| field != arg.field_type())
            {
                return Err(invalid("host call descriptors differ from its SQL catalog"));
            }
            Ok(NodeKind::BatchHost {
                slot: *slot,
                operation: hosts.sql_batch_operation(*slot)?,
                arity: args.len(),
            })
        }
    }
}

/// Bounded source-preorder traversal. The stack contains borrowed source nodes,
/// never cloned executable children. Scheduling is charged before stack growth.
struct Walk<'a> {
    pending: Vec<(&'a LocalExpr, usize)>,
    schema: &'a [FieldType],
    scheduled: usize,
    limits: CompileLimits,
    policy: TypePolicy<'a>,
}

impl<'a> Walk<'a> {
    fn new(
        spec: &'a LocalExpr,
        schema: &'a [FieldType],
        limits: CompileLimits,
        policy: TypePolicy<'a>,
    ) -> LocalResult<Self> {
        if limits.max_nodes == 0 || limits.max_depth == 0 {
            return Err(resource("ordinary construction node/depth budget exceeded"));
        }
        for field_type in schema {
            check_type(field_type, policy)?;
        }
        let mut pending = Vec::new();
        pending
            .try_reserve(1)
            .map_err(|_| resource("ordinary traversal allocation failed"))?;
        pending.push((spec, 1));
        Ok(Self {
            pending,
            schema,
            scheduled: 1,
            limits,
            policy,
        })
    }

    fn next(&mut self) -> LocalResult<Option<(NodeKind, &'a FieldType)>> {
        let Some((expr, depth)) = self.pending.pop() else {
            return Ok(None);
        };
        let kind = describe(expr, self.schema, self.policy)?;
        if let LocalExpr::Call { args, .. } | LocalExpr::HostCall { args, .. } = expr {
            let child_depth = depth
                .checked_add(1)
                .ok_or_else(|| resource("ordinary construction depth overflow"))?;
            self.scheduled = self
                .scheduled
                .checked_add(args.len())
                .ok_or_else(|| resource("ordinary construction node count overflow"))?;
            if self.scheduled > self.limits.max_nodes || child_depth > self.limits.max_depth {
                return Err(resource("ordinary construction node/depth budget exceeded"));
            }
            self.pending
                .try_reserve(args.len())
                .map_err(|_| resource("ordinary traversal allocation failed"))?;
            for arg in args.iter().rev() {
                self.pending.push((arg, child_depth));
            }
        }
        Ok(Some((kind, expr.field_type())))
    }
}

#[cfg(test)]
mod native_integer_kind_tests {
    use super::*;
    #[test]
    fn native_integer_kind_policy_shares_registry_identity_and_computed_rules() {
        use tipb::ScalarFuncSig::*;
        assert_eq!(
            batch_int_kind(FunctionRef::TiPb(IntAnyValue)),
            Some(BatchIntKind::FirstArgument)
        );
        assert_eq!(
            batch_int_kind(FunctionRef::TiPb(RoundInt)),
            Some(BatchIntKind::FirstArgument)
        );
        assert_eq!(
            batch_int_kind(FunctionRef::TiPb(BitXorSig)),
            Some(BatchIntKind::Unsigned)
        );
        assert_eq!(
            batch_int_kind(FunctionRef::TiPb(IntIsNull)),
            Some(BatchIntKind::Signed)
        );
        assert_eq!(
            batch_int_kind(FunctionRef::TiPb(LogicalAnd)),
            Some(BatchIntKind::Signed)
        );
        assert_eq!(batch_int_kind(FunctionRef::TiPb(Lower)), None);
    }
}
