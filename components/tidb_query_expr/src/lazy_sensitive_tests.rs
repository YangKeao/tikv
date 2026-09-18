// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Keeps [`crate::LAZY_SENSITIVE_KERNELS`] and the dispatcher in step.
//!
//! The embedder trusts [`crate::LAZY_SENSITIVE_KERNELS`] to decide whether a
//! compiled program is safe to admit. That is only sound if every signature in
//! the lazy-sensitive control/logical families that this engine dispatches is
//! actually registered with a lazy kernel: a single eager arm would let the
//! embedder admit a shape whose unused branch MySQL never enters.
//!
//! The metas are obtained through the real dispatcher
//! ([`crate::map_expr_node_to_rpn_func`]) with a synthetic `Expr` of the right
//! arity and field types, so adding or changing a dispatcher arm is observed
//! here. Grepping the source would not catch a mapper change that routes a
//! family to a different kernel.

use std::collections::BTreeSet;

use protobuf::ProtobufEnum;
use tidb_query_datatype::FieldTypeTp;
use tipb::{Expr, FieldType, ScalarFuncSig};
use tipb_helper::ExprDefBuilder as E;

use crate::{LAZY_SENSITIVE_KERNELS, map_expr_node_to_rpn_func};

/// Field type used for the value children and the return type of a
/// lazy-sensitive signature, selected by the signature's type suffix.
fn value_field_type(suffix: &str) -> FieldType {
    match suffix {
        "Int" => FieldTypeTp::LongLong,
        "Real" => FieldTypeTp::Double,
        "Decimal" => FieldTypeTp::NewDecimal,
        "Time" => FieldTypeTp::DateTime,
        "String" => FieldTypeTp::VarChar,
        "Duration" => FieldTypeTp::Duration,
        "Json" => FieldTypeTp::Json,
        "VectorFloat32" => FieldTypeTp::TiDbVectorFloat32,
        other => panic!("unknown lazy-sensitive type suffix {other}"),
    }
    .into()
}

/// Classifies a `ScalarFuncSig` debug name as a lazy-sensitive control/logical
/// family member, returning the family and its type suffix. `None` for every
/// other signature.
fn classify(name: &str) -> Option<(&'static str, &str)> {
    // `IfNull` must be tested before `If`.
    const FAMILIES: [&str; 4] = ["IfNull", "If", "Coalesce", "CaseWhen"];
    for family in FAMILIES {
        if let Some(suffix) = name.strip_prefix(family) {
            return Some((family, suffix));
        }
    }
    matches!(name, "LogicalAnd" | "LogicalOr" | "LogicalXor").then_some(("Logical", ""))
}

/// A minimal expression tree with the argument count and field types the
/// signature's generated validator requires. Only the dispatcher is exercised
/// here, so the children are null/int constants rather than full
/// sub-expressions.
fn synthetic(sig: ScalarFuncSig, family: &str, suffix: &str) -> Expr {
    if family == "Logical" {
        let ft: FieldType = FieldTypeTp::LongLong.into();
        return E::scalar_func(sig, ft.clone())
            .push_child(E::constant_int(1))
            .push_child(E::constant_int(0))
            .build();
    }
    let ft = value_field_type(suffix);
    let value = || E::constant_null(ft.clone());
    let condition = || E::constant_int(1);
    match family {
        "IfNull" => E::scalar_func(sig, ft.clone())
            .push_child(value())
            .push_child(value())
            .build(),
        "If" => E::scalar_func(sig, ft.clone())
            .push_child(condition())
            .push_child(value())
            .push_child(value())
            .build(),
        "Coalesce" => E::scalar_func(sig, ft.clone())
            .push_child(value())
            .push_child(value())
            .push_child(value())
            .build(),
        "CaseWhen" => E::scalar_func(sig, ft.clone())
            .push_child(condition())
            .push_child(value())
            .push_child(condition())
            .push_child(value())
            .build(),
        other => panic!("unknown lazy-sensitive family {other}"),
    }
}

#[test]
fn control_and_logical_dispatches_are_all_lazy_and_classified() {
    let mut classified = BTreeSet::new();
    for sig in ScalarFuncSig::values() {
        let name = format!("{sig:?}");
        let Some((family, suffix)) = classify(&name) else {
            continue;
        };
        let meta = match map_expr_node_to_rpn_func(&synthetic(*sig, family, suffix)) {
            Ok(meta) => meta,
            // TiKV intentionally does not dispatch every proto signature (for
            // example the VectorFloat32 control forms). A future arm for one of
            // them is caught here because the dispatch then succeeds.
            Err(_) => continue,
        };
        assert!(
            meta.lazy_fn_ptr.is_some(),
            "{name} is a lazy-sensitive control signature but dispatches to the eager kernel {}",
            meta.name
        );
        assert!(
            LAZY_SENSITIVE_KERNELS.contains(&meta.name),
            "{} is dispatched lazily but is missing from LAZY_SENSITIVE_KERNELS",
            meta.name
        );
        classified.insert(meta.name);
    }
    // Guard against the enumerator silently matching nothing (for example if
    // `ScalarFuncSig::values()` stops returning the control signatures).
    assert!(
        classified.contains("if_condition")
            && classified.contains("if_null")
            && classified.contains("coalesce")
            && classified.contains("case_when")
            && classified.contains("logical_and")
            && classified.contains("logical_or")
            && classified.contains("logical_xor"),
        "classification found only {classified:?}"
    );
}
