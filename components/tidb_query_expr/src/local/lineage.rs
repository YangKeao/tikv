// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Checked SQL TypedRow control-result identity, not another executable graph.
//!
//! This first domain contains only Int/Bytes leaves, same-family selection
//! controls, and signed Int AND/OR. It does not compose the separate C3a
//! ordinary profile or admit PB, AST, batch, host, cast or variable-kind native
//! sources. SQL TypedRow origin is a whole-source producer assertion: these
//! facts check local consistency, not whether a native producer actually
//! ingested PB.
//!
//! The caller owns the matching immutable materialization table, complete
//! detached SQL types and fixed source-kind/collation contracts. It must check
//! those contracts at the same demanded read that supplies the carrier, and
//! separately bound source/literal/metadata bytes before constructing these
//! snapshots. CompileLimits bounds tree counts/depth, not arbitrary payload or
//! FieldType allocations. No native value is evaluated to construct facts.

use tidb_query_datatype::{
    FieldTypeTp,
    codec::data_type::{ScalarValue, VectorValue},
};
use tipb::{FieldType, ScalarFuncSig};

use super::{CompileLimits, LocalError, LocalExpr, LocalProgram, LocalResult};
use crate::{CallMetadata, FunctionRef, LiteralKind};

/// A name in the caller's materialization table, not a row, source-node ID,
/// executable handle, equal-value key or globally allocated identity.
///
/// The caller keeps the namespace/table/facts/program ownership together; equal
/// numeric keys from unrelated tables are not interchangeable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResultMetaId {
    unit: u64,
    record: u64,
}

impl ResultMetaId {
    pub const fn new(unit: u64, record: u64) -> Self {
        Self { unit, record }
    }

    pub const fn unit(self) -> u64 {
        self.unit
    }

    pub const fn record(self) -> u64 {
        self.record
    }
}

/// Representation only. Int does not distinguish native Int/UInt, and Bytes
/// does not distinguish native String/Bytes or carry a String's collation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineageCarrier {
    Int,
    Bytes,
}

/// The asserted producer occurrence, not a caller-chosen transfer policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlProducerRole {
    Constant,
    InputSlot,
    SelectedControl,
    ComputedBoolean,
}

/// One assertion for one ALL-NODE source-preorder occurrence. Root is ordinal0;
/// every argument, including leaves, follows left-to-right. A selected
/// control's ID names its generated-NULL fallback, not every value it can
/// select.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlProducerFact {
    ordinal: usize,
    id: ResultMetaId,
    carrier: LineageCarrier,
    role: ControlProducerRole,
}

impl ControlProducerFact {
    pub const fn constant(ordinal: usize, id: ResultMetaId, carrier: LineageCarrier) -> Self {
        Self {
            ordinal,
            id,
            carrier,
            role: ControlProducerRole::Constant,
        }
    }

    pub const fn input_slot(ordinal: usize, id: ResultMetaId, carrier: LineageCarrier) -> Self {
        Self {
            ordinal,
            id,
            carrier,
            role: ControlProducerRole::InputSlot,
        }
    }

    pub const fn selected_control(
        ordinal: usize,
        generated_null: ResultMetaId,
        carrier: LineageCarrier,
    ) -> Self {
        Self {
            ordinal,
            id: generated_null,
            carrier,
            role: ControlProducerRole::SelectedControl,
        }
    }

    pub const fn computed_boolean(ordinal: usize, own_result: ResultMetaId) -> Self {
        Self {
            ordinal,
            id: own_result,
            carrier: LineageCarrier::Int,
            role: ControlProducerRole::ComputedBoolean,
        }
    }

    pub const fn ordinal(&self) -> usize {
        self.ordinal
    }

    pub const fn id(&self) -> ResultMetaId {
        self.id
    }

    pub const fn carrier(&self) -> LineageCarrier {
        self.carrier
    }

    pub const fn role(&self) -> ControlProducerRole {
        self.role
    }
}

/// Compiler-only flow, derived from a checked producer and the actual source.
/// An outer selection forwards the child's current ID, including a computed or
/// generated boundary. This enum is not an independently executable graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckedResultFlow {
    Leaf {
        id: ResultMetaId,
        carrier: LineageCarrier,
    },
    PreserveSelected {
        generated_null: ResultMetaId,
        carrier: LineageCarrier,
    },
    OwnResult {
        id: ResultMetaId,
    },
}

impl CheckedResultFlow {
    pub(crate) fn carrier(self) -> LineageCarrier {
        match self {
            Self::Leaf { carrier, .. } | Self::PreserveSelected { carrier, .. } => carrier,
            Self::OwnResult { .. } => LineageCarrier::Int,
        }
    }

    pub(crate) fn own_id(self) -> ResultMetaId {
        match self {
            Self::Leaf { id, .. } | Self::OwnResult { id } => id,
            Self::PreserveSelected { generated_null, .. } => generated_null,
        }
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
    // Only the closed signatures below, with checked arity and None metadata.
    Control {
        signature: ScalarFuncSig,
        arity: usize,
    },
}

#[derive(Clone, Debug, PartialEq)]
struct NodeSnapshot {
    kind: NodeKind,
    field_type: FieldType,
}

/// Immutable exact flat source facts. Clone/Debug/Drop do not walk a recursive
/// LocalExpr; there are no child links, evaluator callbacks or kernel pointers.
#[derive(Clone, Debug)]
pub struct ControlLineageFacts {
    namespace: u64,
    nodes: Box<[NodeSnapshot]>,
    producers: Box<[ControlProducerFact]>,
    schema: Box<[FieldType]>,
}

impl ControlLineageFacts {
    /// Asserts SQL TypedRow at EVERY native source node, not only the consumer.
    /// There is no PB/AST/batch constructor. A FunctionRef::TiPb by itself does
    /// not establish PB origin, and C cannot authenticate a false native label.
    ///
    /// Producer records must cover every node exactly once in source-preorder
    /// order, use one namespace, and have distinct IDs. Sparse/high record IDs
    /// are valid: allocation sizes depend only on counts, never ID magnitudes.
    pub fn sql_typed_row(
        spec: &LocalExpr,
        schema: &[FieldType],
        producers: Vec<ControlProducerFact>,
        limits: CompileLimits,
    ) -> LocalResult<Self> {
        if limits.max_nodes == 0 || limits.max_depth == 0 {
            return Err(resource("lineage construction node/depth budget exceeded"));
        }
        if producers.len() > limits.max_nodes {
            return Err(resource("lineage producer count exceeds node budget"));
        }
        let namespace = check_producers(&producers)?;
        let mut walk = Walk::new(spec, schema, limits)?;
        let mut nodes = Vec::new();
        reserve(
            &mut nodes,
            producers.len(),
            "lineage snapshot allocation failed",
        )?;
        while let Some(description) = walk.next()? {
            let ordinal = nodes.len();
            let producer = producers
                .get(ordinal)
                .ok_or_else(|| invalid("lineage is missing a producer occurrence"))?;
            description.check_producer(producer)?;
            // Walk checked this node and newly scheduled children before any
            // literal/FieldType descriptor is cloned into the flat snapshot.
            nodes.push(description.snapshot());
        }
        if nodes.len() != producers.len() {
            return Err(invalid("lineage contains an extra producer occurrence"));
        }
        let mut copied_schema = Vec::new();
        reserve(
            &mut copied_schema,
            schema.len(),
            "lineage schema allocation failed",
        )?;
        copied_schema.extend_from_slice(schema);
        Ok(Self {
            namespace,
            nodes: nodes.into_boxed_slice(),
            producers: producers.into_boxed_slice(),
            schema: copied_schema.into_boxed_slice(),
        })
    }

    pub const fn namespace(&self) -> u64 {
        self.namespace
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn producers(&self) -> &[ControlProducerFact] {
        &self.producers
    }

    pub fn schema(&self) -> &[FieldType] {
        &self.schema
    }

    /// Check the current source and limits without cloning a second snapshot.
    /// Full FieldTypes, exact literal payloads, roles and schema remain checked
    /// even for descendants that will not be demanded during evaluation.
    pub(crate) fn revalidate(
        &self,
        spec: &LocalExpr,
        schema: &[FieldType],
        limits: CompileLimits,
    ) -> LocalResult<()> {
        if self.schema.as_ref() != schema {
            return Err(invalid("lineage schema differs from its exact snapshot"));
        }
        let mut walk = Walk::new(spec, schema, limits)?;
        let mut ordinal = 0;
        while let Some(description) = walk.next()? {
            let snapshot = self
                .nodes
                .get(ordinal)
                .ok_or_else(|| invalid("lineage source has additional nodes"))?;
            if !description.matches(snapshot) {
                return Err(invalid("lineage source differs from its exact snapshot"));
            }
            description.check_producer(&self.producers[ordinal])?;
            ordinal += 1;
        }
        if ordinal != self.nodes.len() {
            return Err(invalid("lineage source has missing nodes"));
        }
        Ok(())
    }

    pub(crate) fn flow(&self, ordinal: usize) -> Option<CheckedResultFlow> {
        let fact = self.producers.get(ordinal)?;
        Some(match fact.role {
            ControlProducerRole::Constant | ControlProducerRole::InputSlot => {
                CheckedResultFlow::Leaf {
                    id: fact.id,
                    carrier: fact.carrier,
                }
            }
            ControlProducerRole::SelectedControl => CheckedResultFlow::PreserveSelected {
                generated_null: fact.id,
                carrier: fact.carrier,
            },
            ControlProducerRole::ComputedBoolean => CheckedResultFlow::OwnResult { id: fact.id },
        })
    }
}

/// Ownership facade for a checked lineaged program. There is deliberately no
/// Deref, raw-RPN accessor or conversion that discards required result
/// identity. Compilation and evaluation methods are supplied by the shared
/// compiler and batch modules, not a second evaluator in this module.
#[derive(Debug)]
pub struct LocalControlProgram {
    pub(super) inner: LocalProgram,
}

impl LocalControlProgram {
    pub fn return_type(&self) -> &FieldType {
        self.inner.return_type()
    }
}

/// Values and materialization IDs in selection-occurrence order, not physical
/// row order. A successful evaluator supplies exactly one ID per value.
#[derive(Debug)]
pub struct LineagedBatch {
    pub(super) values: VectorValue,
    pub(super) result_metadata: Vec<ResultMetaId>,
}

impl LineagedBatch {
    pub fn values(&self) -> &VectorValue {
        &self.values
    }

    pub fn result_metadata(&self) -> &[ResultMetaId] {
        &self.result_metadata
    }

    pub fn into_parts(self) -> (VectorValue, Vec<ResultMetaId>) {
        (self.values, self.result_metadata)
    }
}

fn check_producers(producers: &[ControlProducerFact]) -> LocalResult<u64> {
    let namespace = producers
        .first()
        .ok_or_else(|| invalid("lineage requires a producer for its root"))?
        .id
        .unit;
    let mut ids = Vec::new();
    reserve(
        &mut ids,
        producers.len(),
        "lineage ID index allocation failed",
    )?;
    for (ordinal, producer) in producers.iter().enumerate() {
        if producer.ordinal != ordinal {
            return Err(invalid(
                "lineage producers must cover consecutive all-node source ordinals in order",
            ));
        }
        if producer.id.unit != namespace {
            return Err(invalid("lineage producer belongs to another namespace"));
        }
        ids.push(producer.id);
    }
    // Sort only this temporary ID index, never reorder or deduplicate caller
    // facts. Fixed-size reservation is bounded by the checked producer count.
    ids.sort_unstable();
    if ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid(
            "lineage producer IDs must be unique per occurrence",
        ));
    }
    Ok(namespace)
}

#[derive(Clone, Copy)]
enum Demand {
    Value(Option<LineageCarrier>),
    PredicateInt,
}

#[derive(Clone, Copy)]
enum ControlShape {
    If,
    IfNull,
    CaseWhen,
    Coalesce,
    Boolean,
}

impl ControlShape {
    fn child_demand(
        self,
        index: usize,
        arity: usize,
        demand: Demand,
        carrier: LineageCarrier,
    ) -> Demand {
        let predicate = match self {
            Self::If => index == 0,
            Self::CaseWhen => index % 2 == 0 && index + 1 < arity,
            Self::Boolean => true,
            Self::IfNull | Self::Coalesce => false,
        };
        if predicate || matches!(demand, Demand::PredicateInt) {
            Demand::PredicateInt
        } else {
            Demand::Value(Some(carrier))
        }
    }
}

// Raw SQL flag bits, checked without FieldTypeAccessor's from_bits_truncate.
// Source: TiDB rust/crates/tidb-datatype/src/field_type/mod.rs:91-149,
// FieldTypeFlags declares every bit 0..24 (NUM/GROUP share bit15). TiKV's
// FieldTypeFlag intentionally names fewer bits and is not this known-bit mask.
// ENUM/SET and ENUM_SET_AS_INT are not ordinary Int/Bytes provenance; JSON
// parsing is another conversion domain. Unknown flag bits are explicitly
// refused, never masked. All other documented bits remain in the complete FT.
const UNSIGNED: u32 = 1 << 5;
const EXCLUDED_FLAGS: u32 = (1 << 8) | (1 << 11) | (1 << 18) | (1 << 21);
const DOCUMENTED_FLAGS: u32 = (1 << 25) - 1;

fn check_type(field_type: &FieldType, demand: Demand) -> LocalResult<LineageCarrier> {
    if field_type.get_array() || field_type.get_flag() & (EXCLUDED_FLAGS | !DOCUMENTED_FLAGS) != 0 {
        return Err(invalid(
            "lineage does not admit ARRAY, hybrid, conversion or unknown field flags",
        ));
    }
    let carrier = match FieldTypeTp::from_i32(field_type.get_tp()) {
        Some(FieldTypeTp::LongLong) => LineageCarrier::Int,
        Some(
            FieldTypeTp::VarChar
            | FieldTypeTp::VarString
            | FieldTypeTp::String
            | FieldTypeTp::TinyBlob
            | FieldTypeTp::MediumBlob
            | FieldTypeTp::LongBlob
            | FieldTypeTp::Blob,
        ) => LineageCarrier::Bytes,
        _ => {
            return Err(invalid(
                "lineage field type is outside LongLong and the closed Bytes family",
            ));
        }
    };
    match demand {
        Demand::PredicateInt
            if carrier != LineageCarrier::Int || field_type.get_flag() & UNSIGNED != 0 =>
        {
            Err(invalid(
                "lineage predicates and booleans require signed LongLong throughout selected producers",
            ))
        }
        Demand::Value(Some(expected)) if carrier != expected => Err(invalid(
            "lineage value child is outside the declared result family",
        )),
        _ => Ok(carrier),
    }
}

fn control_shape(
    signature: ScalarFuncSig,
    arity: usize,
    carrier: LineageCarrier,
) -> LocalResult<ControlShape> {
    use ScalarFuncSig::*;
    let (shape, expected, valid_arity) = match signature {
        IfInt => (ControlShape::If, LineageCarrier::Int, arity == 3),
        IfString => (ControlShape::If, LineageCarrier::Bytes, arity == 3),
        IfNullInt => (ControlShape::IfNull, LineageCarrier::Int, arity == 2),
        IfNullString => (ControlShape::IfNull, LineageCarrier::Bytes, arity == 2),
        CaseWhenInt => (ControlShape::CaseWhen, LineageCarrier::Int, arity >= 2),
        CaseWhenString => (ControlShape::CaseWhen, LineageCarrier::Bytes, arity >= 2),
        CoalesceInt => (ControlShape::Coalesce, LineageCarrier::Int, arity >= 1),
        CoalesceString => (ControlShape::Coalesce, LineageCarrier::Bytes, arity >= 1),
        LogicalAnd | LogicalOr => (ControlShape::Boolean, LineageCarrier::Int, arity == 2),
        _ => {
            return Err(invalid(
                "lineage requires an exact admitted selection or boolean control signature",
            ));
        }
    };
    if !valid_arity || carrier != expected {
        return Err(invalid(
            "lineage control arity or declared result family is incorrect",
        ));
    }
    Ok(shape)
}

struct Description<'a> {
    expr: &'a LocalExpr,
    carrier: LineageCarrier,
    role: ControlProducerRole,
    control: Option<ControlShape>,
}

impl Description<'_> {
    fn check_producer(&self, fact: &ControlProducerFact) -> LocalResult<()> {
        if fact.role != self.role || fact.carrier != self.carrier {
            return Err(invalid(
                "lineage producer role or carrier differs from its source node",
            ));
        }
        Ok(())
    }

    fn snapshot(&self) -> NodeSnapshot {
        let kind = match self.expr {
            LocalExpr::Constant {
                value,
                literal_kind,
                ..
            } => NodeKind::Constant {
                value: value.clone(),
                literal_kind: *literal_kind,
            },
            LocalExpr::InputSlot { slot, .. } => NodeKind::InputSlot { slot: *slot },
            LocalExpr::Call {
                function: FunctionRef::TiPb(signature),
                args,
                ..
            } => NodeKind::Control {
                signature: *signature,
                arity: args.len(),
            },
            _ => unreachable!("description only contains checked source shapes"),
        };
        NodeSnapshot {
            kind,
            field_type: self.expr.field_type().clone(),
        }
    }

    fn matches(&self, snapshot: &NodeSnapshot) -> bool {
        if self.expr.field_type() != &snapshot.field_type {
            return false;
        }
        match (self.expr, &snapshot.kind) {
            (
                LocalExpr::Constant {
                    value,
                    literal_kind,
                    ..
                },
                NodeKind::Constant {
                    value: old_value,
                    literal_kind: old_kind,
                },
            ) => value == old_value && literal_kind == old_kind,
            (LocalExpr::InputSlot { slot, .. }, NodeKind::InputSlot { slot: old_slot }) => {
                slot == old_slot
            }
            (
                LocalExpr::Call {
                    function: FunctionRef::TiPb(signature),
                    args,
                    ..
                },
                NodeKind::Control {
                    signature: old_signature,
                    arity,
                },
            ) => signature == old_signature && args.len() == *arity,
            _ => false,
        }
    }
}

fn describe<'a>(
    expr: &'a LocalExpr,
    schema: &[FieldType],
    demand: Demand,
) -> LocalResult<Description<'a>> {
    let carrier = check_type(expr.field_type(), demand)?;
    let (role, control) = match expr {
        LocalExpr::Constant {
            value,
            literal_kind,
            ..
        } => {
            let admitted = match (value, carrier, literal_kind) {
                (ScalarValue::Int(_), LineageCarrier::Int, LiteralKind::Typed)
                | (ScalarValue::Bytes(_), LineageCarrier::Bytes, LiteralKind::Typed)
                | (ScalarValue::Bytes(Some(_)), LineageCarrier::Bytes, LiteralKind::Text) => true,
                _ => false,
            };
            if !admitted {
                return Err(invalid(
                    "lineage constants require their exact Int/Bytes carrier and admitted literal provenance",
                ));
            }
            (ControlProducerRole::Constant, None)
        }
        LocalExpr::InputSlot { slot, field_type } => {
            if schema.get(*slot) != Some(field_type) {
                return Err(invalid(
                    "lineage input slot differs from the complete schema",
                ));
            }
            (ControlProducerRole::InputSlot, None)
        }
        LocalExpr::Call {
            function,
            args,
            metadata,
            return_type,
        } => {
            let FunctionRef::TiPb(signature) = function else {
                return Err(invalid(
                    "local function IDs are outside the lineage control domain",
                ));
            };
            if !matches!(metadata, CallMetadata::None) {
                return Err(invalid("lineage controls require CallMetadata::None"));
            }
            let shape = control_shape(*signature, args.len(), carrier)?;
            let role = if matches!(shape, ControlShape::Boolean) {
                check_type(return_type, Demand::PredicateInt)?;
                ControlProducerRole::ComputedBoolean
            } else {
                ControlProducerRole::SelectedControl
            };
            (role, Some(shape))
        }
        LocalExpr::HostCall { .. } => {
            return Err(invalid("host calls are outside the lineage control domain"));
        }
    };
    Ok(Description {
        expr,
        carrier,
        role,
        control,
    })
}

struct Pending<'a> {
    expr: &'a LocalExpr,
    depth: usize,
    demand: Demand,
}

/// Bounded borrowed preorder traversal. Demand roles flow down the actual tree;
/// no executable child links or reconstructed tree are saved in the snapshot.
struct Walk<'a> {
    pending: Vec<Pending<'a>>,
    schema: &'a [FieldType],
    scheduled: usize,
    limits: CompileLimits,
}

impl<'a> Walk<'a> {
    fn new(
        spec: &'a LocalExpr,
        schema: &'a [FieldType],
        limits: CompileLimits,
    ) -> LocalResult<Self> {
        if limits.max_nodes == 0 || limits.max_depth == 0 {
            return Err(resource("lineage construction node/depth budget exceeded"));
        }
        for field_type in schema {
            check_type(field_type, Demand::Value(None))?;
        }
        let mut pending = Vec::new();
        reserve(&mut pending, 1, "lineage traversal allocation failed")?;
        pending.push(Pending {
            expr: spec,
            depth: 1,
            demand: Demand::Value(None),
        });
        Ok(Self {
            pending,
            schema,
            scheduled: 1,
            limits,
        })
    }

    fn next(&mut self) -> LocalResult<Option<Description<'a>>> {
        let Some(Pending {
            expr,
            depth,
            demand,
        }) = self.pending.pop()
        else {
            return Ok(None);
        };
        let description = describe(expr, self.schema, demand)?;
        if let LocalExpr::Call { args, .. } = expr {
            let child_depth = depth
                .checked_add(1)
                .ok_or_else(|| resource("lineage construction depth overflow"))?;
            self.scheduled = self
                .scheduled
                .checked_add(args.len())
                .ok_or_else(|| resource("lineage construction node count overflow"))?;
            if self.scheduled > self.limits.max_nodes || child_depth > self.limits.max_depth {
                return Err(resource("lineage construction node/depth budget exceeded"));
            }
            reserve(
                &mut self.pending,
                args.len(),
                "lineage traversal allocation failed",
            )?;
            let shape = description
                .control
                .expect("checked call has a control shape");
            for (index, arg) in args.iter().enumerate().rev() {
                self.pending.push(Pending {
                    expr: arg,
                    depth: child_depth,
                    demand: shape.child_demand(index, args.len(), demand, description.carrier),
                });
            }
        }
        Ok(Some(description))
    }
}

fn reserve<T>(values: &mut Vec<T>, additional: usize, message: &str) -> LocalResult<()> {
    values
        .try_reserve_exact(additional)
        .map_err(|_| resource(message))
}

fn invalid(message: &str) -> LocalError {
    LocalError::InvalidSpec(message.into())
}

fn resource(message: &str) -> LocalError {
    LocalError::ResourceLimit(message.into())
}
