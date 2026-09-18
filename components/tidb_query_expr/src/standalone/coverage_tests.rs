// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use protobuf::Message;
use tidb_query_datatype::{
    FieldTypeTp,
    codec::mysql::{DecimalDecoder, JsonDecoder, TimeType},
};
use tipb_helper::ExprDefBuilder as E;

use super::{tests::prepare, *};

fn ft(tp: FieldTypeTp) -> FieldType {
    let mut ft: FieldType = tp.into();
    ft.set_collate(63);
    ft.set_charset("binary".into());
    ft.set_flen(-1);
    ft.set_decimal(-1);
    ft
}
fn call(sig: ScalarFuncSig, tp: FieldTypeTp, children: Vec<Expr>) -> Expr {
    let mut expr = E::scalar_func(sig, ft(tp)).build();
    expr.set_children(children.into());
    expr
}
fn integer(v: i64) -> Expr {
    E::constant_int(v).build()
}
fn bytes(v: &str) -> Expr {
    E::constant_bytes(v.as_bytes().to_vec()).build()
}
fn decimal(v: &str) -> Expr {
    E::constant_decimal(v.parse().unwrap()).build()
}
fn json(v: &str) -> Expr {
    let mut expr = E::constant_null(ft(FieldTypeTp::Json)).build();
    expr.set_tp(ExprType::MysqlJson);
    expr.set_val(json_to_binary(&v.parse().unwrap()).unwrap());
    expr
}
fn vector(v: &[f32]) -> Expr {
    let mut expr = E::constant_null(ft(FieldTypeTp::TiDbVectorFloat32)).build();
    expr.set_tp(ExprType::TiDbVectorFloat32);
    let mut data = (v.len() as u32).to_le_bytes().to_vec();
    for value in v {
        data.extend(value.to_le_bytes());
    }
    expr.set_val(data);
    expr
}

#[test]
fn all_nine_types_identity_null_selection_and_split() {
    let time = DateTime::parse(
        &mut EvalContext::default(),
        "2024-02-29 12:34:56.123456",
        TimeType::DateTime,
        6,
        false,
    )
    .unwrap();
    let duration = Duration::from_nanos(-12_345_678_000, 6).unwrap();
    let cases = vec![
        (FieldTypeTp::LongLong, Column::Int(vec![Some(7), None])),
        (FieldTypeTp::Double, Column::Real(vec![Some(1.25), None])),
        (
            FieldTypeTp::VarChar,
            Column::Bytes(vec![Some(vec![0, 255]), None]),
        ),
        (
            FieldTypeTp::NewDecimal,
            Column::Decimal(vec![Some("12.3400".parse().unwrap()), None]),
        ),
        (
            FieldTypeTp::DateTime,
            Column::DateTime(vec![Some(time), None]),
        ),
        (
            FieldTypeTp::Duration,
            Column::Duration(vec![Some(duration), None]),
        ),
        (
            FieldTypeTp::Json,
            Column::Json(vec![Some(r#"{"a":[null,1]}"#.parse().unwrap()), None]),
        ),
        (
            FieldTypeTp::Enum,
            Column::Enum(vec![Some(Enum::new(b"one".to_vec(), 1)), None]),
        ),
        (
            FieldTypeTp::TiDbVectorFloat32,
            Column::VectorFloat32(vec![
                Some(VectorFloat32::from_f32(vec![1.0, -2.0]).unwrap()),
                None,
            ]),
        ),
    ];
    for (tp, input) in cases {
        let schema = ft(tp);
        let mut identity = prepare(
            E::column_ref(0, schema.clone()),
            std::slice::from_ref(&schema),
            Context::default(),
        );
        assert_eq!(
            identity
                .eval(std::slice::from_ref(&input), 2, None)
                .unwrap()
                .column,
            input
        );
        let selection = vec![1; BATCH_MAX_SIZE + 7];
        let output = identity
            .eval(std::slice::from_ref(&input), 2, Some(&selection))
            .unwrap();
        assert_eq!(output.column.len(), selection.len());
        assert_eq!(
            identity
                .eval(std::slice::from_ref(&input), 2, Some(&[]))
                .unwrap()
                .column
                .len(),
            0
        );
        let mut null = prepare(E::constant_null(schema), &[], Context::default());
        assert_eq!(null.eval(&[], 3, None).unwrap().column.len(), 3);
        // The packed borrowed API must decline unsupported input/intermediate types.
        if !matches!(
            tp,
            FieldTypeTp::LongLong | FieldTypeTp::Double | FieldTypeTp::VarChar
        ) {
            assert!(!identity.supports_borrowed());
        }
    }
}

#[test]
fn original_kernel_family_smoke() {
    use FieldTypeTp::{Double as R, Json as J, LongLong as I, VarChar as B};
    use ScalarFuncSig::*;
    let cases = vec![
        (
            call(PlusIntSignedSigned, I, vec![integer(2), integer(3)]),
            Column::Int(vec![Some(5)]),
        ),
        (
            call(CastStringAsInt, I, vec![bytes("42")]),
            Column::Int(vec![Some(42)]),
        ),
        (
            call(EqString, I, vec![bytes("abc"), bytes("abc")]),
            Column::Int(vec![Some(1)]),
        ),
        (
            call(InInt, I, vec![integer(9), integer(3), integer(9)]),
            Column::Int(vec![Some(1)]),
        ),
        (
            call(IfInt, I, vec![integer(0), integer(10), integer(20)]),
            Column::Int(vec![Some(20)]),
        ),
        (
            call(Md5, B, vec![bytes("abc")]),
            Column::Bytes(vec![Some(b"900150983cd24fb0d6963f7d28e17f72".to_vec())]),
        ),
        (
            call(JsonDepthSig, I, vec![json(r#"{"a":[1]}"#)]),
            Column::Int(vec![Some(3)]),
        ),
        (
            call(JsonArraySig, J, vec![json("1"), json("true")]),
            Column::Json(vec![Some("[1,true]".parse().unwrap())]),
        ),
        (
            call(VecDimsSig, I, vec![vector(&[1.0, 2.0, 3.0])]),
            Column::Int(vec![Some(3)]),
        ),
        (
            call(LikeSig, I, vec![bytes("abc"), bytes("a%"), integer(92)]),
            Column::Int(vec![Some(1)]),
        ),
        (
            call(RegexpLikeSig, I, vec![bytes("abc"), bytes("^a")]),
            Column::Int(vec![Some(1)]),
        ),
        (
            call(Sqrt, R, vec![E::constant_real(9.0).build()]),
            Column::Real(vec![Some(3.0)]),
        ),
        (
            call(InetAton, I, vec![bytes("127.0.0.1")]),
            Column::Int(vec![Some(2130706433)]),
        ),
        (
            call(BitAndSig, I, vec![integer(7), integer(3)]),
            Column::Int(vec![Some(3)]),
        ),
        (
            call(BitCount, I, vec![integer(7)]),
            Column::Int(vec![Some(3)]),
        ),
        (
            call(
                Substring3Args,
                B,
                vec![bytes("abcdef"), integer(2), integer(3)],
            ),
            Column::Bytes(vec![Some(b"bcd".to_vec())]),
        ),
        (
            call(
                Year,
                I,
                vec![call(
                    StrToDateDatetime,
                    FieldTypeTp::DateTime,
                    vec![bytes("2024-02-29"), bytes("%Y-%m-%d")],
                )],
            ),
            Column::Int(vec![Some(2024)]),
        ),
    ];
    for (expr, expected) in cases {
        let sig = expr.get_sig();
        let mut prepared = prepare(expr, &[], Context::default());
        assert_eq!(
            prepared.eval(&[], 1, None).unwrap().column,
            expected,
            "{sig:?}"
        );
    }
}

#[test]
fn lazy_branch_errors_are_skipped_and_needed_branch_errors_abort() {
    // The condition selects the true branch, so the overflowing false branch is
    // never entered and cannot abort the batch.
    let expr = call(
        ScalarFuncSig::IfInt,
        FieldTypeTp::LongLong,
        vec![
            integer(1),
            integer(7),
            call(
                ScalarFuncSig::PlusInt,
                FieldTypeTp::LongLong,
                vec![integer(i64::MAX), integer(1)],
            ),
        ],
    );
    let mut prepared = prepare(expr, &[], Context::default());
    assert_eq!(
        prepared.eval(&[], 1, None).unwrap().column,
        Column::Int(vec![Some(7)])
    );
    // The same branch is needed once the condition is false, so the batch fails.
    let expr = call(
        ScalarFuncSig::IfInt,
        FieldTypeTp::LongLong,
        vec![
            integer(0),
            integer(7),
            call(
                ScalarFuncSig::PlusInt,
                FieldTypeTp::LongLong,
                vec![integer(i64::MAX), integer(1)],
            ),
        ],
    );
    let mut prepared = prepare(expr, &[], Context::default());
    assert_eq!(prepared.eval(&[], 1, None).unwrap_err().code, 1690);
    // RPN metadata construction eagerly compiles even unreachable regex patterns.
    let expr = call(
        ScalarFuncSig::RegexpLikeSig,
        FieldTypeTp::LongLong,
        vec![bytes("abc"), bytes("[")],
    );
    assert!(
        PreparedExpression::compile(&expr.write_to_bytes().unwrap(), &[], Context::default())
            .is_err()
    );
}

#[test]
fn decimal_hidden_storage_and_result_scales_survive_transport_and_evaluation() {
    let mut divide = prepare(
        call(
            ScalarFuncSig::DivideDecimal,
            FieldTypeTp::NewDecimal,
            vec![decimal("1"), decimal("3")],
        ),
        &[],
        Context::default(),
    );
    let Column::Decimal(output) = divide.eval(&[], 1, None).unwrap().column else {
        unreachable!()
    };
    let value = output[0].unwrap();
    assert_ne!(value.frac_cnt(), decimal_to_chunk(&value).unwrap()[2]);
    let raw = decimal_to_chunk(&value).unwrap();
    let exact = decimal_from_chunk(&raw).unwrap();
    assert_eq!(decimal_to_chunk(&exact).unwrap(), raw);
    let ft = ft(FieldTypeTp::NewDecimal);
    let mut sum = prepare(
        call(
            ScalarFuncSig::PlusDecimal,
            FieldTypeTp::NewDecimal,
            vec![E::column_ref(0, ft.clone()).build(), decimal("0")],
        ),
        &[ft],
        Context::default(),
    );
    let Column::Decimal(output) = sum
        .eval(&[Column::Decimal(vec![Some(exact)])], 1, None)
        .unwrap()
        .column
    else {
        unreachable!()
    };
    assert_eq!(output[0].unwrap().frac_cnt(), value.frac_cnt());
    assert_eq!(decimal_to_chunk(&output[0].unwrap()).unwrap()[2], raw[2]);
    // Display rounds to result_frac_cnt: this is why the old string boundary lost
    // data.
    assert_ne!(value, value.to_string().parse::<Decimal>().unwrap());
}

#[test]
fn native_transport_roundtrips_and_rejects_malformed_values() {
    for time_type in [TimeType::Date, TimeType::DateTime, TimeType::Timestamp] {
        let time = DateTime::parse(
            &mut EvalContext::default(),
            "2024-02-29 12:34:56.123456",
            time_type,
            6,
            false,
        )
        .unwrap();
        let chunk = date_time_to_chunk(&time).unwrap();
        assert_eq!(
            date_time_to_chunk(&date_time_from_chunk(&chunk).unwrap()).unwrap(),
            chunk
        );
    }
    for text in [
        "null",
        "-123",
        "18446744073709551615",
        "1.5",
        r#"{"a":[true,"a\u0000b"]}"#,
    ] {
        let original: Json = text.parse().unwrap();
        let binary = json_to_binary(&original).unwrap();
        assert_eq!(
            json_to_binary(&json_from_binary(&binary).unwrap()).unwrap(),
            binary
        );
    }
    for bytes in [
        vec![],
        vec![1],
        vec![3, 0, 0, 0, 0],
        vec![0x0d],
        vec![0x0c, 255],
        vec![0x04, 3],
    ] {
        assert!(json_from_binary(&bytes).is_err());
    }
    let mut raw = decimal_to_chunk(&"1.25".parse().unwrap()).unwrap();
    raw[3] = 2; // Invalid Rust bool must not enter trusted unsafe chunk decoder.
    assert!(decimal_from_chunk(&raw).is_err());
    raw[3] = 0;
    raw[0] = 255;
    assert!(decimal_from_chunk(&raw).is_err());
    assert!(date_time_from_chunk(&[255; 8]).is_err());
}

#[test]
fn trusted_engine_safety_gaps_are_closed_at_public_boundary() {
    // Bounded red witnesses: these trusted native paths panic without facade
    // validation; no unsafe malformed decimal decoder is invoked in this test.
    for sig in [ScalarFuncSig::ToBinary, ScalarFuncSig::LikeSig] {
        let expr = call(sig, FieldTypeTp::VarChar, vec![]);
        assert!(std::panic::catch_unwind(|| crate::map_expr_node_to_rpn_func(&expr)).is_err());
        assert!(
            PreparedExpression::compile(&expr.write_to_bytes().unwrap(), &[], Context::default())
                .is_err()
        );
    }
    assert!(std::panic::catch_unwind(|| [1u8].as_slice().read_json()).is_err());
    assert!(json_from_binary(&[1]).is_err());
    for sig in [
        ScalarFuncSig::RegexpLikeSig,
        ScalarFuncSig::RegexpSubstrSig,
        ScalarFuncSig::RegexpInStrSig,
    ] {
        let expr = call(
            sig,
            if sig == ScalarFuncSig::RegexpSubstrSig {
                FieldTypeTp::VarChar
            } else {
                FieldTypeTp::LongLong
            },
            vec![integer(1), bytes(".")],
        );
        // Native raw_varg validator accepts this, but as_bytes() would panic.
        let meta = crate::map_expr_node_to_rpn_func(&expr).unwrap();
        (meta.validator_ptr)(&expr).unwrap();
        assert!(
            PreparedExpression::compile(&expr.write_to_bytes().unwrap(), &[], Context::default())
                .is_err()
        );
    }
    let schema = ft(FieldTypeTp::Json);
    let mut identity = prepare(
        E::column_ref(0, schema.clone()),
        &[schema],
        Context::default(),
    );
    let bad = Json::new(tidb_query_datatype::codec::mysql::JsonType::Array, vec![]);
    assert!(
        identity
            .eval(&[Column::Json(vec![Some(bad.clone())])], 1, None)
            .is_err()
    );
    assert!(
        identity
            .eval(&[Column::Json(vec![Some(bad)])], 1, Some(&[]))
            .is_ok()
    );
}

#[test]
fn zero_digit_decimal_is_canonicalized_before_native_arithmetic() {
    let mut raw = [0u8; 40];
    raw[2] = 4;
    // This bool is valid: native decode is safe, but subsequent shift assumes
    // at least one digit word. Keep the panic witness debug-only: unchecked
    // release underflow must never be exercised by a regression test.
    #[cfg(debug_assertions)]
    {
        let native = raw.as_slice().read_decimal_from_chunk().unwrap();
        assert!(std::panic::catch_unwind(|| native.shift(1)).is_err());
    }
    let normalized = decimal_from_chunk(&raw).unwrap();
    assert!(normalized.is_zero());
    assert!(normalized.shift(1).is_ok());
    let encoded = decimal_to_chunk(&normalized).unwrap();
    assert_eq!(encoded[0], 1);
    assert_eq!(encoded[2], 4);
    assert_eq!(normalized.to_string(), "0.0000");
    assert_eq!(
        decimal_to_chunk(&decimal_from_chunk(&encoded).unwrap()).unwrap(),
        encoded
    );
    let schema = ft(FieldTypeTp::NewDecimal);
    let mut identity = prepare(
        E::column_ref(0, schema.clone()),
        &[schema],
        Context::default(),
    );
    let Column::Decimal(output) = identity
        .eval(&[Column::Decimal(vec![Some(normalized)])], 1, None)
        .unwrap()
        .column
    else {
        unreachable!()
    };
    assert_eq!(decimal_to_chunk(&output[0].unwrap()).unwrap(), encoded);
    raw[4] = 1;
    assert!(decimal_from_chunk(&raw).is_err());
}

#[test]
fn enum_lookup_is_not_a_claim_of_builder_support() {
    assert_eq!(
        scalar_function_signature("PlusInt"),
        Some(ScalarFuncSig::PlusInt as i32)
    );
    assert_eq!(scalar_function_signature("ImaginaryKernel"), None);
    // Rand is supported by the builder despite having no facade whitelist entry.
    let mut expr = prepare(
        call(ScalarFuncSig::Rand, FieldTypeTp::Double, vec![]),
        &[],
        Context::default(),
    );
    assert!(matches!(
        expr.eval(&[], 1, None).unwrap().column,
        Column::Real(_)
    ));
}
