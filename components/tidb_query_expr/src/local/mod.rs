// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Checked in-process construction over the official RPN evaluator.
//!
//! Legacy admission is exact signed LongLong, the seed arithmetic/NULLIF
//! kernels, explicit strict integer controls, and catalog-bound host calls.
//! Bound values and host arguments are imported/evaluated on demand by the
//! official driver. Existing ordinary calls remain eager postfix. The separate
//! `compile_local_profiled` entry admits only exact signed PlusInt203 under
//! explicit TypedRow/PbRow assertions and identity-conversion left NULL-stop.
//! It does not admit AST/argument-major batch profiles or supply native source-
//! shaped diagnostics. Other domains remain gated. Callers must not pre-convert
//! unvisited branches or hide native expression evaluation in services.
//! The optional reported binding entry owns the unchanged error and an exact
//! failing input/ordinary-kernel site; it is not native diagnostic adaptation.
//! `compile_control_with_lineage` separately admits SQL TypedRow Int/Bytes
//! selection controls and signed Int booleans with required result metadata
//! IDs. It does not compose ordinary arithmetic or hosts, infer native/PB
//! origin, or widen the old entrypoints. Its caller owns the matching
//! materialization table and validates fixed native kind/collation contracts at
//! each demanded read.
//! `compile_numeric_batch` is another closed entry: signed LongLong PlusInt203
//! under a whole-source SQL numeric-batch assertion, with complete left then
//! complete right demand before per-lane parent kernels. Its opaque binding
//! facade does not compose controls, hosts or lineage, expose a row fallback,
//! or authenticate native source/consumer origin.
//! `prepare_evaluated_bytes` separately prepares an opaque reusable worker for
//! a closed operation on ready nullable Int/Bytes: ASCII, LENGTH/OCTET_LENGTH,
//! BIT_LENGTH, LTRIM, RTRIM, UNHEX, CRC32, REVERSE, CHAR_LENGTH, QUOTE, HEX,
//! BIN, LEFT, RIGHT, REPLACE, BIT_COUNT, bitwise NOT/AND/OR/XOR, shifts and
//! normalized truth/presence predicates, logical AND/OR/XOR, MD5, SHA/SHA1,
//! INET_ATON, INET_NTOA, INET6_ATON and INET6_NTOA. Address argument coercion
//! and binary/text/UInt packing stay in the frontend; INET_NTOA consumes Int
//! bits. Hashes consume ready raw Bytes; hash-input conversion and text packing
//! remain frontend responsibilities. Numeric/byte/UTF8 variants are explicit.
//! `EvaluatedArgs` owns fixed shapes, including Int2; Int preserves
//! all 64 bits. Boolean operations instead consume frontend-normalized Int
//! None/0/1, not Real/Decimal/string values. Logical binaries use the real
//! eager official wrapper on ready Int2; native code preserves child demand.
//! Only after checking an explicit undemanded-RHS marker and AND(false, _) or
//! OR(true, _) may that frontend supply an irrelevant representative instead
//! of evaluating RHS. A representative is not a claim that RHS was NULL.
//! Normal wire/control execution stays lazy. Recipes admit only ordered
//! canonical ColumnRefs and their exact prepared calls: one call, or the fixed
//! NOT(IS NULL/TRUE/FALSE) pair for IsNotNull/IsNotTrue/IsNotFalse. These pairs
//! require compile depth 3 and perform two real wrapper dispatches, including
//! for NULL; they are not arbitrary programs. QUOTE(NULL) returns owned
//! non-NULL bytes "NULL". All ready owners remain charged through result
//! extraction. The six private AsinRaw/AcosRaw/SqrtRaw/SignRaw/RadiansRaw/
//! DegreesRaw recipes instead require explicit nullable Ieee754Bits. Their
//! internal little-endian Byte8 transport never admits ordinary Bytes or Int.
//! They share one math helper per operation with the unchanged official Real
//! wrappers, without broadening Real or registering a wire signature. ASIN/
//! ACOS preserve operation NaNs, SQRT nulls negative inputs, SIGN maps NaN to
//! zero, and radians/degrees apply no finite-result policy. Float results own
//! IEEE bits; SIGN owns Int. Frontends keep their NULL/NaN/overflow policies.
//! PiRaw instead requires NoArgs: zero columns and one real zero-argument call
//! to the original PI body, returning owned IEEE bits without a dummy operand.
//! The four IsIpv*Nullable private predicates consume ordinary Bytes and
//! propagate NULL; their non-NULL calls reuse the official predicates, whose
//! wire NULL-to-zero behavior remains unchanged. IPv4 lexical normalization
//! and native argument coercion remain frontend responsibilities. A closed
//! private identity therefore never implies an IEEE input role.
//! SpaceNative/RepeatNative/ToBase64Native/FromBase64Native instead use typed
//! PacketInt/PacketBytes/PacketBytesInt carriers with OutputDisposition. The
//! frontend keeps warning 1301 and original values; a real wrapper dispatch
//! applies SuppressByPacket without running the potentially huge algorithm.
//! The internal non-NULL Int flag is not an original argument or SQL NULL.
//! ReadyIntArg::Undemanded is allowed for RepeatNative only with NULL bytes
//! and Allow: its validated irrelevant zero representative does not claim
//! that RHS was coerced or NULL. FromBase64ValueNative uses ordinary
//! Bytes without packet policy or its signed raw-length guard; execution
//! context availability never selects packet semantics. Wire policies and the
//! shared string/base64 algorithms remain in their original kernel module.
//! Sha2Native requires BytesIntReady with its own role. ReadyIntArg::Undemanded
//! is accepted there only for NULL bytes, before using an irrelevant zero;
//! Value(None) alone denotes a genuinely NULL integer argument. Its digest
//! core is shared with wire SHA2, but invalid lengths are silently NULL here
//! while wire retains warning 1583. Lower/Upper dispatch the binary no-op
//! kernels; LowerUtf8Ready/UpperUtf8Ready bind the original EncodingUtf8Mb4
//! kernels without adding charset owners to the zero-heap transport ABI.
//! LowerAsciiNative/UpperAsciiNative instead preserve the legacy ASCII-only
//! contract through private byte-case wrappers: only ASCII letters change,
//! bytes >= 128 remain exact, and NULL propagates. They use ordinary Bytes and
//! neither impersonate wire no-ops nor decode/repair Unicode. Frontends retain
//! signature choice and one-U+FFFD-per-invalid-byte repair where required.
//! OrdNative folds a frontend-prepared encoded first-character slice, not a
//! whole string. None or at most four bytes is its checked input domain, not a
//! resource limit; oversize inputs are refused, never truncated. Wire ORD keeps
//! its original decoder selection and NULL-to-zero policy; OrdNative propagates
//! NULL and shares only the base-256 fold.
//! TrimBothNative/TrimLeadingNative/TrimTrailingNative consume Bytes2,
//! including syntax-default space patterns, and share the trim core with wire.
//! Native BOTH trims right only after left; wire keeps its independent end
//! bounds. SubstringIndexSignedNative/SubstringIndexUnsignedNative use
//! BytesBytesIntReady and a distinct role. Their count can be Undemanded only
//! for a NULL string operand or an empty delimiter; the caller must preserve a
//! genuinely NULL count as Value(None) before applying the empty-delimiter
//! rule. Native suffixes use forward non-overlapping matches; wire keeps its
//! original reverse search and signed-abs behavior. Unsigned counts retain
//! their raw bits. The four Lpad/Rpad Bytes/Utf8Native recipes admit four ready
//! columns and five nodes, via PacketBytesIntBytes and PadPacket.
//! ReadyBytesArg pairs must both be Value, or both Undemanded for NULL/invalid
//! length or packet suppression. Even zero length or truncation requires both
//! evaluated strings when Allow applies. Validated undemanded strings use
//! empty, non-NULL representatives; these do not claim original string values.
//! Frontends keep length-cast/packet/range/coercion order and select binary for
//! source OR pad. PAD shares its original quotient/remainder writer, with
//! separate policies: native equality truncates and empty-pad growth returns
//! empty; wire keeps its old strict-less truncation, NULL growth and UTF8
//! four-byte bound. Its existing nonzero equal-length/empty-pad
//! division-by-zero bug is not fixed or replaced by an artificial panic here.
//! LnNative/Log2Native use single IEEE754 inputs; LogNative/PowNative require
//! the distinct Ieee754Bits2 carrier. LOG takes base then value; POW takes base
//! then exponent. ReadyIeee754Arg::Undemanded is permitted on either side only
//! for PowNative with Value(None) opposite, never on both sides. Its validated
//! +0-bit representative is not an evaluated operand or SQL NULL. Non-NULL
//! inputs, including domain errors, run the shared std primitive and return
//! actual NaN/Inf bits; frontend warnings/domain masks and finite-result policy
//! remain outside. Wire math retains its original domain/finite/error envelope.
//! UncompressedLengthNative shares the empty/short/little-endian-u32 core:
//! empty and lengths 1..4 yield zero, NULL stays NULL, all 32 header bits are
//! retained. The private path is quiet; wire retains short-input warning 1259.
//! Insert and InsertUtf8Native also admit four columns/five nodes, using
//! ordinary BytesIntIntBytes with real position and length Int values. Along
//! with the four PAD recipes and Locate3Native, these are the entire
//! four-column whitelist. The binary Insert uses the existing wire signature;
//! native UTF8 maps true
//! character boundaries and accepts raw replacement bytes. All share one byte
//! splice; wire UTF8 keeps strict decoding of both strings and its old
//! character-index as byte-offset bug. INSERT packet checks belong after a
//! successful kernel result, against actual result bytes, not a pre-dispatch
//! flag. No general graph or driver limit is widened. Results are owned values,
//! not native SQL descriptors. Frontends retain demand/coercion order,
//! normalization, signature selection and result packing (including
//! CRC32/bitwise UInt). The old single-Bytes eval_one and ASCII-only facade use
//! the same compiler/worker/driver. No native child, binding-service or Host
//! callback, arbitrary signature, original SQL/PB schema, or raw-program escape
//! is exposed. The sealed context and actual wrapper-dispatch witness persist
//! per worker, not per row; diagnostics and execution-owner lifetime stay
//! outside. Wrapper counts are not body counts.

mod batch;
mod compile;
#[cfg(test)]
mod control_tests;
mod diagnostic;
#[cfg(test)]
mod diagnostic_tests;
pub(crate) mod host;
#[cfg(test)]
mod host_tests;
mod lineage;
#[cfg(test)]
mod lineage_tests;
mod profile;
#[cfg(test)]
mod profile_tests;
pub(crate) mod registry;
pub(crate) mod runtime;
mod spec;
#[cfg(test)]
mod tests;

pub use tidb_query_datatype::codec::collation::native::NativeCollation;

pub use self::{
    batch::{
        ComputedBytes, ComputedBytesMetadata, ComputedDecimal, ComputedDecimalDivision,
        ComputedDecimalDivisionMetadata, ComputedDecimalFast, ComputedDecimalFastMetadata,
        ComputedDecimalMetadata, ComputedIeee754Bits, ComputedIeee754BitsMetadata, ComputedInt,
        ComputedInt128, ComputedInt128Metadata, ComputedIntMetadata, ComputedJsonReport,
        ComputedJsonReportMetadata, ComputedNativeVector, ComputedNativeVectorMetadata,
        ComputedUncompress, ComputedUncompressMetadata, ComputedValue, EvaluatedArgs,
        EvaluatedAsciiWorker, EvaluatedBytesOp, EvaluatedBytesWorker, EvaluatedSqlFailureKind,
        JsonReportOutcome, LocalBatch, LocalEvalState, NativeSearchPolicy, OutputDisposition,
        ReadyBytesArg, ReadyConvBaseArg, ReadyDecimalArg, ReadyIeee754Arg, ReadyIntArg,
        ReadySubstringI128, ReportedEvaluatedFailure, UncompressOutcome, WorkerStorage,
        native_decimal_bridge_error, prepare_evaluated_ascii, prepare_evaluated_bytes,
        prepare_grouping_args, prepare_json_array_args, prepare_json_binary_pair_args,
        prepare_json_object_args, prepare_json_path_values_args, prepare_json_paths_args,
        prepare_json_raw_identity_args, prepare_json_raw_paths_values_args,
        prepare_json_serde_args,
    },
    compile::{
        LocalNumericBatchProgram, LocalProgram, compile_control_with_lineage, compile_local,
        compile_local_profiled, compile_local_with_hosts, compile_numeric_batch,
    },
    diagnostic::{LocalFailureSite, LocalFailureStage, ReportedLocalFailure},
    host::{
        ArgMode, HostArgReply, HostArgRequest, HostCatalog, HostCatalogKey, HostInvocation,
        HostSignature, HostSlot, HostStart, HostStep, HostTaskId, LocalHostServices,
        PreparedHostCall,
    },
    lineage::{
        ControlLineageFacts, ControlProducerFact, ControlProducerRole, LineageCarrier,
        LineagedBatch, LocalControlProgram, ResultMetaId,
    },
    profile::{
        NumericBatchFacts, OrdinaryCallSite, OrdinaryProfile, OrdinaryProfileSpec, OrdinarySourceId,
    },
    runtime::{ExecutionLimits, InputRow, LocalRuntimeServices},
    spec::{CompileLimits, LocalCompileContext, LocalError, LocalExpr, LocalResult},
};
pub(crate) use self::{
    batch::{
        EvaluatedArgsRole, EvaluatedKernelKind, NativeDecimalDivisionCallMetadata,
        NativeDecimalDivisionKind, NativeLikeCallMetadata, NativeRegexpCallMetadata,
        NativeRegexpKind,
    },
    diagnostic::FailureRecorder,
    lineage::CheckedResultFlow,
};
pub use crate::{
    CallMetadata,
    FunctionRef,
    LiteralKind,
    LocalFunctionId,
    impl_arithmetic::NativeDecimalDivisionDisposition,
    // Narrow compatibility exports for native tests; production compression
    // enters the closed workers and consumes their owned computed outcomes.
    impl_encryption::{InflateError, frame_compressed, inflate, native_go_flate::go_zlib_deflate},
    impl_math::{
        conv_valid_prefix_native,
        native_decimal_target_scale,
        // Compatibility symbols for existing native test oracles only;
        // production evaluated trig uses the closed workers, not these exports.
        native_go_trig::{go_atan, go_atan2, go_cos, go_sin, go_tan, trig_reduce},
    },
    impl_string::{
        ConcatKind, ConcatTerminal, FieldIntValue, FieldTerminal, PreparedCharArgs,
        PreparedConcatArgs, PreparedExportSetArgs, PreparedFieldArgs, PreparedFindInSetKeys,
        PreparedMakeSetArgs, ReadyFieldIntArg, elt_selected_arg, field_bytes_equal,
        field_int_equal, field_real_equal, legacy_substring_needs_len, make_set_selected,
        prepare_char_args, prepare_concat_args, prepare_export_set_args, prepare_field_bytes_args,
        prepare_field_int_args, prepare_field_real_args, prepare_find_in_set_keys,
        prepare_make_set_args,
    },
    types::function::PreparedOrdinaryCall,
};
