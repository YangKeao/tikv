// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Staged tests for the agreed lineaged compiler/evaluator, deliberately
//! unwired until the parent releases integration and the datatype accounting
//! helpers. These test C's carriers/IDs, not native Datum materialization,
//! authenticated SQL/PB origin, BinaryLiteral admission or a hard
//! pre-allocation heap bound. The owning caller must separately prove actual
//! kind/collation contracts.

use std::{collections::HashSet, thread};

use tidb_query_datatype::{
    EvalType, FieldTypeTp,
    codec::data_type::{ChunkedVecBytes, ScalarValue, VectorValue},
    expr::{Error as EvalError, EvalContext},
};
use tikv_util::sys::thread::StdThreadBuildWrapper;
use tipb::{FieldType, ScalarFuncSig as Sig};

use super::{
    lineage::{CheckedResultFlow, *},
    *,
};

const UNIT: u64 = 0xfedc_ba98_7654_3210;

fn id(ordinal: usize) -> ResultMetaId {
    ResultMetaId::new(UNIT, ordinal as u64)
}
fn int_type() -> FieldType {
    FieldTypeTp::LongLong.into()
}
fn unsigned_type() -> FieldType {
    let mut field_type = int_type();
    field_type.set_flag(1 << 5);
    field_type
}
fn bytes_type(collation: i32) -> FieldType {
    let mut field_type: FieldType = FieldTypeTp::VarString.into();
    field_type.set_charset("binary".into());
    field_type.set_collate(collation);
    field_type
}
fn int(value: Option<i64>) -> LocalExpr {
    LocalExpr::Constant {
        value: ScalarValue::Int(value),
        field_type: int_type(),
        literal_kind: LiteralKind::Typed,
    }
}
fn unsigned(value: Option<i64>) -> LocalExpr {
    LocalExpr::Constant {
        value: ScalarValue::Int(value),
        field_type: unsigned_type(),
        literal_kind: LiteralKind::Typed,
    }
}
fn bytes(value: Option<&[u8]>) -> LocalExpr {
    LocalExpr::Constant {
        value: ScalarValue::Bytes(value.map(<[u8]>::to_vec)),
        field_type: bytes_type(63),
        literal_kind: LiteralKind::Typed,
    }
}
fn slot(index: usize, field_type: &FieldType) -> LocalExpr {
    LocalExpr::InputSlot {
        slot: index,
        field_type: field_type.clone(),
    }
}
fn call(signature: Sig, args: Vec<LocalExpr>, field_type: &FieldType) -> LocalExpr {
    LocalExpr::Call {
        function: FunctionRef::TiPb(signature),
        args: args.into_boxed_slice(),
        return_type: field_type.clone(),
        metadata: CallMetadata::None,
    }
}
fn int_call(signature: Sig, args: Vec<LocalExpr>) -> LocalExpr {
    call(signature, args, &int_type())
}
fn field_type_mut(expr: &mut LocalExpr) -> &mut FieldType {
    match expr {
        LocalExpr::Constant { field_type, .. } | LocalExpr::InputSlot { field_type, .. } => {
            field_type
        }
        LocalExpr::Call { return_type, .. } | LocalExpr::HostCall { return_type, .. } => {
            return_type
        }
    }
}
fn source_facts(spec: &LocalExpr) -> Vec<ControlProducerFact> {
    let mut pending = vec![spec];
    let mut result = Vec::new();
    while let Some(expr) = pending.pop() {
        let ordinal = result.len();
        let carrier = if expr.field_type().get_tp() == FieldTypeTp::LongLong as i32 {
            LineageCarrier::Int
        } else {
            LineageCarrier::Bytes
        };
        let fact = match expr {
            LocalExpr::Constant { .. } => {
                ControlProducerFact::constant(ordinal, id(ordinal), carrier)
            }
            LocalExpr::InputSlot { .. } => {
                ControlProducerFact::input_slot(ordinal, id(ordinal), carrier)
            }
            LocalExpr::Call {
                function: FunctionRef::TiPb(Sig::LogicalAnd | Sig::LogicalOr),
                ..
            } => ControlProducerFact::computed_boolean(ordinal, id(ordinal)),
            _ => ControlProducerFact::selected_control(ordinal, id(ordinal), carrier),
        };
        result.push(fact);
        if let LocalExpr::Call { args, .. } | LocalExpr::HostCall { args, .. } = expr {
            pending.extend(args.iter().rev());
        }
    }
    result
}
fn with_id(fact: ControlProducerFact, next: ResultMetaId) -> ControlProducerFact {
    match fact.role() {
        ControlProducerRole::Constant => {
            ControlProducerFact::constant(fact.ordinal(), next, fact.carrier())
        }
        ControlProducerRole::InputSlot => {
            ControlProducerFact::input_slot(fact.ordinal(), next, fact.carrier())
        }
        ControlProducerRole::SelectedControl => {
            ControlProducerFact::selected_control(fact.ordinal(), next, fact.carrier())
        }
        ControlProducerRole::ComputedBoolean => {
            ControlProducerFact::computed_boolean(fact.ordinal(), next)
        }
    }
}
fn facts(spec: &LocalExpr, schema: &[FieldType]) -> ControlLineageFacts {
    ControlLineageFacts::sql_typed_row(spec, schema, source_facts(spec), CompileLimits::default())
        .unwrap()
}
fn rejected(spec: &LocalExpr, schema: &[FieldType]) {
    assert!(matches!(
        ControlLineageFacts::sql_typed_row(
            spec,
            schema,
            source_facts(spec),
            CompileLimits::default()
        ),
        Err(LocalError::InvalidSpec(_))
    ));
}
fn compile(spec: &LocalExpr, schema: &[FieldType]) -> LocalControlProgram {
    compile_control_with_lineage(
        spec,
        schema,
        LocalCompileContext::default(),
        &facts(spec, schema),
    )
    .unwrap()
}
fn assert_batch(batch: &LineagedBatch, values: &[ScalarValue], metadata: &[ResultMetaId]) {
    assert_eq!(batch.values().len(), values.len());
    assert_eq!(batch.result_metadata(), metadata);
    assert_eq!(values.len(), metadata.len());
    for (index, value) in values.iter().enumerate() {
        assert_eq!(&batch.values().get_scalar_ref(index).to_owned(), value);
    }
}

#[test]
fn lineage_public_ids_producers_and_sparse_preorder_flow_are_exact() {
    let spec = int_call(
        Sig::IfInt,
        vec![
            slot(0, &int_type()),
            int_call(Sig::CoalesceInt, vec![int(None), slot(0, &int_type())]),
            int(Some(1)),
        ],
    );
    let records = [u64::MAX, 0, 1 << 63, 99, u64::MAX - 1, 42];
    let producers: Vec<_> = source_facts(&spec)
        .into_iter()
        .zip(records)
        .map(|(fact, record)| with_id(fact, ResultMetaId::new(UNIT, record)))
        .collect();
    let checked = ControlLineageFacts::sql_typed_row(
        &spec,
        &[int_type()],
        producers.clone(),
        CompileLimits::default(),
    )
    .unwrap();
    assert_eq!(checked.namespace(), UNIT);
    assert_eq!(checked.node_count(), 6);
    assert_eq!(checked.producers(), producers);
    assert_eq!(checked.schema(), &[int_type()]);
    let mut unique = HashSet::new();
    for (ordinal, producer) in checked.producers().iter().enumerate() {
        assert_eq!(producer.ordinal(), ordinal);
        assert_eq!(producer.id().unit(), UNIT);
        assert_eq!(producer.id().record(), records[ordinal]);
        assert_eq!(producer.carrier(), LineageCarrier::Int);
        assert!(unique.insert(producer.id()));
        let expected = if ordinal == 0 || ordinal == 2 {
            CheckedResultFlow::PreserveSelected {
                generated_null: producer.id(),
                carrier: LineageCarrier::Int,
            }
        } else {
            CheckedResultFlow::Leaf {
                id: producer.id(),
                carrier: LineageCarrier::Int,
            }
        };
        assert_eq!(checked.flow(ordinal), Some(expected));
    }
    assert!(checked.flow(6).is_none());
    assert!(checked.flow(usize::MAX).is_none());
    assert!(ResultMetaId::new(0, u64::MAX) < ResultMetaId::new(1, 0));
    checked
        .revalidate(&spec, &[int_type()], CompileLimits::default())
        .unwrap();
    let boolean = facts(
        &int_call(Sig::LogicalAnd, vec![int(None), int(Some(0))]),
        &[],
    );
    assert_eq!(
        boolean.producers()[0].role(),
        ControlProducerRole::ComputedBoolean
    );
    assert_eq!(
        boolean.flow(0),
        Some(CheckedResultFlow::OwnResult { id: id(0) })
    );
}

#[test]
fn lineage_rejects_missing_extra_unordered_duplicate_foreign_or_wrong_producers() {
    let spec = int_call(Sig::IfNullInt, vec![int(None), slot(0, &int_type())]);
    let base = source_facts(&spec);
    let mut reversed = base.clone();
    reversed.swap(1, 2);
    let mut duplicate_id = base.clone();
    duplicate_id[2] = with_id(duplicate_id[2], duplicate_id[1].id());
    let mut foreign = base.clone();
    foreign[1] = with_id(foreign[1], ResultMetaId::new(UNIT - 1, 1));
    let mut wrong_role = base.clone();
    wrong_role[1] = ControlProducerFact::input_slot(1, id(1), LineageCarrier::Int);
    let mut wrong_flow = base.clone();
    wrong_flow[0] = ControlProducerFact::computed_boolean(0, id(0));
    let mut wrong_carrier = base.clone();
    wrong_carrier[2] = ControlProducerFact::input_slot(2, id(2), LineageCarrier::Bytes);
    let mut duplicate_ordinal = base.clone();
    duplicate_ordinal[2] = ControlProducerFact::input_slot(1, id(2), LineageCarrier::Int);
    let mut extra = base.clone();
    extra.push(ControlProducerFact::constant(3, id(3), LineageCarrier::Int));
    for producers in [
        vec![],
        base[..2].to_vec(),
        reversed,
        duplicate_id,
        foreign,
        wrong_role,
        wrong_flow,
        wrong_carrier,
        duplicate_ordinal,
        extra,
    ] {
        assert!(matches!(
            ControlLineageFacts::sql_typed_row(
                &spec,
                &[int_type()],
                producers,
                CompileLimits::default(),
            ),
            Err(LocalError::InvalidSpec(_))
        ));
    }
    let boolean = int_call(Sig::LogicalOr, vec![int(Some(0)), int(None)]);
    let mut wrong = source_facts(&boolean);
    wrong[0] = ControlProducerFact::selected_control(0, id(0), LineageCarrier::Int);
    assert!(
        ControlLineageFacts::sql_typed_row(&boolean, &[], wrong, CompileLimits::default()).is_err()
    );
}

#[test]
fn lineage_closed_positive_signature_and_seven_bytes_type_matrix() {
    for spec in [
        int_call(
            Sig::IfInt,
            vec![int(Some(1)), unsigned(Some(-1)), int(None)],
        ),
        int_call(Sig::IfNullInt, vec![unsigned(None), int(Some(7))]),
        int_call(Sig::CaseWhenInt, vec![int(Some(0)), int(Some(1))]),
        int_call(
            Sig::CaseWhenInt,
            vec![int(None), int(Some(1)), unsigned(Some(-1))],
        ),
        int_call(Sig::CoalesceInt, vec![int(None)]),
        int_call(Sig::CoalesceInt, vec![int(None), unsigned(Some(-1))]),
        int_call(Sig::LogicalAnd, vec![int(Some(2)), int(None)]),
        int_call(Sig::LogicalOr, vec![int(None), int(Some(-1))]),
    ] {
        facts(&spec, &[])
            .revalidate(&spec, &[], CompileLimits::default())
            .unwrap();
    }
    for tp in [
        FieldTypeTp::VarChar,
        FieldTypeTp::VarString,
        FieldTypeTp::String,
        FieldTypeTp::TinyBlob,
        FieldTypeTp::MediumBlob,
        FieldTypeTp::LongBlob,
        FieldTypeTp::Blob,
    ] {
        let ft: FieldType = tp.into();
        for spec in [
            call(
                Sig::IfString,
                vec![int(Some(1)), bytes(Some(b"x")), slot(0, &ft)],
                &ft,
            ),
            call(Sig::IfNullString, vec![bytes(None), slot(0, &ft)], &ft),
            call(Sig::CaseWhenString, vec![int(None), bytes(Some(b"x"))], &ft),
            call(
                Sig::CaseWhenString,
                vec![int(Some(0)), bytes(Some(b"x")), slot(0, &ft)],
                &ft,
            ),
            call(Sig::CoalesceString, vec![slot(0, &ft)], &ft),
        ] {
            facts(&spec, &[ft.clone()])
                .revalidate(&spec, &[ft.clone()], CompileLimits::default())
                .unwrap();
        }
    }
    for leaf in [
        int(None),
        unsigned(Some(-1)),
        bytes(None),
        bytes(Some(&[0xff, 0, 0xfe])),
    ] {
        assert_eq!(facts(&leaf, &[]).node_count(), 1);
    }
}

#[test]
fn lineage_rejects_arity_metadata_other_functions_hosts_and_dead_type_errors() {
    for (sig, count) in [
        (Sig::IfInt, 2),
        (Sig::IfInt, 4),
        (Sig::IfNullInt, 1),
        (Sig::IfNullInt, 3),
        (Sig::CaseWhenInt, 0),
        (Sig::CaseWhenInt, 1),
        (Sig::CoalesceInt, 0),
        (Sig::LogicalAnd, 1),
        (Sig::LogicalOr, 3),
    ] {
        rejected(&int_call(sig, (0..count).map(|_| int(None)).collect()), &[]);
    }
    for sig in [
        Sig::PlusInt,
        Sig::PlusIntSignedSigned,
        Sig::AbsInt,
        Sig::CastIntAsString,
    ] {
        // Even an unreachable ordinary subtree is not a lineage composition.
        rejected(
            &int_call(
                Sig::IfInt,
                vec![
                    int(Some(0)),
                    int_call(sig, vec![int(Some(1)), int(Some(2))]),
                    int(Some(3)),
                ],
            ),
            &[],
        );
    }
    let mut metadata = int_call(Sig::IfNullInt, vec![int(None), int(Some(1))]);
    if let LocalExpr::Call { metadata, .. } = &mut metadata {
        *metadata = CallMetadata::InUnion { in_union: false };
    }
    rejected(&metadata, &[]);
    let mut local = int_call(Sig::IfNullInt, vec![int(None), int(Some(1))]);
    if let LocalExpr::Call { function, .. } = &mut local {
        *function = FunctionRef::Local(LocalFunctionId::NullIfIntSignedSigned);
    }
    rejected(&local, &[]);
    let catalog = HostCatalog::new(vec![HostSignature {
        arg_types: vec![int_type()].into_boxed_slice(),
        return_type: int_type(),
    }])
    .unwrap();
    rejected(
        &LocalExpr::HostCall {
            slot: catalog.slot(0).unwrap(),
            args: vec![int(Some(1))].into_boxed_slice(),
            return_type: int_type(),
        },
        &[],
    );
    rejected(
        &int_call(Sig::IfInt, vec![int(Some(1)), int(Some(7)), bytes(None)]),
        &[],
    );
    rejected(
        &call(
            Sig::IfString,
            vec![bytes(None), bytes(None), bytes(None)],
            &bytes_type(63),
        ),
        &[],
    );
    rejected(
        &call(
            Sig::IfNullInt,
            vec![bytes(None), bytes(None)],
            &bytes_type(63),
        ),
        &[],
    );
    rejected(&int_call(Sig::CoalesceString, vec![int(None)]), &[]);
    rejected(
        &int_call(Sig::CaseWhenInt, vec![int(Some(0)), int(None), bytes(None)]),
        &[],
    );
    rejected(
        &call(
            Sig::LogicalAnd,
            vec![int(None), int(None)],
            &unsigned_type(),
        ),
        &[],
    );
}

#[test]
fn lineage_literals_require_exact_carrier_and_non_binary_provenance() {
    for tp in [
        FieldTypeTp::Tiny,
        FieldTypeTp::Null,
        FieldTypeTp::Double,
        FieldTypeTp::NewDecimal,
        FieldTypeTp::DateTime,
        FieldTypeTp::Json,
        FieldTypeTp::Bit,
        FieldTypeTp::Enum,
        FieldTypeTp::Set,
        FieldTypeTp::TiDbVectorFloat32,
    ] {
        let mut expr = int(None);
        *field_type_mut(&mut expr) = tp.into();
        rejected(&expr, &[]);
    }
    for literal_kind in [LiteralKind::Text, LiteralKind::BinaryLiteral] {
        let mut expr = int(Some(1));
        if let LocalExpr::Constant {
            literal_kind: kind, ..
        } = &mut expr
        {
            *kind = literal_kind;
        }
        rejected(&expr, &[]);
    }
    for value in [Some(&b"abc"[..]), None] {
        let mut expr = bytes(value);
        if let LocalExpr::Constant { literal_kind, .. } = &mut expr {
            *literal_kind = LiteralKind::BinaryLiteral;
        }
        rejected(&expr, &[]);
    }
    let mut text = bytes(Some(&[0xff]));
    if let LocalExpr::Constant { literal_kind, .. } = &mut text {
        *literal_kind = LiteralKind::Text;
    }
    facts(&text, &[]);
    let mut text_null = bytes(None);
    if let LocalExpr::Constant { literal_kind, .. } = &mut text_null {
        *literal_kind = LiteralKind::Text;
    }
    rejected(&text_null, &[]);
    let mut wrong_carrier = bytes(None);
    if let LocalExpr::Constant { value, .. } = &mut wrong_carrier {
        *value = ScalarValue::Int(None);
    }
    rejected(&wrong_carrier, &[]);
    let mut wrong_carrier = int(None);
    if let LocalExpr::Constant { value, .. } = &mut wrong_carrier {
        *value = ScalarValue::Bytes(None);
    }
    rejected(&wrong_carrier, &[]);
}

#[test]
fn lineage_raw_known_flags_and_complete_fields_are_retained_not_truncated() {
    let mut ft = bytes_type(-46);
    // PRI_KEY, ZEROFILL, BIN_CMP and UNDERSCORE_CHARSET are documented native
    // flags absent from TiKV's smaller FieldTypeFlag bitflags declaration.
    let flags = (1 << 1) | (1 << 6) | (1 << 17) | (1 << 24);
    ft.set_flag(flags);
    ft.set_flen(123);
    ft.set_decimal(-1);
    ft.set_charset("ExactCharsetSpelling".into());
    ft.set_elems(vec!["retained metadata".into()].into());
    let spec = slot(0, &ft);
    let checked = facts(&spec, &[ft.clone(), int_type()]);
    assert_eq!(checked.schema()[0], ft);
    assert_eq!(checked.schema()[0].get_flag(), flags);
    for bit in [8, 11, 18, 21, 25, 31] {
        let mut excluded = ft.clone();
        excluded.set_flag(flags | (1 << bit));
        rejected(&slot(0, &excluded), &[excluded]);
    }
    let mut array = ft.clone();
    array.set_array(true);
    rejected(&slot(0, &array), &[array]);
    // Even unused schema entries are checked and retained in the snapshot.
    let mut invalid_unused = int_type();
    invalid_unused.set_flag(1 << 31);
    rejected(&int(Some(1)), &[invalid_unused]);
    for change in 0..6 {
        let mut changed = ft.clone();
        match change {
            0 => changed.set_flag(flags ^ (1 << 1)),
            1 => changed.set_flen(124),
            2 => changed.set_decimal(0),
            3 => changed.set_collate(-45),
            4 => changed.set_charset("different".into()),
            _ => changed.set_elems(vec!["changed".into()].into()),
        }
        assert!(matches!(
            checked.revalidate(
                &slot(0, &changed),
                &[changed, int_type()],
                CompileLimits::default()
            ),
            Err(LocalError::InvalidSpec(_))
        ));
    }
    assert!(
        checked
            .revalidate(&spec, &[ft, unsigned_type()], CompileLimits::default())
            .is_err()
    );
}

#[test]
fn lineage_snapshot_rejects_changed_payload_null_kind_signature_slot_or_order() {
    let original = bytes(Some(&[0, 0xff, 1]));
    let checked = facts(&original, &[]);
    for replacement in [bytes(None), bytes(Some(&[0, 0xff, 2])), bytes(Some(b""))] {
        assert!(
            checked
                .revalidate(&replacement, &[], CompileLimits::default())
                .is_err()
        );
    }
    let mut same_payload_new_kind = bytes(Some(&[0, 0xff, 1]));
    if let LocalExpr::Constant { literal_kind, .. } = &mut same_payload_new_kind {
        *literal_kind = LiteralKind::Text;
    }
    assert!(
        checked
            .revalidate(&same_payload_new_kind, &[], CompileLimits::default())
            .is_err()
    );
    let original = int_call(Sig::IfNullInt, vec![slot(0, &int_type()), int(Some(9))]);
    let schema = [int_type(), int_type()];
    let checked = facts(&original, &schema);
    let replacements = [
        int_call(Sig::IfNullInt, vec![slot(1, &int_type()), int(Some(9))]),
        int_call(Sig::IfNullInt, vec![slot(0, &int_type()), int(Some(8))]),
        int_call(Sig::IfNullInt, vec![int(Some(9)), slot(0, &int_type())]),
        int_call(Sig::CoalesceInt, vec![slot(0, &int_type()), int(Some(9))]),
        int_call(Sig::CoalesceInt, vec![slot(0, &int_type())]),
        slot(0, &int_type()),
    ];
    for replacement in replacements {
        assert!(matches!(
            checked.revalidate(&replacement, &schema, CompileLimits::default()),
            Err(LocalError::InvalidSpec(_))
        ));
        assert!(matches!(
            compile_control_with_lineage(
                &replacement,
                &schema,
                LocalCompileContext::default(),
                &checked,
            ),
            Err(LocalError::InvalidSpec(_))
        ));
    }
    let leaf = facts(&int(Some(1)), &[]);
    assert!(
        leaf.revalidate(&int(None), &[], CompileLimits::default())
            .is_err()
    );
    assert!(
        leaf.revalidate(
            &int_call(Sig::CoalesceInt, vec![int(Some(1))]),
            &[],
            CompileLimits::default()
        )
        .is_err()
    );
    rejected(&slot(1, &int_type()), &[int_type()]);
    rejected(&slot(0, &unsigned_type()), &[int_type()]);
}

#[test]
fn lineage_predicate_signedness_reaches_every_potential_selected_producer() {
    for make in 0..4 {
        let selected_unsigned = match make {
            0 => int_call(
                Sig::IfInt,
                vec![int(Some(0)), unsigned(Some(-1)), int(Some(0))],
            ),
            1 => int_call(Sig::IfNullInt, vec![unsigned(None), int(Some(0))]),
            2 => int_call(
                Sig::CaseWhenInt,
                vec![int(Some(0)), unsigned(Some(-1)), int(Some(0))],
            ),
            _ => int_call(Sig::CoalesceInt, vec![int(None), unsigned(Some(-1))]),
        };
        // Its signed result declaration alone does not prove signed source kind.
        facts(&selected_unsigned, &[]);
        rejected(
            &int_call(
                Sig::IfInt,
                vec![selected_unsigned, int(Some(1)), int(Some(0))],
            ),
            &[],
        );
    }
    rejected(
        &int_call(Sig::LogicalAnd, vec![unsigned(None), int(Some(1))]),
        &[],
    );
    rejected(
        &int_call(Sig::CaseWhenInt, vec![unsigned(Some(0)), int(Some(1))]),
        &[],
    );
    let signed_selection = int_call(Sig::IfNullInt, vec![int(None), int(Some(2))]);
    facts(
        &int_call(
            Sig::IfInt,
            vec![signed_selection, unsigned(Some(-1)), int(None)],
        ),
        &[],
    );
}

#[test]
fn lineage_limits_recheck_current_tree_and_official_ids_do_not_prove_pb() {
    let expr = call(
        Sig::IfNullString,
        vec![bytes(None), bytes(Some(b"x"))],
        &bytes_type(63),
    );
    let checked = facts(&expr, &[]);
    // SQL lowering also uses this official ID. This acceptance authenticates no
    // PB/native-origin claim; there is intentionally no alternate PB constructor.
    assert_eq!(checked.node_count(), 3);
    for limits in [
        CompileLimits {
            max_nodes: 0,
            max_depth: 0,
        },
        CompileLimits {
            max_nodes: 2,
            max_depth: 20,
        },
        CompileLimits {
            max_nodes: 20,
            max_depth: 1,
        },
    ] {
        assert!(matches!(
            checked.revalidate(&expr, &[], limits),
            Err(LocalError::ResourceLimit(_))
        ));
        assert!(matches!(
            ControlLineageFacts::sql_typed_row(&expr, &[], source_facts(&expr), limits),
            Err(LocalError::ResourceLimit(_))
        ));
    }
    let exact = CompileLimits {
        max_nodes: 3,
        max_depth: 2,
    };
    checked.revalidate(&expr, &[], exact).unwrap();
    // Existing entries remain narrower/different; no Int/Bytes or203 composition
    // can become admitted through the new immutable facts by default.
    assert!(compile_local(&expr, &[], LocalCompileContext::default()).is_err());
    assert!(
        OrdinaryProfileSpec::new(&expr, &[], OrdinaryProfile::TypedRow, vec![], exact).is_err()
    );
    assert!(OrdinaryProfileSpec::new(&expr, &[], OrdinaryProfile::PbRow, vec![], exact).is_err());
}

enum Reply {
    Value(VectorValue),
    Error(LocalError),
}
struct Bindings {
    schema: Vec<FieldType>,
    columns: Vec<Vec<ScalarValue>>,
    reads: Vec<(usize, InputRow)>,
    poisoned: Vec<usize>,
    reply_at: Option<(usize, Reply)>,
    warn: bool,
}
impl Bindings {
    fn new(schema: &[FieldType], columns: Vec<Vec<ScalarValue>>) -> Self {
        Self {
            schema: schema.to_vec(),
            columns,
            reads: Vec::new(),
            poisoned: Vec::new(),
            reply_at: None,
            warn: false,
        }
    }
}
impl LocalRuntimeServices for Bindings {
    fn binding_schema(&self) -> &[FieldType] {
        &self.schema
    }
    fn read_input(
        &mut self,
        ctx: &mut EvalContext,
        slot: usize,
        row: InputRow,
        expected: &FieldType,
    ) -> LocalResult<VectorValue> {
        assert_eq!(expected, &self.schema[slot]);
        self.reads.push((slot, row));
        if self.warn {
            ctx.warnings.append_warning(EvalError::Eval(
                format!("read:{slot}:{}:{}", row.occurrence, row.input_row),
                1234,
            ));
        }
        assert!(
            !self.poisoned.contains(&slot),
            "unselected binding was read"
        );
        if self
            .reply_at
            .as_ref()
            .is_some_and(|(at, _)| *at == self.reads.len())
        {
            return match self.reply_at.take().unwrap().1 {
                Reply::Value(value) => Ok(value),
                Reply::Error(error) => Err(error),
            };
        }
        Ok(VectorValue::from_scalar(
            &self.columns[slot][row.input_row],
            1,
        ))
    }
    fn host_services(&mut self) -> Option<&mut dyn LocalHostServices> {
        panic!("lineaged control consulted host services")
    }
}
fn run(
    program: &mut LocalControlProgram,
    services: &mut Bindings,
    physical_rows: usize,
    selection: &[usize],
) -> LineagedBatch {
    program
        .eval_with_bindings(
            ExecutionLimits::default(),
            &mut EvalContext::default(),
            physical_rows,
            selection,
            services,
        )
        .unwrap()
}
fn resource_run(
    program: &mut LocalControlProgram,
    services: &mut Bindings,
    selection: &[usize],
    bytes: usize,
    ctx: &mut EvalContext,
) -> LocalError {
    let state = ExecutionLimits {
        max_retained_bytes: bytes,
        ..ExecutionLimits::default()
    };
    let report = program
        .eval_with_bindings(state, ctx, 1, selection, services)
        .unwrap_err();
    assert!(matches!(report, LocalError::ResourceLimit(_)));
    report
}

#[test]
fn lineage_uint_bits_selected_under_signed_result_keep_source_id() {
    let schema = [int_type(), unsigned_type()];
    let spec = int_call(
        Sig::IfInt,
        vec![slot(0, &schema[0]), slot(1, &schema[1]), int(Some(-1))],
    );
    let mut program = compile(&spec, &schema);
    assert_eq!(program.return_type(), &int_type());
    let mut services = Bindings::new(
        &schema,
        vec![
            vec![ScalarValue::Int(Some(1)), ScalarValue::Int(Some(0))],
            vec![ScalarValue::Int(Some(-1)), ScalarValue::Int(Some(-1))],
        ],
    );
    let output = run(&mut program, &mut services, 2, &[0, 1, 0]);
    assert_batch(
        &output,
        &[
            ScalarValue::Int(Some(-1)),
            ScalarValue::Int(Some(-1)),
            ScalarValue::Int(Some(-1)),
        ],
        &[id(2), id(3), id(2)],
    );
    assert_eq!(
        services.reads,
        vec![
            (
                0,
                InputRow {
                    occurrence: 0,
                    input_row: 0
                }
            ),
            (
                1,
                InputRow {
                    occurrence: 0,
                    input_row: 0
                }
            ),
            (
                0,
                InputRow {
                    occurrence: 1,
                    input_row: 1
                }
            ),
            (
                0,
                InputRow {
                    occurrence: 2,
                    input_row: 0
                }
            ),
            (
                1,
                InputRow {
                    occurrence: 2,
                    input_row: 0
                }
            ),
        ]
    );
    let (values, ids) = output.into_parts();
    assert_eq!(values.to_int_vec(), vec![Some(-1); 3]);
    assert_eq!(ids, vec![id(2), id(3), id(2)]);
    // The caller's record at id2 reconstructs UInt(MAX); id3 reconstructs
    // Int(-1). C does not perform the native materialization or infer kind
    // from these bits.
}

#[test]
fn lineage_identical_raw_bytes_keep_selected_records_and_declared_type_separate() {
    let declaration = bytes_type(-45);
    let mut source_type = bytes_type(-46);
    source_type.set_tp(FieldTypeTp::Blob as i32);
    source_type.set_flag(1 << 7);
    let schema = [int_type(), source_type.clone()];
    let raw = [0xff, 0, 0xc0, 0x80];
    let spec = call(
        Sig::IfString,
        vec![slot(0, &schema[0]), slot(1, &schema[1]), bytes(Some(&raw))],
        &declaration,
    );
    let mut program = compile(&spec, &schema);
    assert_eq!(program.return_type(), &declaration);
    let mut services = Bindings::new(
        &schema,
        vec![
            vec![ScalarValue::Int(Some(1)), ScalarValue::Int(Some(0))],
            vec![ScalarValue::Bytes(Some(raw.to_vec())); 2],
        ],
    );
    let output = run(&mut program, &mut services, 2, &[0, 1]);
    let expected = vec![ScalarValue::Bytes(Some(raw.to_vec())); 2];
    assert_batch(&output, &expected, &[id(2), id(3)]);
    // D's id2 is String(collation A) for the binary/blob Chunk accessor; id3
    // may be ordinary Bytes. Neither payload equality nor declared
    // collation B rewrites those IDs. Native kinds/collations themselves
    // belong to D's tests.
}

#[test]
fn lineage_selected_and_generated_null_ids_are_distinct_for_both_carriers() {
    for carrier in [LineageCarrier::Int, LineageCarrier::Bytes] {
        let (if_sig, ifnull_sig, case_sig, coalesce_sig, ft, null) = match carrier {
            LineageCarrier::Int => (
                Sig::IfInt,
                Sig::IfNullInt,
                Sig::CaseWhenInt,
                Sig::CoalesceInt,
                int_type(),
                ScalarValue::Int(None),
            ),
            LineageCarrier::Bytes => (
                Sig::IfString,
                Sig::IfNullString,
                Sig::CaseWhenString,
                Sig::CoalesceString,
                bytes_type(63),
                ScalarValue::Bytes(None),
            ),
        };
        let nil = || LocalExpr::Constant {
            value: null.clone(),
            field_type: ft.clone(),
            literal_kind: LiteralKind::Typed,
        };
        for (spec, selected) in [
            (call(if_sig, vec![int(Some(1)), nil(), nil()], &ft), id(2)),
            (call(ifnull_sig, vec![nil(), nil()], &ft), id(2)),
            (call(case_sig, vec![int(Some(0)), nil()], &ft), id(0)),
            (call(case_sig, vec![int(Some(1)), nil()], &ft), id(2)),
            (call(case_sig, vec![int(Some(0)), nil(), nil()], &ft), id(3)),
            (call(coalesce_sig, vec![nil(), nil()], &ft), id(0)),
            (
                call(
                    if_sig,
                    vec![
                        int(Some(1)),
                        call(coalesce_sig, vec![nil(), nil()], &ft),
                        nil(),
                    ],
                    &ft,
                ),
                id(2),
            ),
        ] {
            let mut program = compile(&spec, &[]);
            let output = run(&mut program, &mut Bindings::new(&[], vec![]), 1, &[0]);
            assert_batch(&output, &[null.clone()], &[selected]);
        }
    }
}

#[test]
fn lineage_boolean_results_and_outer_selections_keep_current_computed_id() {
    for signature in [Sig::LogicalAnd, Sig::LogicalOr] {
        for left in [None, Some(0), Some(2)] {
            for right in [None, Some(0), Some(-1)] {
                let expected = match signature {
                    Sig::LogicalAnd if left == Some(0) || right == Some(0) => Some(0),
                    Sig::LogicalOr
                        if left.is_some_and(|v| v != 0) || right.is_some_and(|v| v != 0) =>
                    {
                        Some(1)
                    }
                    _ if left.is_none() || right.is_none() => None,
                    Sig::LogicalAnd => Some(1),
                    _ => Some(0),
                };
                let boolean = int_call(signature, vec![int(left), int(right)]);
                let spec = int_call(Sig::IfInt, vec![int(Some(1)), boolean, int(None)]);
                let mut program = compile(&spec, &[]);
                assert_batch(
                    &run(&mut program, &mut Bindings::new(&[], vec![]), 1, &[0]),
                    &[ScalarValue::Int(expected)],
                    &[id(2)],
                );
            }
        }
    }
}

#[test]
fn lineage_controls_demand_each_predicate_once_and_skip_poisoned_values() {
    let ft = bytes_type(63);
    let schema = [int_type(), ft.clone(), ft.clone()];
    let spec = call(
        Sig::CaseWhenString,
        vec![
            slot(0, &schema[0]),
            slot(1, &ft),
            int(Some(1)),
            slot(2, &ft),
        ],
        &ft,
    );
    let mut program = compile(&spec, &schema);
    let mut services = Bindings::new(
        &schema,
        vec![
            vec![ScalarValue::Int(None)],
            vec![ScalarValue::Bytes(None)],
            vec![ScalarValue::Bytes(Some(b"selected".to_vec()))],
        ],
    );
    services.poisoned.push(1);
    assert_batch(
        &run(&mut program, &mut services, 1, &[0]),
        &[ScalarValue::Bytes(Some(b"selected".to_vec()))],
        &[id(4)],
    );
    assert_eq!(
        services.reads,
        vec![
            (
                0,
                InputRow {
                    occurrence: 0,
                    input_row: 0
                }
            ),
            (
                2,
                InputRow {
                    occurrence: 0,
                    input_row: 0
                }
            )
        ]
    );
    let spec = call(Sig::CoalesceString, vec![slot(1, &ft), slot(2, &ft)], &ft);
    let mut program = compile(&spec, &schema);
    services.reads.clear();
    services.poisoned = vec![2];
    services.columns[1][0] = ScalarValue::Bytes(Some(Vec::new()));
    assert_batch(
        &run(&mut program, &mut services, 1, &[0]),
        &[ScalarValue::Bytes(Some(Vec::new()))],
        &[id(1)],
    );
    assert_eq!(services.reads.len(), 1);
}

#[test]
fn lineage_selection_occurrences_align_ids_without_physical_row_memoization() {
    let ft = bytes_type(63);
    let schema = [int_type(), ft.clone(), ft.clone()];
    let spec = call(
        Sig::IfString,
        vec![slot(0, &schema[0]), slot(1, &ft), slot(2, &ft)],
        &ft,
    );
    let mut program = compile(&spec, &schema);
    let mut services = Bindings::new(
        &schema,
        vec![
            vec![
                ScalarValue::Int(Some(1)),
                ScalarValue::Int(Some(0)),
                ScalarValue::Int(Some(1)),
            ],
            vec![
                ScalarValue::Bytes(Some(vec![10])),
                ScalarValue::Bytes(Some(vec![11])),
                ScalarValue::Bytes(Some(vec![12])),
            ],
            vec![
                ScalarValue::Bytes(Some(vec![20])),
                ScalarValue::Bytes(Some(vec![21])),
                ScalarValue::Bytes(Some(vec![22])),
            ],
        ],
    );
    for selection in [
        vec![],
        vec![1],
        (0..1024).map(|i| i % 3).collect(),
        (0..1025).map(|i| i % 3).collect(),
        vec![2, 1, 0],
        vec![2, 0, 2],
    ] {
        services.reads.clear();
        let output = run(&mut program, &mut services, 3, &selection);
        let mut expected = Vec::new();
        let mut ids = Vec::new();
        for (occurrence, &row) in selection.iter().enumerate() {
            let selected_slot = if row == 1 { 2 } else { 1 };
            expected.push(services.columns[selected_slot][row].clone());
            ids.push(id(selected_slot + 1));
            assert_eq!(
                services.reads[2 * occurrence],
                (
                    0,
                    InputRow {
                        occurrence,
                        input_row: row
                    }
                )
            );
            assert_eq!(
                services.reads[2 * occurrence + 1],
                (
                    selected_slot,
                    InputRow {
                        occurrence,
                        input_row: row
                    }
                )
            );
        }
        assert_batch(&output, &expected, &ids);
        assert_eq!(services.reads.len(), 2 * selection.len());
    }
}

#[test]
fn lineage_demanded_contract_errors_keep_raw_error_and_warning_set() {
    let ft = bytes_type(-46);
    let schema = [int_type(), ft.clone()];
    let spec = call(
        Sig::IfString,
        vec![slot(0, &schema[0]), slot(1, &ft), bytes(Some(b"safe"))],
        &ft,
    );
    let mut program = compile(&spec, &schema);
    let mut services = Bindings::new(
        &schema,
        vec![
            vec![ScalarValue::Int(Some(0)), ScalarValue::Int(Some(1))],
            vec![ScalarValue::Bytes(None); 2],
        ],
    );
    services.warn = true;
    // Models D detecting wrong non-NULL native kind/collation on its SAME read.
    services.reply_at = Some((
        3,
        Reply::Error(LocalError::BindingContract(
            "non-NULL source kind/collation differs".into(),
        )),
    ));
    let mut ctx = EvalContext::default();
    let report = program
        .eval_with_bindings(
            ExecutionLimits::default(),
            &mut ctx,
            2,
            &[0, 1, 0],
            &mut services,
        )
        .unwrap_err();
    assert!(matches!(report, LocalError::BindingContract(_)));
    assert_eq!(services.reads.len(), 3);
    assert_eq!(ctx.warnings.warning_cnt, 3);
    assert_eq!(ctx.warnings.warnings.len(), 3);
    services.reads.clear();
    let output = run(&mut program, &mut services, 2, &[0]);
    assert_batch(
        &output,
        &[ScalarValue::Bytes(Some(b"safe".to_vec()))],
        &[id(3)],
    );
    assert_eq!(services.reads.len(), 1);
}

#[test]
fn lineage_malformed_replies_and_preflight_failures_remain_raw() {
    let ft = bytes_type(63);
    let mut program = compile(&slot(0, &ft), &[ft.clone()]);
    let mut services = Bindings::new(&[ft.clone()], vec![vec![ScalarValue::Bytes(None)]]);
    for reply in [
        VectorValue::from_scalar(&ScalarValue::Int(None), 1),
        VectorValue::with_capacity(0, EvalType::Bytes),
        VectorValue::from_scalar(&ScalarValue::Bytes(None), 2),
    ] {
        services.reads.clear();
        services.reply_at = Some((1, Reply::Value(reply)));
        let report = program
            .eval_with_bindings(
                ExecutionLimits::default(),
                &mut EvalContext::default(),
                1,
                &[0, 0],
                &mut services,
            )
            .unwrap_err();
        assert!(matches!(report, LocalError::BindingContract(_)));
        assert_eq!(services.reads.len(), 1);
    }
    services.reads.clear();
    for selection in [vec![1], vec![0, 1]] {
        let report = program
            .eval_with_bindings(
                ExecutionLimits::default(),
                &mut EvalContext::default(),
                1,
                &selection,
                &mut services,
            )
            .unwrap_err();
        assert!(matches!(report, LocalError::InvalidBatch(_)));
        assert!(services.reads.is_empty());
    }
    services.schema[0].set_collate(-45);
    let report = program
        .eval_with_bindings(
            ExecutionLimits::default(),
            &mut EvalContext::default(),
            1,
            &[],
            &mut services,
        )
        .unwrap_err();
    assert!(matches!(report, LocalError::InvalidBatch(_)));
    assert!(services.reads.is_empty());
    services.schema[0] = ft;
    assert!(run(&mut program, &mut services, 1, &[]).values().is_empty());
}

#[test]
fn lineage_empty_bytes_sentinel_and_fixed_scaffolding_obey_resource_limits() {
    let ft = bytes_type(63);
    let mut program = compile(&slot(0, &ft), &[ft.clone()]);
    let mut services = Bindings::new(&[ft], vec![vec![ScalarValue::Bytes(None)]]);
    // Empty Bytes still owns an offset sentinel; refusal may occur without a
    // callback. No hard allocator-peak or zero-allocation claim is made here.
    resource_run(
        &mut program,
        &mut services,
        &[],
        0,
        &mut EvalContext::default(),
    );
    assert!(services.reads.is_empty());
    resource_run(
        &mut program,
        &mut services,
        &[0],
        0,
        &mut EvalContext::default(),
    );
    assert!(services.reads.is_empty());
    assert_batch(&run(&mut program, &mut services, 1, &[]), &[], &[]);
}

#[test]
fn lineage_oversized_provider_capacity_is_rejected_after_that_read_not_before() {
    let ft = bytes_type(63);
    let mut program = compile(&slot(0, &ft), &[ft.clone()]);
    let mut services = Bindings::new(&[ft], vec![vec![ScalarValue::Bytes(None)]]);
    let mut oversized = ChunkedVecBytes::try_with_capacities(1, 1024 * 1024).unwrap();
    oversized.push_ref(Some(b""));
    assert!(oversized.retained_heap_bytes().unwrap() >= 1024 * 1024);
    services.reply_at = Some((1, Reply::Value(VectorValue::Bytes(oversized))));
    services.warn = true;
    let mut ctx = EvalContext::default();
    resource_run(&mut program, &mut services, &[0, 0], 64 * 1024, &mut ctx);
    assert_eq!(
        services.reads,
        vec![(
            0,
            InputRow {
                occurrence: 0,
                input_row: 0
            }
        )]
    );
    assert_eq!(ctx.warnings.warning_cnt, 1);
    assert_eq!(ctx.warnings.warnings.len(), 1);
}

#[test]
fn lineage_output_payload_growth_stops_later_occurrences_and_final_publication() {
    let ft = bytes_type(63);
    let mut program = compile(&slot(0, &ft), &[ft.clone()]);
    let mut services = Bindings::new(
        &[ft],
        vec![vec![ScalarValue::Bytes(Some(vec![0xff; 4096]))]],
    );
    services.warn = true;
    let mut ctx = EvalContext::default();
    resource_run(
        &mut program,
        &mut services,
        &vec![0; 128],
        64 * 1024,
        &mut ctx,
    );
    let reads = services.reads.len();
    assert!(
        reads > 1 && reads < 128,
        "output growth, not initial scaffolding, must stop this prefix"
    );
    assert_eq!(ctx.warnings.warning_cnt, reads);
    for (occurrence, &(slot, row)) in services.reads.iter().enumerate() {
        assert_eq!(slot, 0);
        assert_eq!(
            row,
            InputRow {
                occurrence,
                input_row: 0
            }
        );
    }
    // The same denied growth on the final selected occurrence must not publish
    // a successful partial result. The incoming callback itself already ran.
    services.reads.clear();
    resource_run(
        &mut program,
        &mut services,
        &vec![0; reads],
        64 * 1024,
        &mut EvalContext::default(),
    );
    assert_eq!(services.reads.len(), reads);
}

#[test]
fn lineage_empty_and_null_bytes_still_charge_offsets_bitmap_and_id_capacity() {
    for payload in [None, Some(Vec::new())] {
        let ft = bytes_type(63);
        let mut program = compile(&slot(0, &ft), &[ft.clone()]);
        let mut services = Bindings::new(&[ft], vec![vec![ScalarValue::Bytes(payload)]]);
        let rows = 16_384usize;
        let ids = rows * std::mem::size_of::<ResultMetaId>();
        let offsets = (rows + 1) * std::mem::size_of::<usize>();
        let bitmap = rows.div_ceil(64) * std::mem::size_of::<u64>();
        // Full-N scaffolding precedes effects. IDs alone exceed128KiB; the
        // other caps independently exercise the offset and bitmap minima.
        // These are required layout lower bounds, NOT exact Vec capacities.
        for cap in [128 * 1024, ids + offsets - 1, ids + offsets + bitmap - 1] {
            resource_run(
                &mut program,
                &mut services,
                &vec![0; rows],
                cap,
                &mut EvalContext::default(),
            );
            assert!(services.reads.is_empty());
        }
        let output = program
            .eval_with_bindings(
                ExecutionLimits {
                    max_retained_bytes: 1024 * 1024,
                    ..ExecutionLimits::default()
                },
                &mut EvalContext::default(),
                1,
                &vec![0; rows],
                &mut services,
            )
            .unwrap();
        assert_eq!(output.values().len(), rows);
        assert_eq!(output.result_metadata().len(), rows);
        assert_eq!(services.reads.len(), rows);
    }
}

#[test]
fn lineage_unselected_large_constant_is_not_materialized_or_reinterpreted() {
    let ft = bytes_type(63);
    let schema = [int_type()];
    let huge = vec![0xff; 1024 * 1024];
    let spec = call(
        Sig::IfString,
        vec![
            slot(0, &schema[0]),
            bytes(Some(&huge)),
            bytes(Some(b"small")),
        ],
        &ft,
    );
    let mut program = compile(&spec, &schema);
    let mut services = Bindings::new(&schema, vec![vec![ScalarValue::Int(Some(0))]]);
    let state = ExecutionLimits {
        max_retained_bytes: 64 * 1024,
        ..ExecutionLimits::default()
    };
    let output = program
        .eval_with_bindings(state, &mut EvalContext::default(), 1, &[0], &mut services)
        .unwrap();
    assert_batch(
        &output,
        &[ScalarValue::Bytes(Some(b"small".to_vec()))],
        &[id(3)],
    );
    assert_eq!(services.reads.len(), 1);
    services.reads.clear();
    services.columns[0][0] = ScalarValue::Int(Some(1));
    resource_run(
        &mut program,
        &mut services,
        &[0, 0],
        64 * 1024,
        &mut EvalContext::default(),
    );
    assert_eq!(services.reads.len(), 1);
}

#[test]
fn lineage_deep_facts_compile_eval_metadata_and_drop_remain_iterative() {
    thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn_wrapper(|| {
            for depth in [33, 64, 256] {
                let ft = bytes_type(63);
                let mut spec = bytes(Some(&[0xff]));
                for _ in 1..depth {
                    spec = call(Sig::CoalesceString, vec![spec], &ft);
                }
                let checked = facts(&spec, &[]);
                assert_eq!(checked.node_count(), depth);
                let flat_clone = checked.clone();
                flat_clone
                    .revalidate(&spec, &[], CompileLimits::default())
                    .unwrap();
                let mut program = compile_control_with_lineage(
                    &spec,
                    &[],
                    LocalCompileContext::default(),
                    &checked,
                )
                .unwrap();
                assert_eq!(program.return_type(), &ft);
                // Crate-private inspection only; the public facade has no RPN
                // escape that can discard its required materialization IDs.
                assert_eq!(program.inner.expression.node_count(), depth);
                assert_eq!(program.inner.expression.work_count(), depth);
                let output = run(&mut program, &mut Bindings::new(&[], vec![]), 1, &[0]);
                assert_batch(
                    &output,
                    &[ScalarValue::Bytes(Some(vec![0xff]))],
                    &[id(depth - 1)],
                );
                drop(output);
                drop(program);
                drop(flat_clone);
                drop(checked);
                drop(spec);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
