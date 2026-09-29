// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Closed ordinary-call demand facts, not a second function selector or graph.
//!
//! C3a admits identity-domain signed Int PLUS under the two row profiles. C3c
//! separately admits SQL native numeric-batch demand with stricter type facts;
//! its facts cannot be passed to the row-only constructor or compiler.
//! Consumer/origin labels are producer assertions. This layer validates the
//! closed source shape, not native consumer selection or protobuf ingestion.

use tidb_query_datatype::{FieldTypeTp, codec::data_type::ScalarValue};
use tipb::{FieldType, ScalarFuncSig};

use super::{CompileLimits, LocalError, LocalExpr, LocalResult, registry};
use crate::{CallMetadata, FunctionRef, LiteralKind};

/// The consumer's execution contract, not a property inferred from a function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrdinaryProfile {
    TypedRow,
    PbRow,
    /// Representable for explicit rejection; not admitted by C3a.
    AstValueScalar,
    /// Requires an operand-major scheduler; not admitted by C3a.
    NativeNumericBatch,
}

impl OrdinaryProfile {
    fn check_admitted(self) -> LocalResult<()> {
        match self {
            Self::TypedRow | Self::PbRow => Ok(()),
            Self::AstValueScalar | Self::NativeNumericBatch => Err(LocalError::InvalidSpec(
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

    /// Asserts SQL native numeric-batch demand, without a PB-origin assertion.
    /// Only `NumericBatchFacts` admits this label; row facts still reject it.
    pub const fn sql_native_numeric_batch(ordinal: usize, source: OrdinarySourceId) -> Self {
        Self {
            ordinal,
            source,
            profile: OrdinaryProfile::NativeNumericBatch,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeKind {
    Constant {
        value: Option<i64>,
        literal_kind: LiteralKind,
    },
    InputSlot {
        slot: usize,
    },
    /// Exactly TiPb PlusInt=203, arity 2, CallMetadata::None. All three facts
    /// are checked before this variant is produced, including during validate.
    PlusInt,
}

#[derive(Clone, Debug, PartialEq)]
struct NodeSnapshot {
    kind: NodeKind,
    field_type: FieldType,
}

/// Immutable row-only admission facts. Mixed TypedRow/PbRow children retain
/// their individual row demand; neither the consumer nor a child may use the
/// numeric-batch label. There is no conversion from NumericBatchFacts.
#[derive(Clone, Debug)]
pub struct OrdinaryProfileSpec {
    consumer: OrdinaryProfile,
    snapshot: ClosedInt203Snapshot,
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
        let snapshot = ClosedInt203Snapshot::capture(
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

/// Immutable SQL numeric-batch admission facts for closed signed Int203 trees.
///
/// Every call has a batch-only site; all nodes and schema fields satisfy the
/// strict batch type policy. Full source/schema snapshots are rechecked by the
/// batch compiler. The label asserts demand, not a proven native consumer path.
/// Leaf roots are allowed without sites; this does not claim that a native
/// numeric-batch entry is selected for every standalone constant or column.
/// These facts contain no executable graph, evaluator, cache or row-facts
/// escape.
#[derive(Clone, Debug)]
pub struct NumericBatchFacts {
    snapshot: ClosedInt203Snapshot,
    sites: Box<[OrdinaryCallSite]>,
}

impl NumericBatchFacts {
    pub fn sql_native_numeric_batch(
        spec: &LocalExpr,
        schema: &[FieldType],
        sites: Vec<OrdinaryCallSite>,
        limits: CompileLimits,
    ) -> LocalResult<Self> {
        if sites.len() > limits.max_nodes {
            return Err(resource(
                "numeric batch call-site count exceeds node budget",
            ));
        }
        if sites
            .windows(2)
            .any(|pair| pair[0].ordinal >= pair[1].ordinal)
        {
            return Err(invalid(
                "numeric batch call sites must have strictly increasing source ordinals",
            ));
        }
        let snapshot = ClosedInt203Snapshot::capture(
            spec,
            schema,
            limits,
            TypePolicy::SqlNativeNumericBatch,
            |nodes| check_batch_sites(nodes, &sites),
        )?;
        Ok(Self {
            snapshot,
            sites: sites.into_boxed_slice(),
        })
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
            .validate(spec, schema, limits, TypePolicy::SqlNativeNumericBatch)
    }

    pub(crate) fn site(&self, ordinal: usize) -> Option<&OrdinaryCallSite> {
        self.sites
            .binary_search_by_key(&ordinal, |site| site.ordinal)
            .ok()
            .map(|index| &self.sites[index])
    }
}

/// The shared representation is only an exact, flat source snapshot. Its fixed
/// PlusInt arity makes preorder sufficient to preserve shape. Clone/Debug/Drop
/// never recurse through a LocalExpr; neither public facade shares admission.
#[derive(Clone, Debug)]
struct ClosedInt203Snapshot {
    nodes: Box<[NodeSnapshot]>,
    schema: Box<[FieldType]>,
}

impl ClosedInt203Snapshot {
    fn capture(
        spec: &LocalExpr,
        schema: &[FieldType],
        limits: CompileLimits,
        policy: TypePolicy,
        check_sites: impl FnOnce(&[NodeSnapshot]) -> LocalResult<()>,
    ) -> LocalResult<Self> {
        if matches!(policy, TypePolicy::SqlNativeNumericBatch) {
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
        policy: TypePolicy,
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
        if node.kind != NodeKind::PlusInt {
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
        if node.kind != NodeKind::PlusInt {
            continue;
        }
        let site = sites
            .get(next)
            .ok_or_else(|| invalid("numeric batch is missing a call site"))?;
        if site.ordinal != ordinal {
            return Err(invalid(
                "numeric batch call site does not identify the next source call",
            ));
        }
        if site.profile != OrdinaryProfile::NativeNumericBatch
            || site.original_pb_signature.is_some()
        {
            return Err(invalid(
                "numeric batch calls require SQL batch sites without PB provenance",
            ));
        }
        next += 1;
    }
    if next != sites.len() {
        return Err(invalid("numeric batch contains an extra call site"));
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
enum TypePolicy {
    RowBaseline,
    SqlNativeNumericBatch,
}

// Keep raw protobuf flags: TiKV's FieldTypeFlag accessor names only a subset.
// The native TiDB flag inventory is rust/crates/tidb-datatype/src/field_type/
// mod.rs:91-149. C3c rejects UNSIGNED, ENUM, SET, PARSE_TO_JSON,
// ENUM_SET_AS_INT and undocumented bits; the remaining documented bits are
// retained verbatim.
const BATCH_EXCLUDED_FLAGS: u32 = (1 << 5) | (1 << 8) | (1 << 11) | (1 << 18) | (1 << 21);
const DOCUMENTED_FLAGS: u32 = (1 << 25) - 1;

fn check_type(field_type: &FieldType, policy: TypePolicy) -> LocalResult<()> {
    match policy {
        TypePolicy::RowBaseline => registry::check_signed_int_type(field_type)
            .map_err(|error| LocalError::InvalidSpec(error.to_string())),
        TypePolicy::SqlNativeNumericBatch => {
            if field_type.get_tp() != FieldTypeTp::LongLong as i32
                || field_type.get_array()
                || field_type.get_flag() & (BATCH_EXCLUDED_FLAGS | !DOCUMENTED_FLAGS) != 0
            {
                return Err(invalid(
                    "numeric batch requires non-array signed LongLong with supported raw flags",
                ));
            }
            Ok(())
        }
    }
}

fn describe(expr: &LocalExpr, schema: &[FieldType], policy: TypePolicy) -> LocalResult<NodeKind> {
    check_type(expr.field_type(), policy)?;
    match expr {
        LocalExpr::Constant {
            value,
            literal_kind,
            ..
        } => {
            let ScalarValue::Int(value) = value else {
                return Err(invalid("ordinary constants require the Int carrier"));
            };
            if *literal_kind != LiteralKind::Typed {
                return Err(invalid(
                    "ordinary constants require Typed literal provenance",
                ));
            }
            Ok(NodeKind::Constant {
                value: *value,
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
            if *function != FunctionRef::TiPb(ScalarFuncSig::PlusInt)
                || args.len() != 2
                || !matches!(metadata, CallMetadata::None)
            {
                return Err(invalid(
                    "ordinary C3a calls require exact PlusInt=203, arity 2 and no metadata",
                ));
            }
            Ok(NodeKind::PlusInt)
        }
        LocalExpr::HostCall { .. } => {
            Err(invalid("host calls are outside the ordinary C3a profile"))
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
    policy: TypePolicy,
}

impl<'a> Walk<'a> {
    fn new(
        spec: &'a LocalExpr,
        schema: &'a [FieldType],
        limits: CompileLimits,
        policy: TypePolicy,
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
        if let LocalExpr::Call { args, .. } = expr {
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
