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
        | LocalFunctionId::DegreesRaw
        | LocalFunctionId::PiRaw
        | LocalFunctionId::IsIpv4Nullable
        | LocalFunctionId::IsIpv6Nullable
        | LocalFunctionId::IsIpv4CompatNullable
        | LocalFunctionId::IsIpv4MappedNullable
        | LocalFunctionId::SpaceNative
        | LocalFunctionId::RepeatNative
        | LocalFunctionId::ToBase64Native
        | LocalFunctionId::FromBase64Native
        | LocalFunctionId::FromBase64ValueNative
        | LocalFunctionId::LowerUtf8Ready
        | LocalFunctionId::UpperUtf8Ready
        | LocalFunctionId::Sha2Native
        | LocalFunctionId::OrdNative
        | LocalFunctionId::TrimBothNative
        | LocalFunctionId::TrimLeadingNative
        | LocalFunctionId::TrimTrailingNative
        | LocalFunctionId::SubstringIndexSignedNative
        | LocalFunctionId::SubstringIndexUnsignedNative
        | LocalFunctionId::LpadBytesNative
        | LocalFunctionId::RpadBytesNative
        | LocalFunctionId::LpadUtf8Native
        | LocalFunctionId::RpadUtf8Native
        | LocalFunctionId::LnNative
        | LocalFunctionId::LogNative
        | LocalFunctionId::Log2Native
        | LocalFunctionId::PowNative
        | LocalFunctionId::UncompressedLengthNative
        | LocalFunctionId::InsertUtf8Native
        | LocalFunctionId::LowerAsciiNative
        | LocalFunctionId::UpperAsciiNative
        | LocalFunctionId::Substring2BytesNative
        | LocalFunctionId::Substring3BytesNative
        | LocalFunctionId::Substring2Utf8Native
        | LocalFunctionId::Substring3Utf8Native
        | LocalFunctionId::Substring2BytesLegacy
        | LocalFunctionId::Substring3BytesLegacy
        | LocalFunctionId::Substring2Utf8Legacy
        | LocalFunctionId::Substring3Utf8Legacy
        | LocalFunctionId::StrcmpNative
        | LocalFunctionId::Locate2Native
        | LocalFunctionId::Locate3Native
        | LocalFunctionId::Locate3BytesExtNative
        | LocalFunctionId::Locate3Utf8ExtNative
        | LocalFunctionId::FindInSetNative
        | LocalFunctionId::FindInSetPreparedNative
        | LocalFunctionId::OctStringNative
        | LocalFunctionId::ConcatNative
        | LocalFunctionId::ConcatWsNative
        | LocalFunctionId::EltNative
        | LocalFunctionId::FieldBytesNative
        | LocalFunctionId::FieldIntNative
        | LocalFunctionId::FieldRealNative
        | LocalFunctionId::MakeSetNative
        | LocalFunctionId::ExportSetNative
        | LocalFunctionId::AbsIntNative
        | LocalFunctionId::AbsUIntNative
        | LocalFunctionId::AbsRealNative
        | LocalFunctionId::AbsDecimalNative
        | LocalFunctionId::CeilIntNative
        | LocalFunctionId::FloorIntNative
        | LocalFunctionId::CeilRealNative
        | LocalFunctionId::FloorRealNative
        | LocalFunctionId::CeilDecimalNative
        | LocalFunctionId::FloorDecimalNative
        | LocalFunctionId::RoundIntNative
        | LocalFunctionId::RoundIntWithScaleNative
        | LocalFunctionId::RoundRealNative
        | LocalFunctionId::RoundDecimalNative
        | LocalFunctionId::TruncateIntNative
        | LocalFunctionId::TruncateUIntNative
        | LocalFunctionId::TruncateIntUnsignedScaleNative
        | LocalFunctionId::TruncateRealNative
        | LocalFunctionId::TruncateDecimalNative
        | LocalFunctionId::RoundInt128Legacy
        | LocalFunctionId::RoundRealLegacy
        | LocalFunctionId::RoundDecimalLegacy
        | LocalFunctionId::MathNullWitnessNative
        | LocalFunctionId::CharNative
        | LocalFunctionId::ConvNative
        | LocalFunctionId::ConvBinaryLiteralNative
        | LocalFunctionId::ConvLegacy
        | LocalFunctionId::SinGoNative
        | LocalFunctionId::CosGoNative
        | LocalFunctionId::TanGoNative
        | LocalFunctionId::CotGoNative
        | LocalFunctionId::AtanGoNative
        | LocalFunctionId::Atan2GoNative
        | LocalFunctionId::SinLibmLegacy
        | LocalFunctionId::CosLibmLegacy
        | LocalFunctionId::CotLibmLegacy
        | LocalFunctionId::AtanLibmLegacy
        | LocalFunctionId::Atan2LibmLegacy
        | LocalFunctionId::ExpGoNative
        | LocalFunctionId::Log10GoNative
        | LocalFunctionId::CompressGoNative
        | LocalFunctionId::UncompressNative
        | LocalFunctionId::JsonValidTextNative
        | LocalFunctionId::JsonValidBinaryNative
        | LocalFunctionId::JsonValidOtherNative
        | LocalFunctionId::JsonTypeTextNative
        | LocalFunctionId::JsonTypeBinaryNative
        | LocalFunctionId::JsonDepthNative
        | LocalFunctionId::JsonStorageFreeNative
        | LocalFunctionId::JsonStorageSizeNative
        | LocalFunctionId::JsonQuoteNative
        | LocalFunctionId::YearCoreNative
        | LocalFunctionId::MonthCoreNative
        | LocalFunctionId::DayOfMonthCoreNative
        | LocalFunctionId::QuarterCoreNative
        | LocalFunctionId::HourTextNative
        | LocalFunctionId::MinuteTextNative
        | LocalFunctionId::SecondTextNative
        | LocalFunctionId::HourNanosNative
        | LocalFunctionId::MinuteNanosNative
        | LocalFunctionId::SecondNanosNative
        | LocalFunctionId::MonthNameTextNative
        | LocalFunctionId::TimeToSecTextNative
        | LocalFunctionId::PeriodAddNative
        | LocalFunctionId::PeriodDiffNative
        | LocalFunctionId::GetFormatNative
        | LocalFunctionId::GetFormatNullNative
        | LocalFunctionId::DayOfWeekTextNative
        | LocalFunctionId::WeekdayTextNative
        | LocalFunctionId::DayOfYearTextNative
        | LocalFunctionId::DayNameTextNative
        | LocalFunctionId::DateDiffTextNative
        | LocalFunctionId::DateDiffNullNative
        | LocalFunctionId::DateDiffCoreNative
        | LocalFunctionId::ToDaysTextNative
        | LocalFunctionId::ToSecondsTextNative
        | LocalFunctionId::TsoLogicalNative
        | LocalFunctionId::WeekDateTextNative
        | LocalFunctionId::WeekTextNative
        | LocalFunctionId::YearWeekTextNative
        | LocalFunctionId::WeekOfYearTextNative
        | LocalFunctionId::WeekNullNative
        | LocalFunctionId::WeekCoreNative
        | LocalFunctionId::PasswordNative
        | LocalFunctionId::Sm3Native
        | LocalFunctionId::MakeDateNative
        | LocalFunctionId::FromDaysNative
        | LocalFunctionId::MakeTimePartsNative
        | LocalFunctionId::SecToTimeNative
        | LocalFunctionId::DateFormatTextNative
        | LocalFunctionId::DateFormatCoreNative
        | LocalFunctionId::DateFormatNullNative
        | LocalFunctionId::DateFormatMissingNative
        | LocalFunctionId::DurationTextProbeNative
        | LocalFunctionId::TimeFormatTextNative
        | LocalFunctionId::LastDayTextNative
        | LocalFunctionId::IsUuidNative
        | LocalFunctionId::UuidVersionNative
        | LocalFunctionId::UuidTimestampNative
        | LocalFunctionId::UuidToBinParseNative
        | LocalFunctionId::UuidToBinSwapNative
        | LocalFunctionId::BinToUuidNative
        | LocalFunctionId::TranslateUtf8Native
        | LocalFunctionId::TranslateBinaryNative
        | LocalFunctionId::TranslateNullNative
        | LocalFunctionId::SqlEncodeNative
        | LocalFunctionId::SqlDecodeNative
        | LocalFunctionId::SqlCryptNullNative
        | LocalFunctionId::TidbShardNative
        | LocalFunctionId::VitessHashNative
        | LocalFunctionId::FormatBytesNative
        | LocalFunctionId::FormatNanoTimeNative
        | LocalFunctionId::VecAsTextNative
        | LocalFunctionId::VecDimsNative
        | LocalFunctionId::VecL1DistanceNative
        | LocalFunctionId::VecL2DistanceNative
        | LocalFunctionId::VecNegativeInnerProductNative
        | LocalFunctionId::VecCosineDistanceNative
        | LocalFunctionId::VecL2NormNative
        | LocalFunctionId::VecFromTextNative
        | LocalFunctionId::VecRealNullNative
        | LocalFunctionId::LikeNative
        | LocalFunctionId::IlikeNative
        | LocalFunctionId::LikeLegacyNative
        | LocalFunctionId::LikeNullIntNative
        | LocalFunctionId::LikeMissingLegacyNative
        | LocalFunctionId::RegexpLikeNative
        | LocalFunctionId::RegexpSubstrNative
        | LocalFunctionId::RegexpInstrNative
        | LocalFunctionId::RegexpReplaceNative
        | LocalFunctionId::RegexpLikeLegacyCiNative
        | LocalFunctionId::RegexpLikeLegacyBinNative
        | LocalFunctionId::RegexpNullIntNative
        | LocalFunctionId::RegexpNullBytesNative
        | LocalFunctionId::RegexpMissingLegacyNative
        | LocalFunctionId::UnaryPlusIntNative
        | LocalFunctionId::UnaryPlusBitsNative
        | LocalFunctionId::UnaryPlusDecimalNative
        | LocalFunctionId::UnaryPlusBytesNative
        | LocalFunctionId::UnaryMinusIntNative
        | LocalFunctionId::UnaryMinusUIntNative
        | LocalFunctionId::UnaryMinusIntConstantNative
        | LocalFunctionId::UnaryMinusUIntConstantNative
        | LocalFunctionId::UnaryMinusBitsNative
        | LocalFunctionId::UnaryMinusDecimalNative
        | LocalFunctionId::UnaryNullNative
        | LocalFunctionId::AddIntSsNative
        | LocalFunctionId::AddIntSuNative
        | LocalFunctionId::AddIntUsNative
        | LocalFunctionId::AddIntUuNative
        | LocalFunctionId::SubIntSsNative
        | LocalFunctionId::SubIntSuNative
        | LocalFunctionId::SubIntUsNative
        | LocalFunctionId::SubIntUuNative
        | LocalFunctionId::SubIntSuForcedNative
        | LocalFunctionId::SubIntUsForcedNative
        | LocalFunctionId::SubIntUuForcedNative
        | LocalFunctionId::MulIntSignedNative
        | LocalFunctionId::MulIntUnsignedNative
        | LocalFunctionId::AddRealNative
        | LocalFunctionId::SubRealNative
        | LocalFunctionId::MulRealNative
        | LocalFunctionId::AddDecimalNative
        | LocalFunctionId::SubDecimalNative
        | LocalFunctionId::MulDecimalNative
        | LocalFunctionId::AddVectorNative
        | LocalFunctionId::SubVectorNative
        | LocalFunctionId::MulVectorNative
        | LocalFunctionId::BinaryArithmeticNullNative
        | LocalFunctionId::AddInt128SignedLegacy
        | LocalFunctionId::AddInt128UnsignedLegacy
        | LocalFunctionId::AddInt128RejectLeftLegacy
        | LocalFunctionId::AddInt128RejectRightLegacy
        | LocalFunctionId::SubInt128SignedLegacy
        | LocalFunctionId::SubInt128UnsignedLegacy
        | LocalFunctionId::SubInt128RejectLeftLegacy
        | LocalFunctionId::SubInt128RejectRightLegacy
        | LocalFunctionId::MulInt128SignedLegacy
        | LocalFunctionId::MulInt128UnsignedLegacy
        | LocalFunctionId::AddRealLegacy
        | LocalFunctionId::SubRealLegacy
        | LocalFunctionId::MulRealLegacy
        | LocalFunctionId::AddDecimalLegacy
        | LocalFunctionId::SubDecimalLegacy
        | LocalFunctionId::MulDecimalLegacy
        | LocalFunctionId::BinaryArithmeticMissingLegacy
        | LocalFunctionId::AddDecimalFastNative
        | LocalFunctionId::SubDecimalFastNative
        | LocalFunctionId::MulDecimalFastNative
        | LocalFunctionId::ModIntSsNative
        | LocalFunctionId::ModIntSuNative
        | LocalFunctionId::ModIntUsNative
        | LocalFunctionId::ModIntUuNative
        | LocalFunctionId::ModInt128Legacy
        | LocalFunctionId::ModRealNative
        | LocalFunctionId::ModRealLegacy
        | LocalFunctionId::ModDecimalNative
        | LocalFunctionId::DivRealNative
        | LocalFunctionId::DivRealLegacy
        | LocalFunctionId::DivDecimalNative
        | LocalFunctionId::DivDecimalLegacy
        | LocalFunctionId::AesEncrypt128EcbNative
        | LocalFunctionId::AesEncrypt192EcbNative
        | LocalFunctionId::AesEncrypt256EcbNative
        | LocalFunctionId::AesDecrypt128EcbNative
        | LocalFunctionId::AesDecrypt192EcbNative
        | LocalFunctionId::AesDecrypt256EcbNative
        | LocalFunctionId::AesEncrypt128CbcNative
        | LocalFunctionId::AesEncrypt192CbcNative
        | LocalFunctionId::AesEncrypt256CbcNative
        | LocalFunctionId::AesDecrypt128CbcNative
        | LocalFunctionId::AesDecrypt192CbcNative
        | LocalFunctionId::AesDecrypt256CbcNative
        | LocalFunctionId::AesEncrypt128OfbNative
        | LocalFunctionId::AesEncrypt192OfbNative
        | LocalFunctionId::AesEncrypt256OfbNative
        | LocalFunctionId::AesDecrypt128OfbNative
        | LocalFunctionId::AesDecrypt192OfbNative
        | LocalFunctionId::AesDecrypt256OfbNative
        | LocalFunctionId::AesEncrypt128CfbNative
        | LocalFunctionId::AesEncrypt192CfbNative
        | LocalFunctionId::AesEncrypt256CfbNative
        | LocalFunctionId::AesDecrypt128CfbNative
        | LocalFunctionId::AesDecrypt192CfbNative
        | LocalFunctionId::AesDecrypt256CfbNative
        | LocalFunctionId::AesNullNative
        | LocalFunctionId::CompareIntSsNative(_)
        | LocalFunctionId::CompareIntSuNative(_)
        | LocalFunctionId::CompareIntUsNative(_)
        | LocalFunctionId::CompareIntUuNative(_)
        | LocalFunctionId::CompareInt128Legacy(_)
        | LocalFunctionId::CompareRealNative(_)
        | LocalFunctionId::CompareRealLegacy(_)
        | LocalFunctionId::CompareDecimalNative(_)
        | LocalFunctionId::CompareBytesNative(_)
        | LocalFunctionId::CompareVectorNative(_)
        | LocalFunctionId::CompareTimeCoreNative(_)
        | LocalFunctionId::CompareDurationNative(_)
        | LocalFunctionId::CompareJsonNative(_)
        | LocalFunctionId::CompareNullNative
        | LocalFunctionId::CompareMissingLegacy
        | LocalFunctionId::GroupingBitAndNative
        | LocalFunctionId::GroupingNumericCmpNative
        | LocalFunctionId::GroupingNumericSetNative
        | LocalFunctionId::GroupingNullNative
        | LocalFunctionId::JsonContainsSerdeNative
        | LocalFunctionId::JsonContainsPathSerdeNative
        | LocalFunctionId::JsonOverlapsSerdeNative
        | LocalFunctionId::JsonMemberOfSerdeNative
        | LocalFunctionId::JsonLengthSerdeNative
        | LocalFunctionId::JsonLengthPathSerdeNative
        | LocalFunctionId::JsonPathExistsSerdeNative
        | LocalFunctionId::JsonMemberOfBinaryLegacy
        | LocalFunctionId::JsonPredicateNullNative
        | LocalFunctionId::JsonPredicateMissingLegacy
        | LocalFunctionId::JsonArraySerdeNative
        | LocalFunctionId::JsonObjectSerdeNative
        | LocalFunctionId::JsonKeysSerdeNative
        | LocalFunctionId::JsonKeysPathSerdeNative
        | LocalFunctionId::JsonPrettySerdeNative
        | LocalFunctionId::JsonOutputNullNative
        | LocalFunctionId::JsonExtractSerdeNative
        | LocalFunctionId::JsonInsertSerdeNative
        | LocalFunctionId::JsonSetSerdeNative
        | LocalFunctionId::JsonReplaceSerdeNative
        | LocalFunctionId::JsonRemoveSerdeNative
        | LocalFunctionId::JsonArrayAppendSerdeNative
        | LocalFunctionId::JsonArrayInsertSerdeNative
        | LocalFunctionId::JsonReplaceRawLegacy
        | LocalFunctionId::JsonArrayAppendRawLegacy
        | LocalFunctionId::JsonArrayAppendEmptyLegacy
        | LocalFunctionId::JsonValueAbsentLegacy
        | LocalFunctionId::JsonUnquoteTextNative
        | LocalFunctionId::JsonUnquoteBinaryNative
        | LocalFunctionId::UtcDateNative
        | LocalFunctionId::UtcTimestampNative
        | LocalFunctionId::CurrentTimeWithoutFspNative
        | LocalFunctionId::CurrentTimeWithFspNative
        | LocalFunctionId::UtcTimeWithoutFspNative
        | LocalFunctionId::UtcTimeWithFspNative
        | LocalFunctionId::UtcTimeNullNative => Err(other_err!(
            "Function {:?} requires the closed private factory",
            id
        )),
    }
}
