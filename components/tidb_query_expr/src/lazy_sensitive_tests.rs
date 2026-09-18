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

use std::collections::{BTreeMap, BTreeSet};

use protobuf::ProtobufEnum;
use tidb_query_datatype::{Collation, FieldTypeTp, builder::FieldTypeBuilder};
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

/// The five Tier 2 families whose lazy kernels replaced eager ones. A family
/// leaves [`LAZY_SENSITIVE_KERNELS`] only when *every* signature it dispatches
/// is lazy; the test below enforces exactly that against the real dispatcher.
const TIER2_FAMILIES: [&str; 5] = ["Elt", "Field", "Greatest", "Least", "Interval"];

/// Classifies a `ScalarFuncSig` debug name as a Tier 2 family member, returning
/// the family and its type suffix.
fn classify_tier2(name: &str) -> Option<(&'static str, &str)> {
    if name == "Elt" {
        return Some(("Elt", ""));
    }
    for family in ["Field", "Greatest", "Least", "Interval"] {
        if let Some(suffix) = name.strip_prefix(family) {
            return Some((family, suffix));
        }
    }
    None
}

/// Field type of a Tier 2 value argument, or `None` when the suffix belongs to
/// a signature TiKV does not dispatch (`GreatestVectorFloat32`, ...).
fn tier2_value_field_type(suffix: &str) -> Option<FieldType> {
    Some(
        match suffix {
            "Int" => FieldTypeTp::LongLong,
            "Real" => FieldTypeTp::Double,
            "Decimal" => FieldTypeTp::NewDecimal,
            "Time" | "Date" => FieldTypeTp::DateTime,
            "Duration" => FieldTypeTp::Duration,
            "String" | "CmpStringAsDate" | "CmpStringAsTime" => FieldTypeTp::VarChar,
            _ => return None,
        }
        .into(),
    )
}

/// Same as [`synthetic`], for the Tier 2 families. `None` when the suffix is
/// not one this engine dispatches.
fn synthetic_tier2(sig: ScalarFuncSig, family: &str, suffix: &str) -> Option<Expr> {
    // `Elt`'s suffix is empty, so it must be handled before the suffix lookup.
    if family == "Elt" {
        return Some(
            E::scalar_func(sig, FieldTypeTp::VarChar)
                .push_child(E::constant_int(1))
                .push_child(E::constant_bytes(vec![1]))
                .push_child(E::constant_bytes(vec![2]))
                .build(),
        );
    }
    let ft = tier2_value_field_type(suffix)?;
    Some(match family {
        "Field" => match suffix {
            // `field<T>` returns `Int` for every element type, and
            // `FieldString` goes through `map_field_string_sig`, which reads
            // the collation of the *return* field type.
            "Int" | "Real" => E::scalar_func(sig, FieldTypeTp::LongLong)
                .push_child(E::constant_null(ft.clone()))
                .push_child(E::constant_null(ft))
                .build(),
            "String" => E::scalar_func(
                sig,
                FieldTypeBuilder::new()
                    .tp(FieldTypeTp::LongLong)
                    .collation(Collation::Utf8Mb4Bin)
                    .build(),
            )
            .push_child(E::constant_null(FieldTypeTp::VarChar))
            .push_child(E::constant_null(FieldTypeTp::VarChar))
            .build(),
            _ => return None,
        },
        "Greatest" | "Least" => E::scalar_func(sig, ft.clone())
            .push_child(E::constant_null(ft.clone()))
            .push_child(E::constant_null(ft))
            .build(),
        "Interval" => {
            let child_ft = match suffix {
                "Int" => FieldTypeTp::LongLong,
                "Real" => FieldTypeTp::Double,
                _ => return None,
            };
            E::scalar_func(sig, FieldTypeTp::LongLong)
                .push_child(E::constant_null(child_ft))
                .push_child(E::constant_null(child_ft))
                .build()
        }
        other => panic!("unknown Tier 2 family {other}"),
    })
}

/// A Tier 2 family may leave [`LAZY_SENSITIVE_KERNELS`] only once every
/// signature it dispatches carries a lazy kernel, so that no eager node of the
/// family can reach [`crate::standalone::PreparedExpression::eager_lazy_risk`].
/// Conversely, as long as one dispatched signature is still eager, its kernel
/// name must stay listed or the embedder would miss the risk. This fails if a
/// family is partially registered while already removed from the list.
#[test]
fn tier2_families_are_listed_only_while_not_fully_lazy() {
    // family -> (lazy kernel names, eager kernel names), filled from the real
    // dispatcher.
    let mut findings: BTreeMap<&'static str, (BTreeSet<&'static str>, BTreeSet<&'static str>)> =
        BTreeMap::new();
    // Classified signatures this test cannot build a tree for.
    let mut unbuildable = BTreeSet::new();
    for sig in ScalarFuncSig::values() {
        let name = format!("{sig:?}");
        let Some((family, suffix)) = classify_tier2(&name) else {
            continue;
        };
        let Some(expr) = synthetic_tier2(*sig, family, suffix) else {
            unbuildable.insert(name);
            continue;
        };
        let meta = map_expr_node_to_rpn_func(&expr).unwrap_or_else(|err| {
            panic!("{name} is a dispatched Tier 2 signature but its synthetic failed: {err}")
        });
        let entry = findings.entry(family).or_default();
        if meta.lazy_fn_ptr.is_some() {
            entry.0.insert(meta.name);
        } else {
            entry.1.insert(meta.name);
        }
    }

    // The only Tier 2 signatures without a synthetic tree are the two the
    // engine does not dispatch at all. A new signature must be handled here so
    // it cannot silently dodge the laziness check.
    assert_eq!(
        unbuildable,
        BTreeSet::from([
            "GreatestVectorFloat32".to_owned(),
            "LeastVectorFloat32".to_owned(),
        ]),
        "unhandled Tier 2 signatures"
    );

    // Guard against the enumerator silently matching nothing.
    for family in TIER2_FAMILIES {
        assert!(
            findings.contains_key(family),
            "classification never dispatched a {family} signature, found only {:?}",
            findings.keys().collect::<Vec<_>>()
        );
    }

    for (family, (lazy, eager)) in &findings {
        if eager.is_empty() {
            for name in lazy {
                assert!(
                    !LAZY_SENSITIVE_KERNELS.contains(name),
                    "{name} of the fully lazy {family} family is still in LAZY_SENSITIVE_KERNELS"
                );
            }
        } else {
            for name in eager {
                assert!(
                    LAZY_SENSITIVE_KERNELS.contains(name),
                    "{name} of the partially lazy {family} family must stay in \
                     LAZY_SENSITIVE_KERNELS so eager_lazy_risk reports it"
                );
            }
        }
    }
}
