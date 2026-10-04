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
        prepare_json_nullable_values_args, prepare_json_object_args, prepare_json_path_values_args,
        prepare_json_paths_args, prepare_json_raw_identity_args,
        prepare_json_raw_paths_values_args, prepare_json_raw_values_args, prepare_json_search_args,
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
        NativeRegexpKind, NativeTemporalCallMetadata,
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

#[cfg(test)]
mod temporal_literal_tests {
    use tidb_query_datatype::codec::mysql::{
        Time, TimeType,
        time::{NativeSessionTimeZone, NativeTemporalValue},
    };

    use super::*;
    use crate::{NativeTemporalLiteralResult, decode_native_temporal_literal_result};

    fn prepare(operation: EvaluatedBytesOp, limits: ExecutionLimits) -> EvaluatedBytesWorker {
        prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            limits,
            usize::MAX,
        )
        .unwrap()
    }

    fn args(value: &str, modes: i64, zone: NativeSessionTimeZone) -> EvaluatedArgs {
        EvaluatedArgs::TemporalText {
            value: value.as_bytes().to_vec(),
            modes,
            zone,
        }
    }

    fn computed_bytes(value: ComputedValue) -> Vec<u8> {
        let ComputedValue::Bytes(value) = value else {
            panic!("literal must own Bytes");
        };
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        value
            .into_option()
            .expect("a literal report is never SQL NULL")
    }

    #[test]
    fn temporal_literals_admit_only_actual_text_modes_and_business_calendar_failures() {
        let payload = NativeTemporalCallMetadata::new();
        assert!(payload.is_unbound());
        assert!(matches!(payload.zone(), Err(LocalError::InvalidSpec(_))));
        for (operation, invalid, valid, kind, hour, minute, second, message) in [
            (
                EvaluatedBytesOp::DateLiteralNative,
                "2020-02-30",
                "2020-01-01",
                TimeType::Date,
                0,
                0,
                0,
                // Original date_literal parse-error branch uses its unpadded
                // datetime diagnostic, not the regex gate's date diagnostic.
                "Incorrect datetime value: '2020-2-30'",
            ),
            (
                EvaluatedBytesOp::TimestampLiteralNative,
                "2020-02-30 12:00:00",
                "2020-01-01 12:34:56",
                TimeType::DateTime,
                12,
                34,
                56,
                "Incorrect datetime value: '2020-02-30 12:00:00'",
            ),
        ] {
            let mut worker = prepare(operation, ExecutionLimits::default());
            let storage = worker.retained_storage().unwrap();
            for refused in [
                EvaluatedArgs::BytesInt(Some(valid.as_bytes().to_vec()), Some(0)),
                EvaluatedArgs::BytesInt(None, Some(0)),
                EvaluatedArgs::TemporalText {
                    value: vec![255],
                    modes: 0,
                    zone: NativeSessionTimeZone::utc(),
                },
                args(valid, -1, NativeSessionTimeZone::utc()),
                args(valid, 8, NativeSessionTimeZone::utc()),
            ] {
                assert!(matches!(
                    worker.eval_args(refused),
                    Err(LocalError::InvalidBatch(_))
                ));
                assert_eq!(worker.kernel_invocations(), 0);
                assert!(worker.is_healthy());
                assert_eq!(worker.retained_storage().unwrap(), storage);
            }
            // An invalid calendar is admitted, dispatched and returned as the
            // actual hard-error report, not preflight refusal or a fake NULL.
            let output = computed_bytes(
                worker
                    .eval_args(args(invalid, 0, NativeSessionTimeZone::utc()))
                    .unwrap(),
            );
            assert!(matches!(decode_native_temporal_literal_result(&output),
                Some(NativeTemporalLiteralResult::WrongValue { code: 1292, message: actual }) if actual == message));
            assert_eq!(worker.kernel_invocations(), 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            for modes in 0..=7 {
                let output = computed_bytes(
                    worker
                        .eval_args(args(valid, modes, NativeSessionTimeZone::utc()))
                        .unwrap(),
                );
                let Some(NativeTemporalLiteralResult::Value(value)) =
                    decode_native_temporal_literal_result(&output)
                else {
                    panic!("all three actual mode bits admit a valid calendar");
                };
                assert_eq!(
                    value,
                    NativeTemporalValue {
                        raw: Time::native_core_from_fields(2020, 1, 1, hour, minute, second, 0),
                        kind,
                        fsp: 0,
                    }
                );
                assert_eq!(worker.kernel_invocations(), modes as u64 + 2);
                assert!(worker.is_healthy());
                assert_eq!(worker.retained_storage().unwrap(), storage);
            }
        }
    }

    #[test]
    fn temporal_literals_rebind_actual_zones_and_release_name_owners_on_every_exit() {
        let named = |name: &str| NativeSessionTimeZone::Named(name.parse().unwrap());
        let fixed = || NativeSessionTimeZone::Fixed {
            // Deliberately an IANA-looking name that disagrees with the actual
            // offset, including seconds that SQL offset-text reparsing loses.
            name: "America/Los_Angeles".to_owned(),
            offset_secs: 20_715,
        };
        let mut worker = prepare(
            EvaluatedBytesOp::TimestampLiteralNative,
            ExecutionLimits::default(),
        );
        let storage = worker.retained_storage().unwrap();
        let summer = "2020-07-01 00:00:00+00:00";
        let mut calls = 0;
        for (text, zone, expected) in [
            (
                "2011-03-13 01:59:59.9999999",
                named("America/Los_Angeles"),
                Some((2011, 3, 13, 3, 0, 0, 6)),
            ),
            (
                summer,
                named("Europe/London"),
                Some((2020, 7, 1, 1, 0, 0, 0)),
            ),
            ("2020-01-01", fixed(), None),
            (summer, fixed(), Some((2020, 7, 1, 5, 45, 15, 0))),
            (
                summer,
                named("America/Los_Angeles"),
                Some((2020, 6, 30, 17, 0, 0, 0)),
            ),
            (
                summer,
                named("Europe/London"),
                Some((2020, 7, 1, 1, 0, 0, 0)),
            ),
        ] {
            let output = computed_bytes(worker.eval_args(args(text, 0, zone)).unwrap());
            calls += 1;
            match (decode_native_temporal_literal_result(&output), expected) {
                (
                    Some(NativeTemporalLiteralResult::Value(value)),
                    Some((year, month, day, hour, minute, second, fsp)),
                ) => {
                    assert_eq!(
                        value,
                        NativeTemporalValue {
                            raw: Time::native_core_from_fields(
                                year, month, day, hour, minute, second, 0
                            ),
                            kind: TimeType::DateTime,
                            fsp,
                        }
                    );
                }
                (
                    Some(NativeTemporalLiteralResult::WrongValue {
                        code: 1525,
                        message,
                    }),
                    None,
                ) => {
                    assert_eq!(message, "Incorrect datetime value: '2020-01-01'");
                }
                _ => panic!("unexpected literal value/error outcome"),
            }
            assert_eq!(worker.kernel_invocations(), calls);
            // Storage observation checks that the invocation-owned zone is no
            // longer bound, including after the WrongValue business result.
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let mut bounded = prepare(
            EvaluatedBytesOp::TimestampLiteralNative,
            ExecutionLimits {
                max_retained_bytes: 64 * 1024,
                ..ExecutionLimits::default()
            },
        );
        let storage = bounded.retained_storage().unwrap();
        let mut name = String::with_capacity(128 * 1024);
        name.push_str("UTC");
        assert!(name.capacity() > 64 * 1024 && name.len() == 3);
        assert!(matches!(
            bounded.eval_args(args(
                summer,
                0,
                NativeSessionTimeZone::Fixed {
                    name,
                    offset_secs: 0,
                }
            )),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(
            bounded.kernel_invocations(),
            0,
            "charge actual Fixed name capacity before the kernel"
        );
        assert!(bounded.is_healthy());
        assert_eq!(bounded.retained_storage().unwrap(), storage);
        let output = computed_bytes(
            bounded
                .eval_args(args(summer, 0, NativeSessionTimeZone::utc()))
                .unwrap(),
        );
        assert!(matches!(
            decode_native_temporal_literal_result(&output),
            Some(NativeTemporalLiteralResult::Value(_))
        ));
        assert_eq!(bounded.kernel_invocations(), 1);
        assert!(bounded.is_healthy());
        assert_eq!(bounded.retained_storage().unwrap(), storage);
        // This layer has execution steps, not the frontend's pool slots. A
        // zero-step refusal proves cleanup before dispatch without inventing
        // NULL data or claiming a frontend zero-slot lifecycle test.
        let mut stopped = prepare(
            EvaluatedBytesOp::TimestampLiteralNative,
            ExecutionLimits {
                max_steps: 0,
                ..ExecutionLimits::default()
            },
        );
        let storage = stopped.retained_storage().unwrap();
        for zone in [fixed(), named("Europe/London")] {
            assert!(matches!(
                stopped.eval_args(args(summer, 0, zone)),
                Err(LocalError::ResourceLimit(_))
            ));
            assert_eq!(stopped.kernel_invocations(), 0);
            assert!(stopped.is_healthy());
            assert_eq!(stopped.retained_storage().unwrap(), storage);
        }
    }
}
