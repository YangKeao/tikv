// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;

const OPERATIONS: &[EvaluatedBytesOp] = &[
    EvaluatedBytesOp::Ascii,
    EvaluatedBytesOp::Length,
    EvaluatedBytesOp::BitLength,
    EvaluatedBytesOp::LTrim,
    EvaluatedBytesOp::RTrim,
    EvaluatedBytesOp::UnHex,
    EvaluatedBytesOp::Crc32,
    EvaluatedBytesOp::Reverse,
    EvaluatedBytesOp::ReverseUtf8,
    EvaluatedBytesOp::CharLength,
    EvaluatedBytesOp::CharLengthUtf8,
    EvaluatedBytesOp::Quote,
    EvaluatedBytesOp::HexInt,
    EvaluatedBytesOp::HexStr,
    EvaluatedBytesOp::Bin,
    EvaluatedBytesOp::Left,
    EvaluatedBytesOp::LeftUtf8,
    EvaluatedBytesOp::Right,
    EvaluatedBytesOp::RightUtf8,
    EvaluatedBytesOp::Replace,
    EvaluatedBytesOp::BitCount,
    EvaluatedBytesOp::BitNeg,
    EvaluatedBytesOp::BitAnd,
    EvaluatedBytesOp::BitOr,
    EvaluatedBytesOp::BitXor,
    EvaluatedBytesOp::LeftShift,
    EvaluatedBytesOp::RightShift,
    EvaluatedBytesOp::UnaryNot,
    EvaluatedBytesOp::IsNull,
    EvaluatedBytesOp::IsTrue,
    EvaluatedBytesOp::IsFalse,
    EvaluatedBytesOp::IsTrueWithNull,
    EvaluatedBytesOp::IsNotNull,
    EvaluatedBytesOp::IsNotTrue,
    EvaluatedBytesOp::IsNotFalse,
    EvaluatedBytesOp::Md5,
    EvaluatedBytesOp::Sha1,
    EvaluatedBytesOp::LogicalAnd,
    EvaluatedBytesOp::LogicalOr,
    EvaluatedBytesOp::LogicalXor,
    EvaluatedBytesOp::InetAton,
    EvaluatedBytesOp::InetNtoa,
    EvaluatedBytesOp::Inet6Aton,
    EvaluatedBytesOp::Inet6Ntoa,
    EvaluatedBytesOp::AsinRaw,
    EvaluatedBytesOp::AcosRaw,
    EvaluatedBytesOp::SqrtRaw,
    EvaluatedBytesOp::SignRaw,
    EvaluatedBytesOp::RadiansRaw,
    EvaluatedBytesOp::DegreesRaw,
    EvaluatedBytesOp::PiRaw,
    EvaluatedBytesOp::IsIpv4Nullable,
    EvaluatedBytesOp::IsIpv6Nullable,
    EvaluatedBytesOp::IsIpv4CompatNullable,
    EvaluatedBytesOp::IsIpv4MappedNullable,
    EvaluatedBytesOp::SpaceNative,
    EvaluatedBytesOp::RepeatNative,
    EvaluatedBytesOp::ToBase64Native,
    EvaluatedBytesOp::FromBase64Native,
    EvaluatedBytesOp::FromBase64ValueNative,
    EvaluatedBytesOp::Lower,
    EvaluatedBytesOp::Upper,
    EvaluatedBytesOp::LowerUtf8Ready,
    EvaluatedBytesOp::UpperUtf8Ready,
    EvaluatedBytesOp::Sha2Native,
    EvaluatedBytesOp::OrdNative,
    EvaluatedBytesOp::TrimBothNative,
    EvaluatedBytesOp::TrimLeadingNative,
    EvaluatedBytesOp::TrimTrailingNative,
    EvaluatedBytesOp::SubstringIndexSignedNative,
    EvaluatedBytesOp::SubstringIndexUnsignedNative,
    EvaluatedBytesOp::LpadBytesNative,
    EvaluatedBytesOp::RpadBytesNative,
    EvaluatedBytesOp::LpadUtf8Native,
    EvaluatedBytesOp::RpadUtf8Native,
    EvaluatedBytesOp::LnNative,
    EvaluatedBytesOp::LogNative,
    EvaluatedBytesOp::Log2Native,
    EvaluatedBytesOp::PowNative,
    EvaluatedBytesOp::UncompressedLengthNative,
    EvaluatedBytesOp::Insert,
    EvaluatedBytesOp::InsertUtf8Native,
    EvaluatedBytesOp::LowerAsciiNative,
    EvaluatedBytesOp::UpperAsciiNative,
    EvaluatedBytesOp::Substring2BytesNative,
    EvaluatedBytesOp::Substring3BytesNative,
    EvaluatedBytesOp::Substring2Utf8Native,
    EvaluatedBytesOp::Substring3Utf8Native,
    EvaluatedBytesOp::Substring2BytesLegacy,
    EvaluatedBytesOp::Substring3BytesLegacy,
    EvaluatedBytesOp::Substring2Utf8Legacy,
    EvaluatedBytesOp::Substring3Utf8Legacy,
    EvaluatedBytesOp::StrcmpNative,
    EvaluatedBytesOp::Locate2Native,
    EvaluatedBytesOp::Locate3Native,
    EvaluatedBytesOp::Locate3BytesExtNative,
    EvaluatedBytesOp::Locate3Utf8ExtNative,
    EvaluatedBytesOp::FindInSetNative,
    EvaluatedBytesOp::FindInSetPreparedNative,
    EvaluatedBytesOp::OctInt,
    EvaluatedBytesOp::OctStringNative,
    EvaluatedBytesOp::ConcatNative,
    EvaluatedBytesOp::ConcatWsNative,
    EvaluatedBytesOp::EltNative,
    EvaluatedBytesOp::FieldBytesNative,
    EvaluatedBytesOp::FieldIntNative,
    EvaluatedBytesOp::FieldRealNative,
    EvaluatedBytesOp::MakeSetNative,
    EvaluatedBytesOp::ExportSetNative,
    EvaluatedBytesOp::AbsIntNative,
    EvaluatedBytesOp::AbsUIntNative,
    EvaluatedBytesOp::AbsRealNative,
    EvaluatedBytesOp::AbsDecimalNative,
    EvaluatedBytesOp::CeilIntNative,
    EvaluatedBytesOp::FloorIntNative,
    EvaluatedBytesOp::CeilRealNative,
    EvaluatedBytesOp::FloorRealNative,
    EvaluatedBytesOp::CeilDecimalNative,
    EvaluatedBytesOp::FloorDecimalNative,
    EvaluatedBytesOp::RoundIntNative,
    EvaluatedBytesOp::RoundIntWithScaleNative,
    EvaluatedBytesOp::RoundRealNative,
    EvaluatedBytesOp::RoundDecimalNative,
    EvaluatedBytesOp::TruncateIntNative,
    EvaluatedBytesOp::TruncateUIntNative,
    EvaluatedBytesOp::TruncateIntUnsignedScaleNative,
    EvaluatedBytesOp::TruncateRealNative,
    EvaluatedBytesOp::TruncateDecimalNative,
    EvaluatedBytesOp::RoundInt128Legacy,
    EvaluatedBytesOp::RoundRealLegacy,
    EvaluatedBytesOp::RoundDecimalLegacy,
    EvaluatedBytesOp::MathNullWitnessNative,
    EvaluatedBytesOp::CharNative,
    EvaluatedBytesOp::ConvNative,
    EvaluatedBytesOp::ConvBinaryLiteralNative,
    EvaluatedBytesOp::ConvLegacy,
    EvaluatedBytesOp::SinGoNative,
    EvaluatedBytesOp::CosGoNative,
    EvaluatedBytesOp::TanGoNative,
    EvaluatedBytesOp::CotGoNative,
    EvaluatedBytesOp::AtanGoNative,
    EvaluatedBytesOp::Atan2GoNative,
    EvaluatedBytesOp::SinLibmLegacy,
    EvaluatedBytesOp::CosLibmLegacy,
    EvaluatedBytesOp::CotLibmLegacy,
    EvaluatedBytesOp::AtanLibmLegacy,
    EvaluatedBytesOp::Atan2LibmLegacy,
    EvaluatedBytesOp::ExpGoNative,
    EvaluatedBytesOp::Log10GoNative,
    EvaluatedBytesOp::CompressGoNative,
    EvaluatedBytesOp::UncompressNative,
    EvaluatedBytesOp::JsonValidTextNative,
    EvaluatedBytesOp::JsonValidBinaryNative,
    EvaluatedBytesOp::JsonValidOtherNative,
    EvaluatedBytesOp::JsonTypeTextNative,
    EvaluatedBytesOp::JsonTypeBinaryNative,
    EvaluatedBytesOp::JsonDepthNative,
    EvaluatedBytesOp::JsonStorageFreeNative,
    EvaluatedBytesOp::JsonStorageSizeNative,
    EvaluatedBytesOp::JsonQuoteNative,
    EvaluatedBytesOp::YearCoreNative,
    EvaluatedBytesOp::MonthCoreNative,
    EvaluatedBytesOp::DayOfMonthCoreNative,
    EvaluatedBytesOp::QuarterCoreNative,
    EvaluatedBytesOp::HourTextNative,
    EvaluatedBytesOp::MinuteTextNative,
    EvaluatedBytesOp::SecondTextNative,
    EvaluatedBytesOp::HourNanosNative,
    EvaluatedBytesOp::MinuteNanosNative,
    EvaluatedBytesOp::SecondNanosNative,
    EvaluatedBytesOp::MonthNameTextNative,
    EvaluatedBytesOp::TimeToSecTextNative,
    EvaluatedBytesOp::PeriodAddNative,
    EvaluatedBytesOp::PeriodDiffNative,
    EvaluatedBytesOp::GetFormatNative,
    EvaluatedBytesOp::GetFormatNullNative,
    EvaluatedBytesOp::DayOfWeekTextNative,
    EvaluatedBytesOp::WeekdayTextNative,
    EvaluatedBytesOp::DayOfYearTextNative,
    EvaluatedBytesOp::DayNameTextNative,
    EvaluatedBytesOp::DateDiffTextNative,
    EvaluatedBytesOp::DateDiffNullNative,
    EvaluatedBytesOp::DateDiffCoreNative,
    EvaluatedBytesOp::ToDaysTextNative,
    EvaluatedBytesOp::ToSecondsTextNative,
    EvaluatedBytesOp::TsoLogicalNative,
    EvaluatedBytesOp::WeekDateTextNative,
    EvaluatedBytesOp::WeekTextNative,
    EvaluatedBytesOp::YearWeekTextNative,
    EvaluatedBytesOp::WeekOfYearTextNative,
    EvaluatedBytesOp::WeekNullNative,
    EvaluatedBytesOp::WeekCoreNative,
    EvaluatedBytesOp::PasswordNative,
    EvaluatedBytesOp::Sm3Native,
    EvaluatedBytesOp::MakeDateNative,
    EvaluatedBytesOp::FromDaysNative,
    EvaluatedBytesOp::MakeTimePartsNative,
    EvaluatedBytesOp::SecToTimeNative,
    EvaluatedBytesOp::DateFormatTextNative,
    EvaluatedBytesOp::DateFormatCoreNative,
    EvaluatedBytesOp::DateFormatNullNative,
    EvaluatedBytesOp::DateFormatMissingNative,
    EvaluatedBytesOp::DurationTextProbeNative,
    EvaluatedBytesOp::TimeFormatTextNative,
    EvaluatedBytesOp::LastDayTextNative,
    EvaluatedBytesOp::IsUuidNative,
    EvaluatedBytesOp::UuidVersionNative,
    EvaluatedBytesOp::UuidTimestampNative,
    EvaluatedBytesOp::UuidToBinParseNative,
    EvaluatedBytesOp::UuidToBinSwapNative,
    EvaluatedBytesOp::BinToUuidNative,
    EvaluatedBytesOp::TranslateUtf8Native,
    EvaluatedBytesOp::TranslateBinaryNative,
    EvaluatedBytesOp::TranslateNullNative,
    EvaluatedBytesOp::SqlEncodeNative,
    EvaluatedBytesOp::SqlDecodeNative,
    EvaluatedBytesOp::SqlCryptNullNative,
    EvaluatedBytesOp::TidbShardNative,
    EvaluatedBytesOp::VitessHashNative,
    EvaluatedBytesOp::FormatBytesNative,
    EvaluatedBytesOp::FormatNanoTimeNative,
    EvaluatedBytesOp::VecAsTextNative,
    EvaluatedBytesOp::VecDimsNative,
    EvaluatedBytesOp::VecL1DistanceNative,
    EvaluatedBytesOp::VecL2DistanceNative,
    EvaluatedBytesOp::VecNegativeInnerProductNative,
    EvaluatedBytesOp::VecCosineDistanceNative,
    EvaluatedBytesOp::VecL2NormNative,
    EvaluatedBytesOp::VecFromTextNative,
    EvaluatedBytesOp::VecRealNullNative,
    EvaluatedBytesOp::LikeNative,
    EvaluatedBytesOp::IlikeNative,
    EvaluatedBytesOp::LikeLegacyNative,
    EvaluatedBytesOp::LikeNullIntNative,
    EvaluatedBytesOp::LikeMissingLegacyNative,
    EvaluatedBytesOp::RegexpLikeNative,
    EvaluatedBytesOp::RegexpSubstrNative,
    EvaluatedBytesOp::RegexpInstrNative,
    EvaluatedBytesOp::RegexpReplaceNative,
    EvaluatedBytesOp::RegexpLikeLegacyCiNative,
    EvaluatedBytesOp::RegexpLikeLegacyBinNative,
    EvaluatedBytesOp::RegexpNullIntNative,
    EvaluatedBytesOp::RegexpNullBytesNative,
    EvaluatedBytesOp::RegexpMissingLegacyNative,
    EvaluatedBytesOp::UnaryPlusIntNative,
    EvaluatedBytesOp::UnaryPlusBitsNative,
    EvaluatedBytesOp::UnaryPlusDecimalNative,
    EvaluatedBytesOp::UnaryPlusBytesNative,
    EvaluatedBytesOp::UnaryMinusIntNative,
    EvaluatedBytesOp::UnaryMinusUIntNative,
    EvaluatedBytesOp::UnaryMinusIntConstantNative,
    EvaluatedBytesOp::UnaryMinusUIntConstantNative,
    EvaluatedBytesOp::UnaryMinusBitsNative,
    EvaluatedBytesOp::UnaryMinusDecimalNative,
    EvaluatedBytesOp::UnaryNullNative,
    EvaluatedBytesOp::AddIntSsNative,
    EvaluatedBytesOp::AddIntSuNative,
    EvaluatedBytesOp::AddIntUsNative,
    EvaluatedBytesOp::AddIntUuNative,
    EvaluatedBytesOp::SubIntSsNative,
    EvaluatedBytesOp::SubIntSuNative,
    EvaluatedBytesOp::SubIntUsNative,
    EvaluatedBytesOp::SubIntUuNative,
    EvaluatedBytesOp::SubIntSuForcedNative,
    EvaluatedBytesOp::SubIntUsForcedNative,
    EvaluatedBytesOp::SubIntUuForcedNative,
    EvaluatedBytesOp::MulIntSignedNative,
    EvaluatedBytesOp::MulIntUnsignedNative,
    EvaluatedBytesOp::AddRealNative,
    EvaluatedBytesOp::SubRealNative,
    EvaluatedBytesOp::MulRealNative,
    EvaluatedBytesOp::AddDecimalNative,
    EvaluatedBytesOp::SubDecimalNative,
    EvaluatedBytesOp::MulDecimalNative,
    EvaluatedBytesOp::AddVectorNative,
    EvaluatedBytesOp::SubVectorNative,
    EvaluatedBytesOp::MulVectorNative,
    EvaluatedBytesOp::BinaryArithmeticNullNative,
    EvaluatedBytesOp::AddInt128SignedLegacy,
    EvaluatedBytesOp::AddInt128UnsignedLegacy,
    EvaluatedBytesOp::AddInt128RejectLeftLegacy,
    EvaluatedBytesOp::AddInt128RejectRightLegacy,
    EvaluatedBytesOp::SubInt128SignedLegacy,
    EvaluatedBytesOp::SubInt128UnsignedLegacy,
    EvaluatedBytesOp::SubInt128RejectLeftLegacy,
    EvaluatedBytesOp::SubInt128RejectRightLegacy,
    EvaluatedBytesOp::MulInt128SignedLegacy,
    EvaluatedBytesOp::MulInt128UnsignedLegacy,
    EvaluatedBytesOp::AddRealLegacy,
    EvaluatedBytesOp::SubRealLegacy,
    EvaluatedBytesOp::MulRealLegacy,
    EvaluatedBytesOp::AddDecimalLegacy,
    EvaluatedBytesOp::SubDecimalLegacy,
    EvaluatedBytesOp::MulDecimalLegacy,
    EvaluatedBytesOp::BinaryArithmeticMissingLegacy,
    EvaluatedBytesOp::AddDecimalFastNative,
    EvaluatedBytesOp::SubDecimalFastNative,
    EvaluatedBytesOp::MulDecimalFastNative,
    EvaluatedBytesOp::ModIntSsNative,
    EvaluatedBytesOp::ModIntSuNative,
    EvaluatedBytesOp::ModIntUsNative,
    EvaluatedBytesOp::ModIntUuNative,
    EvaluatedBytesOp::ModInt128Legacy,
    EvaluatedBytesOp::ModRealNative,
    EvaluatedBytesOp::ModRealLegacy,
    EvaluatedBytesOp::ModDecimalNative,
    EvaluatedBytesOp::DivRealNative,
    EvaluatedBytesOp::DivRealLegacy,
    EvaluatedBytesOp::DivDecimalNative,
    EvaluatedBytesOp::DivDecimalLegacy,
    EvaluatedBytesOp::AesEncrypt128EcbNative,
    EvaluatedBytesOp::AesEncrypt192EcbNative,
    EvaluatedBytesOp::AesEncrypt256EcbNative,
    EvaluatedBytesOp::AesDecrypt128EcbNative,
    EvaluatedBytesOp::AesDecrypt192EcbNative,
    EvaluatedBytesOp::AesDecrypt256EcbNative,
    EvaluatedBytesOp::AesEncrypt128CbcNative,
    EvaluatedBytesOp::AesEncrypt192CbcNative,
    EvaluatedBytesOp::AesEncrypt256CbcNative,
    EvaluatedBytesOp::AesDecrypt128CbcNative,
    EvaluatedBytesOp::AesDecrypt192CbcNative,
    EvaluatedBytesOp::AesDecrypt256CbcNative,
    EvaluatedBytesOp::AesEncrypt128OfbNative,
    EvaluatedBytesOp::AesEncrypt192OfbNative,
    EvaluatedBytesOp::AesEncrypt256OfbNative,
    EvaluatedBytesOp::AesDecrypt128OfbNative,
    EvaluatedBytesOp::AesDecrypt192OfbNative,
    EvaluatedBytesOp::AesDecrypt256OfbNative,
    EvaluatedBytesOp::AesEncrypt128CfbNative,
    EvaluatedBytesOp::AesEncrypt192CfbNative,
    EvaluatedBytesOp::AesEncrypt256CfbNative,
    EvaluatedBytesOp::AesDecrypt128CfbNative,
    EvaluatedBytesOp::AesDecrypt192CfbNative,
    EvaluatedBytesOp::AesDecrypt256CfbNative,
    EvaluatedBytesOp::AesNullNative,
    EvaluatedBytesOp::CompareNullNative,
    EvaluatedBytesOp::CompareMissingLegacy,
    EvaluatedBytesOp::GroupingBitAndNative,
    EvaluatedBytesOp::GroupingNumericCmpNative,
    EvaluatedBytesOp::GroupingNumericSetNative,
    EvaluatedBytesOp::GroupingNullNative,
    EvaluatedBytesOp::JsonContainsSerdeNative,
    EvaluatedBytesOp::JsonContainsPathSerdeNative,
    EvaluatedBytesOp::JsonOverlapsSerdeNative,
    EvaluatedBytesOp::JsonMemberOfSerdeNative,
    EvaluatedBytesOp::JsonLengthSerdeNative,
    EvaluatedBytesOp::JsonLengthPathSerdeNative,
    EvaluatedBytesOp::JsonPathExistsSerdeNative,
    EvaluatedBytesOp::JsonMemberOfBinaryLegacy,
    EvaluatedBytesOp::JsonPredicateNullNative,
    EvaluatedBytesOp::JsonPredicateMissingLegacy,
    EvaluatedBytesOp::JsonArraySerdeNative,
    EvaluatedBytesOp::JsonObjectSerdeNative,
    EvaluatedBytesOp::JsonKeysSerdeNative,
    EvaluatedBytesOp::JsonKeysPathSerdeNative,
    EvaluatedBytesOp::JsonPrettySerdeNative,
    EvaluatedBytesOp::JsonOutputNullNative,
    EvaluatedBytesOp::JsonExtractSerdeNative,
    EvaluatedBytesOp::JsonInsertSerdeNative,
    EvaluatedBytesOp::JsonSetSerdeNative,
    EvaluatedBytesOp::JsonReplaceSerdeNative,
    EvaluatedBytesOp::JsonRemoveSerdeNative,
    EvaluatedBytesOp::JsonArrayAppendSerdeNative,
    EvaluatedBytesOp::JsonArrayInsertSerdeNative,
    EvaluatedBytesOp::JsonReplaceRawLegacy,
    EvaluatedBytesOp::JsonArrayAppendRawLegacy,
    EvaluatedBytesOp::JsonArrayAppendEmptyLegacy,
    EvaluatedBytesOp::JsonValueAbsentLegacy,
    EvaluatedBytesOp::JsonUnquoteTextNative,
    EvaluatedBytesOp::JsonUnquoteBinaryNative,
    EvaluatedBytesOp::UtcDateNative,
    EvaluatedBytesOp::UtcTimestampNative,
    EvaluatedBytesOp::CurrentTimeWithoutFspNative,
    EvaluatedBytesOp::CurrentTimeWithFspNative,
    EvaluatedBytesOp::UtcTimeWithoutFspNative,
    EvaluatedBytesOp::UtcTimeWithFspNative,
    EvaluatedBytesOp::UtcTimeNullNative,
    EvaluatedBytesOp::JsonMergeSerdeNative,
    EvaluatedBytesOp::JsonMergePatchSerdeNative,
    EvaluatedBytesOp::JsonMergePatchRawLegacy,
    EvaluatedBytesOp::NowNative,
    EvaluatedBytesOp::CurrentDateNative,
    EvaluatedBytesOp::SysdateNative,
    EvaluatedBytesOp::DateCoreNative,
    EvaluatedBytesOp::DateCorePredicateLegacy,
    EvaluatedBytesOp::WeightStringNative,
    EvaluatedBytesOp::WeightStringCharNative,
    EvaluatedBytesOp::WeightStringBinaryNative,
    EvaluatedBytesOp::WeightStringNumericNative,
    EvaluatedBytesOp::FormatLocaleNative,
    EvaluatedBytesOp::AnyValueNative,
    EvaluatedBytesOp::NameConstNative,
    EvaluatedBytesOp::TidbParseTsoNative,
    EvaluatedBytesOp::TimeDiffTextNative,
    EvaluatedBytesOp::IntDivIntSsNative,
    EvaluatedBytesOp::IntDivIntUsNative,
    EvaluatedBytesOp::IntDivIntSuNative,
    EvaluatedBytesOp::IntDivIntUuNative,
    EvaluatedBytesOp::IntDivInt128Legacy,
    EvaluatedBytesOp::IntDivDecimalSignedNative,
    EvaluatedBytesOp::IntDivDecimalUnsignedNative,
    EvaluatedBytesOp::IntDivDecimalLegacy,
    EvaluatedBytesOp::TimeNative,
    EvaluatedBytesOp::MicrosecondNative,
    EvaluatedBytesOp::MicrosecondLegacy,
    EvaluatedBytesOp::AddTimeNative,
    EvaluatedBytesOp::SubTimeNative,
    EvaluatedBytesOp::TimeAddRightDatetimeNative,
    EvaluatedBytesOp::TimestampAddNative,
    EvaluatedBytesOp::TimestampAddPrefixNullNative,
    EvaluatedBytesOp::JsonSearchSerdeNative,
    EvaluatedBytesOp::DateLiteralNative,
    EvaluatedBytesOp::TimestampLiteralNative,
    EvaluatedBytesOp::ConvertTzNative,
    EvaluatedBytesOp::Timestamp1Native,
    EvaluatedBytesOp::Timestamp2BaseNative,
    EvaluatedBytesOp::Timestamp2AddNative,
    EvaluatedBytesOp::TimestampNullNative,
    EvaluatedBytesOp::UnixTimestampNowNative,
    EvaluatedBytesOp::UnixTimestampNullNative,
    EvaluatedBytesOp::UnixTimestampParseNative,
    EvaluatedBytesOp::UnixTimestampValueNative,
    EvaluatedBytesOp::UnixTimestampIntLegacy,
    EvaluatedBytesOp::UnixTimestampDecLegacy,
    EvaluatedBytesOp::FromUnixTimeNumericNative,
    EvaluatedBytesOp::FromUnixTimeTextNative,
    EvaluatedBytesOp::FromUnixTimeLocalNative,
    EvaluatedBytesOp::FromUnixTimeLegacy,
    EvaluatedBytesOp::FromUnixTimeNullNative,
    EvaluatedBytesOp::IfNullHeadNative,
    EvaluatedBytesOp::IfNullFinishNative,
    EvaluatedBytesOp::IfHeadNative,
    EvaluatedBytesOp::IfFinishNative,
    EvaluatedBytesOp::CoalesceEndNative,
    EvaluatedBytesOp::NullIfNative,
    EvaluatedBytesOp::CastRealUnsignedNative,
    EvaluatedBytesOp::BoundedStalenessHeadNative,
    EvaluatedBytesOp::BoundedStalenessFinishNative,
    EvaluatedBytesOp::TimestampDiffTextNative,
    EvaluatedBytesOp::TimestampDiffCoreNative,
    EvaluatedBytesOp::ToBinaryNative,
    EvaluatedBytesOp::FromBinaryNative,
    EvaluatedBytesOp::ConvertUsingNative,
    EvaluatedBytesOp::StrToDateHeadNative,
    EvaluatedBytesOp::StrToDateFinishNative,
    EvaluatedBytesOp::StrToDateTypedFinishNative,
    EvaluatedBytesOp::JsonSumCrc32SerdeNative,
    EvaluatedBytesOp::ExtractSelectNative,
    EvaluatedBytesOp::ExtractDatetimeNative,
    EvaluatedBytesOp::ExtractDurationNative,
    EvaluatedBytesOp::ExtractMixedDurationNative,
    EvaluatedBytesOp::ExtractMixedFinishNative,
    EvaluatedBytesOp::ExtractCompositeNative,
    EvaluatedBytesOp::ExtremumHeadNative,
    EvaluatedBytesOp::ExtremumNumericNative,
    EvaluatedBytesOp::ExtremumTimeNative,
    EvaluatedBytesOp::ExtremumVectorNative,
    EvaluatedBytesOp::ExtremumStringNative,
    EvaluatedBytesOp::ExtremumTimeTextNative,
    EvaluatedBytesOp::ExtremumTimeContextNative,
    EvaluatedBytesOp::ExtremumFinishNative,
    EvaluatedBytesOp::IntervalEagerHeadNative,
    EvaluatedBytesOp::IntervalLazyHeadNative,
    EvaluatedBytesOp::IntervalStepNative,
    EvaluatedBytesOp::DateArithmeticHeadNative,
    EvaluatedBytesOp::DateArithmeticDurationHeadNative,
    EvaluatedBytesOp::DateArithmeticStepNative,
    EvaluatedBytesOp::DateArithmeticOverflowNative,
    EvaluatedBytesOp::LegacyDateArithmeticTextHeadNative,
    EvaluatedBytesOp::LegacyDateArithmeticTimeHeadNative,
    EvaluatedBytesOp::LegacyDateArithmeticDurationHeadNative,
    EvaluatedBytesOp::LegacyDateArithmeticStepNative,
    EvaluatedBytesOp::LegacyDateArithmeticParseNative,
    EvaluatedBytesOp::InTypedValuesNative,
    EvaluatedBytesOp::InLegacyIntHeadNative,
    EvaluatedBytesOp::InLegacyStringHeadNative,
    EvaluatedBytesOp::InLegacyStepNative,
];

fn ready(operation: EvaluatedBytesOp, case: usize) -> Vec<ScalarValue> {
    operation
        .input_types()
        .iter()
        .enumerate()
        .map(|(slot, kind)| {
            let n = [0, 0, 1, -1, 2, 7, 3, 12][(case + slot) % 8];
            match kind {
                EvalType::Int => ScalarValue::Int((case != 0).then_some(n)),
                EvalType::Decimal => ScalarValue::Decimal((case != 0).then(|| Decimal::from(n))),
                EvalType::VectorFloat32 => ScalarValue::VectorFloat32(None),
                EvalType::Bytes => ScalarValue::Bytes(if case == 0 {
                    None
                } else {
                    Some(match case {
                        1 => vec![],
                        2 => b"hello".to_vec(),
                        3 => b"12".to_vec(),
                        4 => vec![0xff, 0],
                        5 => 1f64.to_bits().to_le_bytes().to_vec(),
                        6 => 1i128.to_le_bytes().to_vec(),
                        _ => b"[1,2]".to_vec(),
                    })
                }),
                _ => unreachable!(),
            }
        })
        .collect()
}

fn observe(operation: EvaluatedBytesOp, case: usize, profile: usize) -> (String, u64) {
    let program = compile_evaluated_bytes(operation, LocalCompileContext::default()).unwrap();
    let values = ready(operation, case);
    let mut config = EvalConfig::default();
    if profile == 1 {
        config.set_flag(tidb_query_datatype::expr::Flag::IGNORE_TRUNCATE);
    }
    if profile == 2 {
        config.set_flag(tidb_query_datatype::expr::Flag::TRUNCATE_AS_WARNING);
    }
    let mut ctx = EvalContext::new(Arc::new(config));
    let mut witness = ReadyValueDispatchWitness::default();

    let result = catch_unwind(AssertUnwindSafe(|| {
        let result = program.expression.eval_with_ready_args(
            operation,
            &mut ctx,
            &program.schema,
            &values,
            operation.input_role(),
            &[0],
            &mut witness,
        );
        result
            .map(|node| format!("{:?}", node.get_logical_scalar_ref(0)))
            .map_err(|e| format!("{e:?}"))
    }));
    let result = match result {
        Ok(v) => format!("{v:?}"),
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| p.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic");
            format!("PANIC:{msg}")
        }
    };
    (
        format!("{result}|{:?}", ctx.warnings),
        witness.invocations(),
    )
}

#[test]
fn ready_kernel_original_observations() {
    // Independent pre-change capture: only the retired Decimal work marker
    // changes admission. Preserve every other historical record and the old digest.
    let policy: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("ready_policy_observations.json")).unwrap();
    let mut policy_records = 0;
    let mut policy_successes = 0;

    let mut hash = 0xcbf29ce484222325u64;
    let mut observations = 0;
    let mut dispatched = 0;
    let mut successful = 0;
    for &op in OPERATIONS {
        for case in 0..8 {
            for profile in 0..3 {
                let (value, calls) = observe(op, case, profile);
                let actual = format!("{op:?}|{case}|{profile}|{value}|{calls}\n");
                let key = format!("{op:?}");
                let delta = policy.iter().find(|entry| {
                    entry["operation"] == key
                        && entry["case"] == case
                        && entry["profile"] == profile
                });
                let record = if let Some(delta) = delta {
                    assert!(matches!(
                        ready(op, case).last(),
                        Some(ScalarValue::Int(Some(-1)))
                    ));
                    assert_eq!(
                        actual.trim_end_matches('\n'),
                        delta["after"].as_str().unwrap()
                    );
                    policy_records += 1;
                    policy_successes += usize::from(value.starts_with("Ok("));
                    format!("{}\n", delta["before"].as_str().unwrap())
                } else {
                    successful += usize::from(value.starts_with("Ok("));
                    dispatched += usize::from(calls > 0);
                    actual
                };
                for b in record.bytes() {
                    hash = (hash ^ u64::from(b)).wrapping_mul(0x100000001b3);
                }
                observations += 1;
            }
        }
    }
    assert_eq!(
        (policy.len(), policy_records, policy_successes),
        (60, 60, 54)
    );
    assert_eq!(
        (observations, hash, dispatched, successful, OPERATIONS.len()),
        (11448, 0x8992E4132C233F8C, 6561, 5271, 477)
    );
    eprintln!(
        "READY KERNEL ORIGINAL {observations} {hash:016X} dispatched={dispatched} successful={successful} operations={}",
        OPERATIONS.len()
    );
}

#[test]
fn ready_kernel_public_worker_never_starts_nested_frames() {
    let before = crate::types::expr_eval::ready_kernel_frame_entries();
    let mut worker = prepare_evaluated_bytes(
        EvaluatedBytesOp::Length,
        LocalCompileContext::default(),
        ExecutionLimits::default(),
        16 * 1024 * 1024,
    )
    .unwrap();
    assert!(
        worker
            .eval_args(EvaluatedArgs::Bytes(Some(b"hello".to_vec())))
            .is_ok()
    );
    assert_eq!(
        crate::types::expr_eval::ready_kernel_frame_entries() - before,
        0
    );
}

#[test]
fn ready_kernel_budget_refusals_precede_exact_dispatch_and_do_not_replay() {
    for (operation, maximum) in [
        (EvaluatedBytesOp::Length, 0),
        (EvaluatedBytesOp::Length, 1),
        (EvaluatedBytesOp::IsNotNull, 2),
    ] {
        let limits = ExecutionLimits {
            max_steps: maximum,
            ..ExecutionLimits::default()
        };
        let mut worker =
            prepare_evaluated_bytes(operation, LocalCompileContext::default(), limits, 16 << 20)
                .unwrap();
        for invocation in 1..=2 {
            let args = if operation == EvaluatedBytesOp::Length {
                EvaluatedArgs::Bytes(Some(b"abc".to_vec()))
            } else {
                EvaluatedArgs::Int(Some(1))
            };
            worker.eval_args(args).unwrap();
            assert_eq!(
                worker.kernel_invocations(),
                operation.call_count() as u64 * invocation
            );
            assert!(worker.is_healthy());
        }
    }
    for (operation, maximum, args) in [
        (
            EvaluatedBytesOp::Length,
            2,
            EvaluatedArgs::Bytes(Some(b"abc".to_vec())),
        ),
        (EvaluatedBytesOp::IsNotNull, 3, EvaluatedArgs::Int(Some(1))),
    ] {
        let mut worker = prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits {
                max_steps: maximum,
                ..ExecutionLimits::default()
            },
            16 << 20,
        )
        .unwrap();
        assert!(worker.eval_args(args).is_ok());
        assert_eq!(worker.kernel_invocations(), operation.call_count() as u64);
    }
}
