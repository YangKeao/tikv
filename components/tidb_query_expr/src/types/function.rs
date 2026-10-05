// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

//! People implementing RPN functions with fixed argument type and count don't
//! necessarily need to understand how `Evaluator` and `RpnDef` work. There's a
//! procedural macro called `rpn_fn` defined in `tidb_query_codegen` to help you
//! create RPN functions. For example:
//!
//! ```ignore
//! use tidb_query_codegen::rpn_fn;
//!
//! #[rpn_fn(nullable)]
//! fn foo(lhs: &Option<Int>, rhs: &Option<Int>) -> Result<Option<Int>> {
//!     // Your RPN function logic
//! }
//! ```
//!
//! You can still call the `foo` function directly; the macro preserves the
//! original function It creates a `foo_fn_meta()` function (simply add
//! `_fn_meta` to the original function name) which generates an `RpnFnMeta`
//! struct.
//!
//! For more information on the procedural macro, see the documentation in
//! `components/tidb_query_codegen/src/rpn_function`.

use std::{any::Any, convert::TryFrom, marker::PhantomData};

use static_assertions::assert_eq_size;
use tidb_query_common::Result;
use tidb_query_datatype::{EvalType, FieldTypeAccessor, codec::data_type::*, expr::EvalContext};
use tipb::{Expr, ExprType, FieldType, ScalarFuncSig};

use super::{RpnStackNode, expr_eval::LogicalRows};
use crate::RpnExpression;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlKind {
    And,
    Or,
    If,
    IfNull,
    CaseWhen,
    Coalesce,
}

impl ControlKind {
    pub(crate) fn is_logical(self) -> bool {
        matches!(self, Self::And | Self::Or)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ShortCircuitFnMeta {
    pub sig: ScalarFuncSig,
    // Scheduling belongs to the official iterative driver, not a callback
    // which recursively invokes another evaluator.
    pub(crate) kind: ControlKind,
}

/// Selector-only information, deliberately not part of the public RpnFnMeta
/// constructed by rpn_fn (including consumers outside this crate).
pub(crate) struct SelectedCall {
    pub(crate) func_meta: RpnFnMeta,
    pub(crate) control: Option<ControlKind>,
}

impl From<RpnFnMeta> for SelectedCall {
    fn from(func_meta: RpnFnMeta) -> Self {
        Self {
            func_meta,
            control: None,
        }
    }
}

/// Metadata of an RPN function.
#[derive(Clone, Copy)]
pub struct RpnFnMeta {
    /// The display name of the RPN function. Mainly used in tests.
    pub name: &'static str,

    /// Validator shared by wire and typed local construction.
    pub(crate) validator_ptr: fn(call: &CallShape) -> Result<()>,

    /// The sole metadata constructor, operating on original argument
    /// identities.
    pub(crate) metadata_ptr: fn(call: &mut CallBuild) -> Result<Box<dyn Any + Send>>,

    #[allow(clippy::type_complexity)]
    /// The RPN function.
    pub fn_ptr: fn(
        // Common arguments
        ctx: &mut EvalContext,
        output_rows: usize,
        args: &[RpnStackNode<'_>],
        // Uncommon arguments are grouped together
        extra: &mut RpnFnCallExtra<'_>,
        metadata: &(dyn Any + Send),
    ) -> Result<VectorValue>,
}

impl std::fmt::Debug for RpnFnMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

/// A function identity for in-process construction, without reserving wire IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FunctionRef {
    TiPb(ScalarFuncSig),
    Local(LocalFunctionId),
}

/// Closed, TiKV-owned local signatures. Each variant has a checked support
/// domain. The raw math, nullable IP, native, legacy, and ready variants are
/// non-wire, factory-only identities, not ordinary local kernels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalFunctionId {
    NullIfIntSignedSigned,
    AsinRaw,
    AcosRaw,
    SqrtRaw,
    SignRaw,
    RadiansRaw,
    DegreesRaw,
    PiRaw,
    IsIpv4Nullable,
    IsIpv6Nullable,
    IsIpv4CompatNullable,
    IsIpv4MappedNullable,
    SpaceNative,
    RepeatNative,
    ToBase64Native,
    FromBase64Native,
    FromBase64ValueNative,
    LowerUtf8Ready,
    UpperUtf8Ready,
    Sha2Native,
    OrdNative,
    TrimBothNative,
    TrimLeadingNative,
    TrimTrailingNative,
    SubstringIndexSignedNative,
    SubstringIndexUnsignedNative,
    LpadBytesNative,
    RpadBytesNative,
    LpadUtf8Native,
    RpadUtf8Native,
    LnNative,
    LogNative,
    Log2Native,
    PowNative,
    UncompressedLengthNative,
    InsertUtf8Native,
    LowerAsciiNative,
    UpperAsciiNative,
    Substring2BytesNative,
    Substring3BytesNative,
    Substring2Utf8Native,
    Substring3Utf8Native,
    Substring2BytesLegacy,
    Substring3BytesLegacy,
    Substring2Utf8Legacy,
    Substring3Utf8Legacy,
    StrcmpNative,
    Locate2Native,
    Locate3Native,
    Locate3BytesExtNative,
    Locate3Utf8ExtNative,
    FindInSetNative,
    FindInSetPreparedNative,
    OctStringNative,
    ConcatNative,
    ConcatWsNative,
    EltNative,
    FieldBytesNative,
    FieldIntNative,
    FieldRealNative,
    MakeSetNative,
    ExportSetNative,
    AbsIntNative,
    AbsUIntNative,
    AbsRealNative,
    AbsDecimalNative,
    CeilIntNative,
    FloorIntNative,
    CeilRealNative,
    FloorRealNative,
    CeilDecimalNative,
    FloorDecimalNative,
    RoundIntNative,
    RoundIntWithScaleNative,
    RoundRealNative,
    RoundDecimalNative,
    TruncateIntNative,
    TruncateUIntNative,
    TruncateIntUnsignedScaleNative,
    TruncateRealNative,
    TruncateDecimalNative,
    RoundInt128Legacy,
    RoundRealLegacy,
    RoundDecimalLegacy,
    MathNullWitnessNative,
    CharNative,
    ConvNative,
    ConvBinaryLiteralNative,
    ConvLegacy,
    SinGoNative,
    CosGoNative,
    TanGoNative,
    CotGoNative,
    AtanGoNative,
    Atan2GoNative,
    SinLibmLegacy,
    CosLibmLegacy,
    CotLibmLegacy,
    AtanLibmLegacy,
    Atan2LibmLegacy,
    ExpGoNative,
    Log10GoNative,
    CompressGoNative,
    UncompressNative,
    JsonValidTextNative,
    JsonValidBinaryNative,
    JsonValidOtherNative,
    JsonTypeTextNative,
    JsonTypeBinaryNative,
    JsonDepthNative,
    JsonStorageFreeNative,
    JsonStorageSizeNative,
    JsonQuoteNative,
    YearCoreNative,
    MonthCoreNative,
    DayOfMonthCoreNative,
    QuarterCoreNative,
    HourTextNative,
    MinuteTextNative,
    SecondTextNative,
    HourNanosNative,
    MinuteNanosNative,
    SecondNanosNative,
    MonthNameTextNative,
    TimeToSecTextNative,
    PeriodAddNative,
    PeriodDiffNative,
    GetFormatNative,
    GetFormatNullNative,
    DayOfWeekTextNative,
    WeekdayTextNative,
    DayOfYearTextNative,
    DayNameTextNative,
    DateDiffTextNative,
    DateDiffNullNative,
    DateDiffCoreNative,
    ToDaysTextNative,
    ToSecondsTextNative,
    TsoLogicalNative,
    WeekDateTextNative,
    WeekTextNative,
    YearWeekTextNative,
    WeekOfYearTextNative,
    WeekNullNative,
    WeekCoreNative,
    PasswordNative,
    Sm3Native,
    MakeDateNative,
    FromDaysNative,
    MakeTimePartsNative,
    SecToTimeNative,
    DateFormatTextNative,
    DateFormatCoreNative,
    DateFormatNullNative,
    DateFormatMissingNative,
    DurationTextProbeNative,
    TimeFormatTextNative,
    LastDayTextNative,
    IsUuidNative,
    UuidVersionNative,
    UuidTimestampNative,
    UuidToBinParseNative,
    UuidToBinSwapNative,
    BinToUuidNative,
    TranslateUtf8Native,
    TranslateBinaryNative,
    TranslateNullNative,
    SqlEncodeNative,
    SqlDecodeNative,
    SqlCryptNullNative,
    TidbShardNative,
    VitessHashNative,
    FormatBytesNative,
    FormatNanoTimeNative,
    VecAsTextNative,
    VecDimsNative,
    VecL1DistanceNative,
    VecL2DistanceNative,
    VecNegativeInnerProductNative,
    VecCosineDistanceNative,
    VecL2NormNative,
    VecFromTextNative,
    VecRealNullNative,
    LikeNative,
    IlikeNative,
    LikeLegacyNative,
    LikeNullIntNative,
    LikeMissingLegacyNative,
    RegexpLikeNative,
    RegexpSubstrNative,
    RegexpInstrNative,
    RegexpReplaceNative,
    RegexpLikeLegacyCiNative,
    RegexpLikeLegacyBinNative,
    RegexpNullIntNative,
    RegexpNullBytesNative,
    RegexpMissingLegacyNative,
    UnaryPlusIntNative,
    UnaryPlusBitsNative,
    UnaryPlusDecimalNative,
    UnaryPlusBytesNative,
    UnaryMinusIntNative,
    UnaryMinusUIntNative,
    UnaryMinusIntConstantNative,
    UnaryMinusUIntConstantNative,
    UnaryMinusBitsNative,
    UnaryMinusDecimalNative,
    UnaryNullNative,
    AddIntSsNative,
    AddIntSuNative,
    AddIntUsNative,
    AddIntUuNative,
    SubIntSsNative,
    SubIntSuNative,
    SubIntUsNative,
    SubIntUuNative,
    SubIntSuForcedNative,
    SubIntUsForcedNative,
    SubIntUuForcedNative,
    MulIntSignedNative,
    MulIntUnsignedNative,
    AddRealNative,
    SubRealNative,
    MulRealNative,
    AddDecimalNative,
    SubDecimalNative,
    MulDecimalNative,
    AddVectorNative,
    SubVectorNative,
    MulVectorNative,
    BinaryArithmeticNullNative,
    AddInt128SignedLegacy,
    AddInt128UnsignedLegacy,
    AddInt128RejectLeftLegacy,
    AddInt128RejectRightLegacy,
    SubInt128SignedLegacy,
    SubInt128UnsignedLegacy,
    SubInt128RejectLeftLegacy,
    SubInt128RejectRightLegacy,
    MulInt128SignedLegacy,
    MulInt128UnsignedLegacy,
    AddRealLegacy,
    SubRealLegacy,
    MulRealLegacy,
    AddDecimalLegacy,
    SubDecimalLegacy,
    MulDecimalLegacy,
    BinaryArithmeticMissingLegacy,
    AddDecimalFastNative,
    SubDecimalFastNative,
    MulDecimalFastNative,
    ModIntSsNative,
    ModIntSuNative,
    ModIntUsNative,
    ModIntUuNative,
    ModInt128Legacy,
    ModRealNative,
    ModRealLegacy,
    ModDecimalNative,
    DivRealNative,
    DivRealLegacy,
    DivDecimalNative,
    DivDecimalLegacy,
    AesEncrypt128EcbNative,
    AesEncrypt192EcbNative,
    AesEncrypt256EcbNative,
    AesDecrypt128EcbNative,
    AesDecrypt192EcbNative,
    AesDecrypt256EcbNative,
    AesEncrypt128CbcNative,
    AesEncrypt192CbcNative,
    AesEncrypt256CbcNative,
    AesDecrypt128CbcNative,
    AesDecrypt192CbcNative,
    AesDecrypt256CbcNative,
    AesEncrypt128OfbNative,
    AesEncrypt192OfbNative,
    AesEncrypt256OfbNative,
    AesDecrypt128OfbNative,
    AesDecrypt192OfbNative,
    AesDecrypt256OfbNative,
    AesEncrypt128CfbNative,
    AesEncrypt192CfbNative,
    AesEncrypt256CfbNative,
    AesDecrypt128CfbNative,
    AesDecrypt192CfbNative,
    AesDecrypt256CfbNative,
    AesNullNative,
    CompareIntSsNative(crate::ComparisonOp),
    CompareIntSuNative(crate::ComparisonOp),
    CompareIntUsNative(crate::ComparisonOp),
    CompareIntUuNative(crate::ComparisonOp),
    CompareInt128Legacy(crate::ComparisonOp),
    CompareRealNative(crate::ComparisonOp),
    CompareRealLegacy(crate::ComparisonOp),
    CompareDecimalNative(crate::ComparisonOp),
    CompareBytesNative(crate::ComparisonOp),
    CompareVectorNative(crate::ComparisonOp),
    CompareTimeCoreNative(crate::ComparisonOp),
    CompareDurationNative(crate::ComparisonOp),
    CompareJsonNative(crate::ComparisonOp),
    CompareNullNative,
    CompareMissingLegacy,
    GroupingBitAndNative,
    GroupingNumericCmpNative,
    GroupingNumericSetNative,
    GroupingNullNative,
    JsonContainsSerdeNative,
    JsonContainsPathSerdeNative,
    JsonOverlapsSerdeNative,
    JsonMemberOfSerdeNative,
    JsonLengthSerdeNative,
    JsonLengthPathSerdeNative,
    JsonPathExistsSerdeNative,
    JsonMemberOfBinaryLegacy,
    JsonPredicateNullNative,
    JsonPredicateMissingLegacy,
    JsonArraySerdeNative,
    JsonObjectSerdeNative,
    JsonKeysSerdeNative,
    JsonKeysPathSerdeNative,
    JsonPrettySerdeNative,
    JsonOutputNullNative,
    JsonExtractSerdeNative,
    JsonInsertSerdeNative,
    JsonSetSerdeNative,
    JsonReplaceSerdeNative,
    JsonRemoveSerdeNative,
    JsonArrayAppendSerdeNative,
    JsonArrayInsertSerdeNative,
    JsonReplaceRawLegacy,
    JsonArrayAppendRawLegacy,
    JsonArrayAppendEmptyLegacy,
    JsonValueAbsentLegacy,
    JsonUnquoteTextNative,
    JsonUnquoteBinaryNative,
    UtcDateNative,
    UtcTimestampNative,
    CurrentTimeWithoutFspNative,
    CurrentTimeWithFspNative,
    UtcTimeWithoutFspNative,
    UtcTimeWithFspNative,
    UtcTimeNullNative,
    JsonMergeSerdeNative,
    JsonMergePatchSerdeNative,
    JsonMergePatchRawLegacy,
    NowNative,
    CurrentDateNative,
    SysdateNative,
    DateCoreNative,
    DateCorePredicateLegacy,
    WeightStringNative,
    WeightStringCharNative,
    WeightStringBinaryNative,
    WeightStringNumericNative,
    FormatLocaleNative,
    AnyValueNative,
    NameConstNative,
    TidbParseTsoNative,
    TimeDiffTextNative,
    IntDivIntSsNative,
    IntDivIntUsNative,
    IntDivIntSuNative,
    IntDivIntUuNative,
    IntDivInt128Legacy,
    IntDivDecimalSignedNative,
    IntDivDecimalUnsignedNative,
    IntDivDecimalLegacy,
    TimeNative,
    MicrosecondNative,
    MicrosecondLegacy,
    AddTimeNative,
    SubTimeNative,
    TimeAddRightDatetimeNative,
    TimestampAddNative,
    TimestampAddPrefixNullNative,
    JsonSearchSerdeNative,
    DateLiteralNative,
    TimestampLiteralNative,
    ConvertTzNative,
    Timestamp1Native,
    Timestamp2BaseNative,
    Timestamp2AddNative,
    TimestampNullNative,
    UnixTimestampNowNative,
    UnixTimestampNullNative,
    UnixTimestampParseNative,
    UnixTimestampValueNative,
    UnixTimestampIntLegacy,
    UnixTimestampDecLegacy,
    FromUnixTimeNumericNative,
    FromUnixTimeTextNative,
    FromUnixTimeLocalNative,
    FromUnixTimeLegacy,
    FromUnixTimeNullNative,
    IfNullHeadNative,
    IfNullFinishNative,
    IfHeadNative,
    IfFinishNative,
    CoalesceEndNative,
    NullIfNative,
    CastRealUnsignedNative,
    BoundedStalenessHeadNative,
    BoundedStalenessFinishNative,
    TimestampDiffTextNative,
    TimestampDiffCoreNative,
    ToBinaryNative,
    FromBinaryNative,
    ConvertUsingNative,
    StrToDateHeadNative,
    StrToDateFinishNative,
    StrToDateTypedFinishNative,
    JsonSumCrc32SerdeNative,
    ExtractSelectNative,
    ExtractDatetimeNative,
    ExtractDurationNative,
    ExtractMixedDurationNative,
    ExtractMixedFinishNative,
    ExtractCompositeNative,
    ExtremumHeadNative,
    ExtremumNumericNative,
    ExtremumTimeNative,
    ExtremumVectorNative,
    ExtremumStringNative,
    ExtremumTimeTextNative,
    ExtremumTimeContextNative,
    ExtremumFinishNative,
    IntervalEagerHeadNative,
    IntervalLazyHeadNative,
    IntervalStepNative,
    DateArithmeticHeadNative,
    DateArithmeticDurationHeadNative,
    DateArithmeticStepNative,
    DateArithmeticOverflowNative,
}

/// Source provenance, not a deduction from the value's collation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiteralKind {
    Typed,
    Text,
    BinaryLiteral,
}

/// Public metadata inputs are typed; compiled `Any` metadata is never supplied
/// by a local caller.
#[derive(Clone, Debug, Default)]
pub enum CallMetadata {
    #[default]
    None,
    InUnion {
        in_union: bool,
    },
}

#[derive(Clone, Debug)]
enum CallArgValue {
    Wire(ExprType, Vec<u8>),
    Constant(ScalarValue, LiteralKind),
    Dynamic,
}

/// A shallow argument descriptor. It never contains an evaluable child program.
#[derive(Clone, Debug)]
pub(crate) struct CallArg {
    field_type: FieldType,
    value: CallArgValue,
}

impl CallArg {
    pub(crate) fn from_expr(expr: &Expr) -> Self {
        Self {
            field_type: expr.get_field_type().clone(),
            value: CallArgValue::Wire(expr.get_tp(), expr.get_val().to_vec()),
        }
    }

    pub(crate) fn constant(value: ScalarValue, field_type: FieldType, kind: LiteralKind) -> Self {
        Self {
            field_type,
            value: CallArgValue::Constant(value, kind),
        }
    }

    pub(crate) fn dynamic(field_type: FieldType) -> Self {
        Self {
            field_type,
            value: CallArgValue::Dynamic,
        }
    }

    pub(crate) fn field_type(&self) -> &FieldType {
        &self.field_type
    }

    pub(crate) fn constant_bytes(&self) -> Option<&[u8]> {
        match &self.value {
            CallArgValue::Wire(ExprType::Bytes | ExprType::String, value) => Some(value),
            CallArgValue::Constant(ScalarValue::Bytes(Some(value)), _) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn wire_literal(&self) -> Option<(ExprType, &[u8])> {
        match &self.value {
            CallArgValue::Wire(tp, value) => Some((*tp, value)),
            _ => None,
        }
    }

    pub(crate) fn source_type_name(&self) -> String {
        match &self.value {
            CallArgValue::Wire(tp, _) => format!("{:?}", tp),
            CallArgValue::Constant(value, _) => format!("{:?}", value.eval_type()),
            CallArgValue::Dynamic => "dynamic input".to_owned(),
        }
    }

    pub(crate) fn scalar_literal(&self) -> Option<&ScalarValue> {
        match &self.value {
            CallArgValue::Constant(value, _) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn is_dynamic(&self) -> bool {
        matches!(
            self.value,
            CallArgValue::Dynamic
                | CallArgValue::Wire(ExprType::ScalarFunc | ExprType::ColumnRef, _)
        )
    }

    pub(crate) fn is_null_literal(&self) -> bool {
        match &self.value {
            CallArgValue::Wire(ExprType::Null, _) => true,
            CallArgValue::Constant(value, _) => value.is_none(),
            _ => false,
        }
    }

    pub(crate) fn uses_binary_literal_cast(&self) -> Result<bool> {
        match &self.value {
            CallArgValue::Wire(tp, _) => {
                Ok(wire_expr_type_is_scalar(*tp)? && self.field_type.is_binary_string_like())
            }
            CallArgValue::Constant(_, kind) => Ok(*kind == LiteralKind::BinaryLiteral),
            CallArgValue::Dynamic => Ok(false),
        }
    }
}

pub(crate) fn wire_expr_type_is_scalar(tp: ExprType) -> Result<bool> {
    match tp {
        ExprType::Null
        | ExprType::Int64
        | ExprType::Uint64
        | ExprType::String
        | ExprType::Bytes
        | ExprType::Float32
        | ExprType::Float64
        | ExprType::MysqlTime
        | ExprType::MysqlDuration
        | ExprType::MysqlDecimal
        | ExprType::MysqlJson
        | ExprType::MysqlEnum
        | ExprType::TiDbVectorFloat32 => Ok(true),
        ExprType::ScalarFunc | ExprType::ColumnRef => Ok(false),
        _ => Err(other_err!("Unsupported expression type {:?}", tp)),
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CallShape {
    function: FunctionRef,
    return_type: FieldType,
    args: Vec<CallArg>,
}

impl CallShape {
    pub(crate) fn from_expr(expr: &Expr) -> Self {
        Self::new(
            FunctionRef::TiPb(expr.get_sig()),
            expr.get_field_type().clone(),
            expr.get_children().iter().map(CallArg::from_expr).collect(),
        )
    }

    pub(crate) fn new(function: FunctionRef, return_type: FieldType, args: Vec<CallArg>) -> Self {
        Self {
            function,
            return_type,
            args,
        }
    }
    pub(crate) fn function(&self) -> FunctionRef {
        self.function
    }
    pub(crate) fn return_type(&self) -> &FieldType {
        &self.return_type
    }
    pub(crate) fn args(&self) -> &[CallArg] {
        &self.args
    }
}

enum MetadataSource {
    Wire(Vec<u8>),
    Local(CallMetadata),
}

/// Retention is independent of immutable source-order argument facts.
pub(crate) struct CallBuild {
    shape: CallShape,
    metadata: MetadataSource,
    retained_args: Vec<usize>,
}

impl CallBuild {
    pub(crate) fn from_expr(expr: &Expr) -> Self {
        Self {
            shape: CallShape::from_expr(expr),
            metadata: MetadataSource::Wire(expr.get_val().to_vec()),
            retained_args: (0..expr.get_children().len()).collect(),
        }
    }

    pub(crate) fn local(shape: CallShape, metadata: CallMetadata) -> Self {
        let retained_args = (0..shape.args.len()).collect();
        Self {
            shape,
            metadata: MetadataSource::Local(metadata),
            retained_args,
        }
    }

    pub(crate) fn shape(&self) -> &CallShape {
        &self.shape
    }
    pub(crate) fn args(&self) -> &[CallArg] {
        self.shape.args()
    }
    pub(crate) fn set_retained_args(&mut self, retained: Vec<usize>) -> Result<()> {
        let mut seen = vec![false; self.args().len()];
        for &index in &retained {
            if index >= seen.len() || seen[index] {
                return Err(other_err!("Invalid retained function argument {}", index));
            }
            seen[index] = true;
        }
        self.retained_args = retained;
        Ok(())
    }
}

/// Only common preparation can pair a function with its validated metadata.
pub(crate) struct PreparedCall {
    function: FunctionRef,
    control: Option<ControlKind>,
    func_meta: RpnFnMeta,
    field_type: FieldType,
    metadata: Box<dyn Any + Send>,
    retained_args: Vec<usize>,
}

/// Opaque ordinary-demand call prepared by the canonical selector/validator.
/// Source/site facts are retained assertions, not proof of native PB ingestion
/// or a site-aware native diagnostic adapter. No evaluator closure is stored.
#[derive(Debug)]
pub struct PreparedOrdinaryCall {
    function: FunctionRef,
    func_meta: RpnFnMeta,
    field_type: FieldType,
    metadata: Box<dyn Any + Send>,
    site: crate::local::OrdinaryCallSite,
}

impl PreparedOrdinaryCall {
    pub fn function(&self) -> FunctionRef {
        self.function
    }
    pub fn return_type(&self) -> &FieldType {
        &self.field_type
    }
    pub fn site(&self) -> &crate::local::OrdinaryCallSite {
        &self.site
    }
    pub(crate) fn kernel(&self) -> (RpnFnMeta, &(dyn Any + Send)) {
        (self.func_meta, &*self.metadata)
    }
}

impl PreparedCall {
    pub(crate) fn into_ordinary(
        self,
        site: crate::local::OrdinaryCallSite,
    ) -> Result<PreparedOrdinaryCall> {
        if self.function != FunctionRef::TiPb(ScalarFuncSig::PlusInt)
            || self.control.is_some()
            || self.retained_args != [0, 1]
            || !self.metadata.is::<()>()
        {
            return Err(other_err!(
                "Ordinary demand requires checked203, unit metadata and source-order arguments"
            ));
        }
        Ok(PreparedOrdinaryCall {
            function: self.function,
            func_meta: self.func_meta,
            field_type: self.field_type,
            metadata: self.metadata,
            site,
        })
    }

    pub(crate) fn short_circuit_meta(&self) -> Option<ShortCircuitFnMeta> {
        let FunctionRef::TiPb(sig) = self.function else {
            return None;
        };
        self.control.map(|kind| ShortCircuitFnMeta { sig, kind })
    }

    pub(crate) fn into_control(
        self,
        args: Box<[RpnExpression]>,
    ) -> Result<crate::RpnExpressionNode> {
        let func_meta = self
            .short_circuit_meta()
            .ok_or_else(|| other_err!("Call is not a checked control"))?;
        if args.len() != self.retained_args.len() {
            return Err(other_err!("Control child count changed after preparation"));
        }
        Ok(crate::RpnExpressionNode::ShortCircuitFnCall {
            func_meta,
            args,
            field_type: self.field_type,
        })
    }

    pub(crate) fn retained_args(&self) -> &[usize] {
        &self.retained_args
    }
    pub(crate) fn into_node(self) -> crate::RpnExpressionNode {
        crate::RpnExpressionNode::FnCall {
            func_meta: self.func_meta,
            args_len: self.retained_args.len(),
            field_type: self.field_type,
            metadata: self.metadata,
        }
    }
}

pub(crate) fn prepare_call(call: &mut CallBuild) -> Result<PreparedCall> {
    let selected = crate::select_call(call.shape())?;
    prepare_selected_call(call, selected)
}

pub(crate) fn prepare_selected_call(
    call: &mut CallBuild,
    selected: SelectedCall,
) -> Result<PreparedCall> {
    let func_meta = selected.func_meta;
    (func_meta.validator_ptr)(call.shape()).map_err(|error| match call.shape.function {
        FunctionRef::TiPb(sig) => other_err!(
            "Invalid {} (sig = {:?}) signature: {}",
            func_meta.name,
            sig,
            error
        ),
        FunctionRef::Local(id) => other_err!(
            "Invalid {} (local = {:?}) signature: {}",
            func_meta.name,
            id,
            error
        ),
    })?;
    let metadata = (func_meta.metadata_ptr)(call)?;
    if selected.control.is_some()
        && (!metadata.is::<()>()
            || call.retained_args.len() != call.args().len()
            || call
                .retained_args
                .iter()
                .enumerate()
                .any(|(position, &index)| position != index))
    {
        return Err(other_err!(
            "Control preparation requires unit metadata and source-order arguments"
        ));
    }
    let retained_shape = CallShape::new(
        call.shape.function,
        call.shape.return_type.clone(),
        call.retained_args
            .iter()
            .map(|&i| call.shape.args[i].clone())
            .collect(),
    );
    (func_meta.validator_ptr)(&retained_shape)?;
    Ok(PreparedCall {
        function: call.shape.function,
        control: selected.control,
        func_meta,
        field_type: call.shape.return_type.clone(),
        metadata,
        retained_args: call.retained_args.clone(),
    })
}

pub(crate) trait FromCallMetadata: protobuf::Message + Default {
    fn from_local(metadata: &CallMetadata) -> Result<Self>;
}

impl FromCallMetadata for tipb::InUnionMetadata {
    fn from_local(metadata: &CallMetadata) -> Result<Self> {
        let mut value = Self::default();
        if let CallMetadata::InUnion { in_union } = metadata {
            value.set_in_union(*in_union);
        }
        Ok(value)
    }
}

pub(crate) fn extract_call_metadata<T: FromCallMetadata>(call: &CallBuild) -> Result<T> {
    match &call.metadata {
        MetadataSource::Wire(value) => extract_metadata_from_val(value),
        MetadataSource::Local(value) => T::from_local(value),
    }
}

/// Extra information about an RPN function call.
pub struct RpnFnCallExtra<'a> {
    /// The field type of the return value.
    pub ret_field_type: &'a FieldType,
}

/// A single argument of an RPN function.
pub trait RpnFnArg: std::fmt::Debug {
    type Type;

    /// Gets the value in the given row.
    fn get(&self, row: usize) -> Self::Type;

    /// Gets the bit vector of the arg.
    /// Returns `None` if scalar value, and bool indicates whether
    /// all is null or isn't null, otherwise a BitVec.
    /// Returns `Some` if vector value, and bool indicates whether
    /// stored bitmap vector has the same layout as elements,
    /// aka. logical_rows is identical or not. If logical_rows is
    /// identical, the second tuple element yields true.
    fn get_bit_vec(&self) -> (Option<&BitVec>, bool);
}

/// Represents an RPN function argument of a `ScalarValue`.
#[derive(Clone, Copy, Debug)]
pub struct ScalarArg<'a, T: EvaluableRef<'a>>(Option<T>, PhantomData<&'a T>);

impl<'a, T: EvaluableRef<'a>> ScalarArg<'a, T> {
    pub fn new(data: Option<T>) -> Self {
        Self(data, PhantomData)
    }
}

impl<'a, T: EvaluableRef<'a>> RpnFnArg for ScalarArg<'a, T> {
    type Type = Option<T>;

    /// Gets the value in the given row. All rows of a `ScalarArg` share the
    /// same value.
    #[inline]
    fn get(&self, _row: usize) -> Option<T> {
        self.0.clone()
    }

    // All items of scalar arg is either not null or null
    #[inline]
    fn get_bit_vec(&self) -> (Option<&BitVec>, bool) {
        (None, self.0.is_some())
    }
}

/// Represents an RPN function argument of a `VectorValue`.
#[derive(Clone, Copy, Debug)]
pub struct VectorArg<'a, T: 'a + EvaluableRef<'a>, C: 'a + ChunkRef<'a, T>> {
    physical_col: C,
    logical_rows: LogicalRows<'a>,
    _phantom: PhantomData<T>,
}

impl<'a, T: EvaluableRef<'a>, C: 'a + ChunkRef<'a, T>> RpnFnArg for VectorArg<'a, T, C> {
    type Type = Option<T>;

    #[inline]
    fn get(&self, row: usize) -> Option<T> {
        let logical_index = self.logical_rows.get_idx(row);
        self.physical_col.get_option_ref(logical_index)
    }

    #[inline]
    fn get_bit_vec(&self) -> (Option<&BitVec>, bool) {
        (
            Some(self.physical_col.get_bit_vec()),
            self.logical_rows.is_ident(),
        )
    }
}

/// Partial or complete argument definition of an RPN function.
///
/// `ArgDef` is constructed at the beginning of evaluating an RPN function. The
/// types of `RpnFnArg`s are determined at this stage. So there won't be dynamic
/// dispatch or enum matches when the function is applied to each row of the
/// input.
pub trait ArgDef: std::fmt::Debug {}

/// RPN function argument definitions in the form of a linked list.
///
/// For example, if an RPN function foo(Int, Real, Decimal) is applied to input
/// of a scalar of integer, a vector of reals and a vector of decimals, the
/// constructed `ArgDef` will be `Arg<ScalarArg<Int>, Arg<VectorValue<Real>,
/// Arg<VectorValue<Decimal>, Null>>>`. `Null` indicates the end of the argument
/// list.
#[derive(Debug)]
pub struct Arg<A: RpnFnArg, Rem: ArgDef> {
    arg: A,
    rem: Rem,
}

impl<A: RpnFnArg, Rem: ArgDef> ArgDef for Arg<A, Rem> {}

impl<A: RpnFnArg, Rem: ArgDef> Arg<A, Rem> {
    /// Gets the value of the head argument in the given row and returns the
    /// remaining argument list.
    #[inline]
    pub fn extract(&self, row: usize) -> (A::Type, &Rem) {
        (self.arg.get(row), &self.rem)
    }

    /// Gets the bit vector of each arg
    #[inline]
    pub fn get_bit_vec(&self) -> ((Option<&BitVec>, bool), &Rem) {
        (self.arg.get_bit_vec(), &self.rem)
    }
}

/// Represents the end of the argument list.
#[derive(Debug)]
pub struct Null;

impl ArgDef for Null {}

/// A generic evaluator of an RPN function.
///
/// For every RPN function, the evaluator should be created first. Then, call
/// its `eval` method with the input to get the result vector.
///
/// There are two kinds of evaluators in general:
/// - `ArgConstructor`: It's a provided `Evaluator`. It is used in the `rpn_fn`
///   attribute macro to generate the `ArgDef`. The `def` parameter of its eval
///   method is the already constructed `ArgDef`. If it is the outmost
///   evaluator, `def` should be `Null`.
/// - Custom evaluators which do the actual execution of the RPN function. The
///   `def` parameter of its eval method is the constructed `ArgDef`.
///   Implementors can then extract values from the arguments, execute the RPN
///   function and fill the result vector.
pub trait Evaluator<'a> {
    fn eval(
        self,
        def: impl ArgDef,
        ctx: &mut EvalContext,
        output_rows: usize,
        args: &'a [RpnStackNode<'a>],
        extra: &mut RpnFnCallExtra<'_>,
        metadata: &(dyn Any + Send),
    ) -> Result<VectorValue>;
}

pub struct ArgConstructor<'a, A: EvaluableRef<'a>, E: Evaluator<'a>> {
    arg_index: usize,
    inner: E,
    _phantom: PhantomData<&'a A>,
}

impl<'a, A: EvaluableRef<'a>, E: Evaluator<'a>> ArgConstructor<'a, A, E> {
    #[inline]
    pub fn new(arg_index: usize, inner: E) -> Self {
        ArgConstructor {
            arg_index,
            inner,
            _phantom: PhantomData,
        }
    }
}

impl<'a, A: EvaluableRef<'a>, E: Evaluator<'a>> Evaluator<'a> for ArgConstructor<'a, A, E> {
    fn eval(
        self,
        def: impl ArgDef,
        ctx: &mut EvalContext,
        output_rows: usize,
        args: &'a [RpnStackNode<'a>],
        extra: &mut RpnFnCallExtra<'_>,
        metadata: &(dyn Any + Send),
    ) -> Result<VectorValue> {
        match &args[self.arg_index] {
            RpnStackNode::Scalar { value, .. } => {
                let v = A::borrow_scalar_value_ref(value.as_scalar_value_ref());
                let new_def = Arg {
                    arg: ScalarArg::new(v),
                    rem: def,
                };
                self.inner
                    .eval(new_def, ctx, output_rows, args, extra, metadata)
            }
            RpnStackNode::Vector { value, .. } => {
                let logical_rows = value.logical_rows_struct();

                let v = A::borrow_vector_value(value.as_ref());

                let new_def = Arg {
                    arg: VectorArg {
                        physical_col: v,
                        logical_rows,
                        _phantom: PhantomData,
                    },
                    rem: def,
                };
                self.inner
                    .eval(new_def, ctx, output_rows, args, extra, metadata)
            }
        }
    }
}

/// Validates whether the return type of an expression node meets expectation.
pub fn validate_expr_return_type(expr: &Expr, et: EvalType) -> Result<()> {
    validate_field_type(expr.get_field_type(), et)
}

pub(crate) fn validate_field_type(field_type: &FieldType, et: EvalType) -> Result<()> {
    let received_et = box_try!(EvalType::try_from(field_type.as_accessor().tp()));
    if et == received_et {
        Ok(())
    } else {
        match (et, received_et) {
            (EvalType::Int, EvalType::Enum) | (EvalType::Bytes, EvalType::Enum) => Ok(()),
            _ => Err(other_err!("Expect `{}`, received `{}`", et, received_et)),
        }
    }
}

/// Validates whether the number of arguments of an expression node meets
/// expectation.
pub fn validate_expr_arguments_eq(expr: &Expr, args: usize) -> Result<()> {
    validate_argument_count_eq(expr.get_children().len(), args)
}

pub(crate) fn validate_argument_count_eq(received_args: usize, args: usize) -> Result<()> {
    if received_args == args {
        Ok(())
    } else {
        Err(other_err!(
            "Expect {} arguments, received {}",
            args,
            received_args
        ))
    }
}

/// Validates whether the number of arguments of an expression node >=
/// expectation.
pub fn validate_expr_arguments_gte(expr: &Expr, args: usize) -> Result<()> {
    validate_argument_count_gte(expr.get_children().len(), args)
}

pub(crate) fn validate_argument_count_gte(received_args: usize, args: usize) -> Result<()> {
    if received_args >= args {
        Ok(())
    } else {
        Err(other_err!(
            "Expect at least {} arguments, received {}",
            args,
            received_args
        ))
    }
}

/// Validates whether the number of arguments of an expression node <=
/// expectation.
pub fn validate_expr_arguments_lte(expr: &Expr, args: usize) -> Result<()> {
    validate_argument_count_lte(expr.get_children().len(), args)
}

pub(crate) fn validate_argument_count_lte(received_args: usize, args: usize) -> Result<()> {
    if received_args <= args {
        Ok(())
    } else {
        Err(other_err!(
            "Expect at most {} arguments, received {}",
            args,
            received_args
        ))
    }
}

// `VARG_PARAM_BUF` is a thread-local cache for evaluating vargs
// `rpn_fn`. In this way, we can reduce overhead of allocating new Vec.
// According to https://doc.rust-lang.org/std/mem/fn.size_of.html ,
// &T and Option<&T> has the same size.
assert_eq_size!(usize, Option<&Int>);
assert_eq_size!(usize, Option<&Real>);
assert_eq_size!(usize, Option<&Decimal>);
assert_eq_size!(usize, Option<&Bytes>);
assert_eq_size!(usize, Option<&DateTime>);
assert_eq_size!(usize, Option<&Duration>);
assert_eq_size!(usize, Option<&Json>);

thread_local! {
    pub static VARG_PARAM_BUF: std::cell::RefCell<Vec<usize>> =
        std::cell::RefCell::new(Vec::with_capacity(20));

    pub static VARG_PARAM_BUF_BYTES_REF: std::cell::RefCell<Vec<Option<BytesRef<'static>>>> =
        std::cell::RefCell::new(Vec::with_capacity(20));

    pub static VARG_PARAM_BUF_JSON_REF: std::cell::RefCell<Vec<Option<JsonRef<'static>>>> =
        std::cell::RefCell::new(Vec::with_capacity(20));

    pub static VARG_PARAM_BUF_VECTOR_FLOAT32_REF: std::cell::RefCell<Vec<Option<VectorFloat32Ref<'static>>>> =
        std::cell::RefCell::new(Vec::with_capacity(20));

    pub static RAW_VARG_PARAM_BUF: std::cell::RefCell<Vec<ScalarValueRef<'static>>> =
        std::cell::RefCell::new(Vec::with_capacity(20));
}

pub fn extract_metadata_from_val<T: protobuf::Message + Default>(val: &[u8]) -> Result<T> {
    if val.is_empty() {
        Ok(T::default())
    } else {
        protobuf::parse_from_bytes::<T>(val)
            .map_err(|e| other_err!("Decode metadata failed: {}", e))
    }
}
