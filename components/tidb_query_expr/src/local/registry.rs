// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use tidb_query_common::Result;
use tidb_query_datatype::{FieldTypeAccessor, FieldTypeFlag, FieldTypeTp};
use tipb::{FieldType, ScalarFuncSig};

use crate::{
    CallMetadata, FunctionRef, LocalFunctionId, RpnFnMeta,
    types::function::{CallShape, validate_argument_count_eq},
};

pub(crate) fn check_signed_int_type(field_type: &FieldType) -> Result<()> {
    let accessor = field_type.as_accessor();
    if accessor.tp() != FieldTypeTp::LongLong || accessor.flag().contains(FieldTypeFlag::UNSIGNED) {
        return Err(other_err!(
            "Local seed requires signed LongLong, received {:?}",
            field_type
        ));
    }
    Ok(())
}

/// Admission is not another kernel map. The only TiPb map remains in lib.rs.
pub(crate) fn check_local_admission(call: &CallShape, metadata: &CallMetadata) -> Result<()> {
    if !matches!(metadata, CallMetadata::None) {
        return Err(other_err!(
            "Local integer seed does not accept function metadata"
        ));
    }
    check_signed_int_type(call.return_type())?;
    for arg in call.args() {
        check_signed_int_type(arg.field_type())?;
    }
    match call.function() {
        FunctionRef::TiPb(
            ScalarFuncSig::PlusIntSignedSigned
            | ScalarFuncSig::AbsInt
            | ScalarFuncSig::LogicalAnd
            | ScalarFuncSig::LogicalOr
            | ScalarFuncSig::IfInt
            | ScalarFuncSig::IfNullInt
            | ScalarFuncSig::CaseWhenInt
            | ScalarFuncSig::CoalesceInt,
        )
        | FunctionRef::Local(LocalFunctionId::NullIfIntSignedSigned) => Ok(()),
        function => Err(other_err!(
            "Function {:?} is outside the admitted local seed domain",
            function
        )),
    }
}

pub(crate) fn map_local_call_to_rpn_func(
    id: LocalFunctionId,
    call: &CallShape,
) -> Result<RpnFnMeta> {
    match id {
        LocalFunctionId::NullIfIntSignedSigned => {
            validate_argument_count_eq(call.args().len(), 2)?;
            check_signed_int_type(call.return_type())?;
            for arg in call.args() {
                check_signed_int_type(arg.field_type())?;
            }
            if call.return_type() != call.args()[0].field_type() {
                return Err(other_err!(
                    "Local NULLIF must retain its first operand's field type"
                ));
            }
            Ok(crate::impl_control::local_nullif_int_signed_signed_fn_meta())
        }
        LocalFunctionId::AsinRaw
        | LocalFunctionId::AcosRaw
        | LocalFunctionId::SqrtRaw
        | LocalFunctionId::SignRaw
        | LocalFunctionId::RadiansRaw
        | LocalFunctionId::DegreesRaw => Err(other_err!(
            "Function {:?} requires the closed raw math factory",
            id
        )),
    }
}
