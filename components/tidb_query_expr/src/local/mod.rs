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

#[cfg(test)]
mod convert_tz_tests {
    use super::*;

    #[test]
    fn convert_tz_bytes3_admission_nullable_dispatch_and_owned_results() {
        let prepare = |limits| {
            prepare_evaluated_bytes(
                EvaluatedBytesOp::ConvertTzNative,
                LocalCompileContext::default(),
                limits,
                usize::MAX,
            )
            .unwrap()
        };
        let ready = |date: &str, from: &str, to: &str| {
            EvaluatedArgs::Bytes3([
                Some(date.as_bytes().to_vec()),
                Some(from.as_bytes().to_vec()),
                Some(to.as_bytes().to_vec()),
            ])
        };
        let mut worker = prepare(ExecutionLimits::default());
        let storage = worker.retained_storage().unwrap();
        assert!(matches!(
            worker.eval_args(EvaluatedArgs::Bytes(None)),
            Err(LocalError::InvalidBatch(_))
        ));
        for malformed_slot in 0..3 {
            // A real NULL elsewhere must not mask malformed present UTF-8 in
            // the physical carrier admission check.
            let mut values = [None, None, None];
            values[malformed_slot] = Some(vec![255]);
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::Bytes3(values)),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let mut calls = 0;
        for null_slot in 0..3 {
            let mut values = [
                Some(b"2004-01-01 12:00:00".to_vec()),
                Some(b"+00:00".to_vec()),
                Some(b"GMT".to_vec()),
            ];
            values[null_slot] = None;
            let ComputedValue::Bytes(output) =
                worker.eval_args(EvaluatedArgs::Bytes3(values)).unwrap()
            else {
                panic!("CONVERT_TZ NULL must remain owned Bytes");
            };
            assert_eq!(output.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(output.into_option(), None);
            calls += 1;
            assert_eq!(worker.kernel_invocations(), calls);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        // Fixed expected rows retained from native time_fn/convert_tz.rs:
        // identity offset, original fraction truncation, and the autumn overlap.
        for (date, from, to, expected) in [
            (
                "2004-01-01 12:00:00",
                "+00:00",
                "GMT",
                Some("2004-01-01 12:00:00"),
            ),
            (
                "2004-01-01 12:00:00.11111111111",
                "-00:00",
                "+12:34",
                Some("2004-01-02 00:34:00.111111"),
            ),
            (
                "2021-10-31 03:00:00",
                "+02:00",
                "Europe/Amsterdam",
                Some("2021-10-31 02:00:00"),
            ),
            ("2004-01-01 12:00:00", "not/a/time_zone", "+00:00", None),
            ("2004-01-01 12:00:00", "+00:00", "not/a/time_zone", None),
            ("2004-02-30 12:00:00", "+00:00", "GMT", None),
        ] {
            let ComputedValue::Bytes(output) = worker.eval_args(ready(date, from, to)).unwrap()
            else {
                panic!("CONVERT_TZ must own nullable UTF-8 Bytes");
            };
            assert_eq!(output.metadata(), ComputedBytesMetadata::OwnBytes);
            assert_eq!(output.value(), expected.map(str::as_bytes));
            assert_eq!(
                output.into_option(),
                expected.map(|value| value.as_bytes().to_vec())
            );
            calls += 1;
            assert_eq!(
                worker.kernel_invocations(),
                calls,
                "unknown zones and bad calendars are business NULLs"
            );
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let mut stopped = prepare(ExecutionLimits {
            max_steps: 0,
            ..ExecutionLimits::default()
        });
        let storage = stopped.retained_storage().unwrap();
        for args in [
            ready("2004-01-01 12:00:00", "+00:00", "GMT"),
            EvaluatedArgs::Bytes3([None, Some(b"+00:00".to_vec()), Some(b"GMT".to_vec())]),
        ] {
            assert!(matches!(
                stopped.eval_args(args),
                Err(LocalError::ResourceLimit(_))
            ));
            assert_eq!(stopped.kernel_invocations(), 0);
            assert!(stopped.is_healthy());
            assert_eq!(stopped.retained_storage().unwrap(), storage);
        }
        // The local step budget is not the frontend pool's zero-slot policy.
    }
}

#[cfg(test)]
mod timestamp_worker_tests {
    use tidb_query_datatype::codec::mysql::time::NativeSessionTimeZone;

    use super::*;
    use crate::{
        NativeIdentityRef, NativeTimestampResult, decode_native_timestamp_result,
        encode_native_identity,
    };

    fn prepare(operation: EvaluatedBytesOp, limits: ExecutionLimits) -> EvaluatedBytesWorker {
        prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            limits,
            usize::MAX,
        )
        .unwrap()
    }

    fn parse_args(value: &str, is_float: bool, zone: NativeSessionTimeZone) -> EvaluatedArgs {
        EvaluatedArgs::TemporalParseText {
            value: value.as_bytes().to_vec(),
            is_float,
            zone,
        }
    }

    fn bytes(value: ComputedValue) -> Option<Vec<u8>> {
        let ComputedValue::Bytes(value) = value else {
            panic!("TIMESTAMP must own Bytes");
        };
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        value.into_option()
    }

    #[test]
    fn timestamp_heads_preserve_source_kind_zone_rebinding_and_resource_cleanup() {
        for operation in [
            EvaluatedBytesOp::Timestamp1Native,
            EvaluatedBytesOp::Timestamp2BaseNative,
        ] {
            let mut worker = prepare(operation, ExecutionLimits::default());
            let storage = worker.retained_storage().unwrap();
            for invalid in [
                EvaluatedArgs::BytesInt(Some(b"2020-01-01".to_vec()), Some(0)),
                EvaluatedArgs::BytesInt(None, Some(1)),
                EvaluatedArgs::TemporalText {
                    value: b"2020-01-01".to_vec(),
                    modes: 0,
                    zone: NativeSessionTimeZone::utc(),
                },
                EvaluatedArgs::TemporalParseText {
                    value: vec![255],
                    is_float: false,
                    zone: NativeSessionTimeZone::utc(),
                },
            ] {
                assert!(matches!(
                    worker.eval_args(invalid),
                    Err(LocalError::InvalidBatch(_))
                ));
                assert_eq!(worker.kernel_invocations(), 0);
                assert!(worker.is_healthy());
                assert_eq!(worker.retained_storage().unwrap(), storage);
            }
            // Same source text, distinct actual SQL source kinds. The numeric
            // result is pinned by the native timestamp(0.123) source test.
            let output = bytes(
                worker
                    .eval_args(parse_args("0.123", true, NativeSessionTimeZone::utc()))
                    .unwrap(),
            )
            .expect("head reports are non-NULL");
            match decode_native_timestamp_result(&output) {
                Some(NativeTimestampResult::Value(value))
                    if operation == EvaluatedBytesOp::Timestamp1Native =>
                {
                    assert_eq!(value, "0000-00-00 00:00:00.123");
                }
                Some(NativeTimestampResult::Base(value))
                    if operation == EvaluatedBytesOp::Timestamp2BaseNative =>
                {
                    assert_eq!(output.len(), 11);
                    assert_eq!(output[0], 15);
                    assert_eq!(value.fsp, 3);
                }
                _ => panic!("unexpected numeric-source TIMESTAMP head report"),
            }
            let output = bytes(
                worker
                    .eval_args(parse_args("0.123", false, NativeSessionTimeZone::utc()))
                    .unwrap(),
            )
            .expect("warnings are non-NULL reports");
            assert!(
                matches!(decode_native_timestamp_result(&output), Some(NativeTimestampResult::Warning { code: 1292, message })
                if message == "Incorrect datetime value: '0.123'")
            );
            assert_eq!(worker.kernel_invocations(), 2);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        let named = |name: &str| NativeSessionTimeZone::Named(name.parse().unwrap());
        let mut worker = prepare(
            EvaluatedBytesOp::Timestamp1Native,
            ExecutionLimits::default(),
        );
        let storage = worker.retained_storage().unwrap();
        let summer = "2020-07-01 00:00:00+00:00";
        for (index, (text, zone, expected)) in [
            (summer, named("Europe/London"), Some("2020-07-01 01:00:00")),
            ("bad", named("America/Los_Angeles"), None),
            (
                summer,
                NativeSessionTimeZone::Fixed {
                    name: "America/Los_Angeles".to_owned(),
                    offset_secs: 20_715,
                },
                Some("2020-07-01 05:45:15"),
            ),
            (
                summer,
                named("America/Los_Angeles"),
                Some("2020-06-30 17:00:00"),
            ),
            (summer, named("Europe/London"), Some("2020-07-01 01:00:00")),
        ]
        .into_iter()
        .enumerate()
        {
            let output = bytes(worker.eval_args(parse_args(text, false, zone)).unwrap())
                .expect("head reports are non-NULL");
            match (decode_native_timestamp_result(&output), expected) {
                (Some(NativeTimestampResult::Value(value)), Some(expected)) => {
                    assert_eq!(value, expected)
                }
                (
                    Some(NativeTimestampResult::Warning {
                        code: 1292,
                        message,
                    }),
                    None,
                ) => assert_eq!(message, "Incorrect datetime value: 'bad'"),
                _ => panic!("unexpected zone-bound TIMESTAMP result"),
            }
            assert_eq!(worker.kernel_invocations(), index as u64 + 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
        for operation in [
            EvaluatedBytesOp::Timestamp1Native,
            EvaluatedBytesOp::Timestamp2BaseNative,
        ] {
            let mut bounded = prepare(
                operation,
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
                bounded.eval_args(parse_args(
                    summer,
                    false,
                    NativeSessionTimeZone::Fixed {
                        name,
                        offset_secs: 0
                    }
                )),
                Err(LocalError::ResourceLimit(_))
            ));
            assert_eq!(bounded.kernel_invocations(), 0);
            assert!(bounded.is_healthy());
            assert_eq!(bounded.retained_storage().unwrap(), storage);
            let output = bytes(
                bounded
                    .eval_args(parse_args(summer, false, named("Europe/London")))
                    .unwrap(),
            )
            .expect("head reports are non-NULL");
            assert!(matches!(
                decode_native_timestamp_result(&output),
                Some(NativeTimestampResult::Value(_)) | Some(NativeTimestampResult::Base(_))
            ));
            assert_eq!(bounded.kernel_invocations(), 1);
            assert!(bounded.is_healthy());
            assert_eq!(bounded.retained_storage().unwrap(), storage);
        }
    }

    #[test]
    fn timestamp_actual_head_frames_feed_add_and_null_profiles_without_fabricated_bases() {
        let mut null = prepare(
            EvaluatedBytesOp::TimestampNullNative,
            ExecutionLimits::default(),
        );
        let storage = null.retained_storage().unwrap();
        for invalid in [
            EvaluatedArgs::Bytes(Some(Vec::new())),
            EvaluatedArgs::Bytes(Some(b"x".to_vec())),
            EvaluatedArgs::BytesInt(None, None),
        ] {
            assert!(matches!(
                null.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(null.kernel_invocations(), 0);
        }
        assert_eq!(
            bytes(null.eval_args(EvaluatedArgs::Bytes(None)).unwrap()),
            None
        );
        assert_eq!(null.kernel_invocations(), 1);
        assert!(null.is_healthy());
        assert_eq!(null.retained_storage().unwrap(), storage);

        let mut head = prepare(
            EvaluatedBytesOp::Timestamp2BaseNative,
            ExecutionLimits::default(),
        );
        let head_storage = head.retained_storage().unwrap();
        let base = bytes(
            head.eval_args(parse_args(
                "2020-01-01 10:00:00",
                false,
                NativeSessionTimeZone::utc(),
            ))
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            decode_native_timestamp_result(&base),
            Some(NativeTimestampResult::Base(_))
        ));
        assert_eq!(base.len(), 11);
        let mut add = prepare(
            EvaluatedBytesOp::Timestamp2AddNative,
            ExecutionLimits::default(),
        );
        let storage = add.retained_storage().unwrap();
        let other_kind = encode_native_identity(NativeIdentityRef::Int(1)).unwrap();
        let mut short = base.clone();
        short.pop();
        let mut trailing = base.clone();
        trailing.push(0);
        for invalid in [
            EvaluatedArgs::Bytes2(None, None),
            EvaluatedArgs::Bytes2(Some(other_kind), None),
            EvaluatedArgs::Bytes2(Some(short), Some(b"01:00:00".to_vec())),
            EvaluatedArgs::Bytes2(Some(trailing), None),
            EvaluatedArgs::Bytes2(Some(base.clone()), Some(vec![255])),
        ] {
            assert!(matches!(
                add.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(add.kernel_invocations(), 0);
            assert!(add.is_healthy());
            assert_eq!(add.retained_storage().unwrap(), storage);
        }
        for (index, (rhs, expected)) in [
            (Some("01:00:00.5"), Some("2020-01-01 11:00:00.5")),
            (None, None),
            (Some("bad"), None),
            (Some("2020-01-01 05:00:00"), None),
        ]
        .into_iter()
        .enumerate()
        {
            // Forward the actual complete Base report from the first worker.
            // No native calendar formatting or reconstructed identity packet.
            let output = bytes(
                add.eval_args(EvaluatedArgs::Bytes2(
                    Some(base.clone()),
                    rhs.map(|value| value.as_bytes().to_vec()),
                ))
                .unwrap(),
            );
            assert_eq!(output.as_deref(), expected.map(str::as_bytes));
            assert_eq!(add.kernel_invocations(), index as u64 + 1);
            assert!(add.is_healthy());
            assert_eq!(add.retained_storage().unwrap(), storage);
        }
        let zero_year = bytes(
            head.eval_args(parse_args(
                "0000-12-31 00:00:00",
                false,
                NativeSessionTimeZone::utc(),
            ))
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            decode_native_timestamp_result(&zero_year),
            Some(NativeTimestampResult::Base(_))
        ));
        // The real RHS is already present. Year zero returns NULL even though
        // adding 838 hours could otherwise land in an in-range year-one value.
        assert_eq!(
            bytes(
                add.eval_args(EvaluatedArgs::Bytes2(
                    Some(zero_year.clone()),
                    Some(b"838:00:00".to_vec())
                ))
                .unwrap()
            ),
            None
        );
        assert_eq!(add.kernel_invocations(), 5);
        assert!(matches!(
            add.eval_args(EvaluatedArgs::Bytes2(Some(zero_year), Some(vec![255]))),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(add.kernel_invocations(), 5);
        let warning = bytes(
            head.eval_args(parse_args("bad", false, NativeSessionTimeZone::utc()))
                .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            decode_native_timestamp_result(&warning),
            Some(NativeTimestampResult::Warning { code: 1292, .. })
        ));
        assert!(matches!(
            add.eval_args(EvaluatedArgs::Bytes2(Some(warning), None)),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(add.kernel_invocations(), 5);
        assert!(add.is_healthy());
        assert_eq!(add.retained_storage().unwrap(), storage);
        assert_eq!(head.kernel_invocations(), 3);
        assert!(head.is_healthy());
        assert_eq!(head.retained_storage().unwrap(), head_storage);
    }
}

#[cfg(test)]
mod unix_timestamp_worker_tests {
    use tidb_query_datatype::codec::mysql::{Time, TimeType, time::NativeSessionTimeZone};

    use super::*;
    use crate::{
        NativeIdentityRef, NativeUnixTimestampResult, decode_native_unix_timestamp_result,
        encode_native_identity,
    };

    fn prepare(operation: EvaluatedBytesOp, limits: ExecutionLimits) -> EvaluatedBytesWorker {
        prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            limits,
            usize::MAX,
        )
        .unwrap()
    }
    fn bytes(value: ComputedValue) -> Option<Vec<u8>> {
        let ComputedValue::Bytes(value) = value else {
            panic!("UNIX_TIMESTAMP must own Bytes");
        };
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        value.into_option()
    }
    fn text(value: &str, zone: NativeSessionTimeZone) -> EvaluatedArgs {
        EvaluatedArgs::TemporalParseText {
            value: value.as_bytes().to_vec(),
            is_float: false,
            zone,
        }
    }
    fn actual_time(raw: u64, kind: TimeType, fsp: u8) -> Vec<u8> {
        // An actual typed SQL temporal representation, not a computed epoch or
        // a fabricated parse-head continuation.
        encode_native_identity(NativeIdentityRef::Time {
            core: raw,
            kind: kind as u8,
            fsp,
        })
        .unwrap()
    }
    fn int_result(output: &[u8], expected: i64) {
        assert!(
            matches!(decode_native_unix_timestamp_result(output), Some(NativeUnixTimestampResult::Value(NativeIdentityRef::Int(value))) if value == expected)
        );
    }
    fn decimal_result(output: &[u8], expected_coefficient: u64, expected_scale: u32) {
        let Some(NativeUnixTimestampResult::Value(NativeIdentityRef::Decimal {
            negative,
            scale,
            storage_scale,
            coefficient,
            ..
        })) = decode_native_unix_timestamp_result(output)
        else {
            panic!("expected actual Decimal identity");
        };
        assert!(!negative);
        assert_eq!(scale, expected_scale);
        assert_eq!(storage_scale, expected_scale);
        if expected_coefficient == 0 {
            assert!(coefficient.iter().all(|byte| *byte == b'0'));
        } else {
            assert_eq!(
                std::str::from_utf8(coefficient)
                    .unwrap()
                    .parse::<u64>()
                    .unwrap(),
                expected_coefficient
            );
        }
    }

    #[test]
    fn unix_timestamp_now_parse_and_value_use_actual_stage_data_and_distinct_zone_reads() {
        let mut now = prepare(
            EvaluatedBytesOp::UnixTimestampNowNative,
            ExecutionLimits::default(),
        );
        let now_storage = now.retained_storage().unwrap();
        for invalid in [
            EvaluatedArgs::Bytes(None),
            EvaluatedArgs::Int2(None, Some(0)),
            EvaluatedArgs::Int2(Some(0), None),
            EvaluatedArgs::Int2(Some(0), Some(-1)),
            EvaluatedArgs::Int2(Some(0), Some(i64::from(u32::MAX) + 1)),
        ] {
            assert!(matches!(
                now.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(now.kernel_invocations(), 0);
        }
        for (index, (seconds, nanos, expected)) in [(1, 0, 1), (0, i64::from(u32::MAX), 4)]
            .into_iter()
            .enumerate()
        {
            let output = bytes(
                now.eval_args(EvaluatedArgs::Int2(Some(seconds), Some(nanos)))
                    .unwrap(),
            )
            .unwrap();
            int_result(&output, expected);
            assert_eq!(now.kernel_invocations(), index as u64 + 1);
            assert!(now.is_healthy());
            assert_eq!(now.retained_storage().unwrap(), now_storage);
        }
        let mut null = prepare(
            EvaluatedBytesOp::UnixTimestampNullNative,
            ExecutionLimits::default(),
        );
        assert!(matches!(
            null.eval_args(EvaluatedArgs::Bytes(Some(Vec::new()))),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(null.kernel_invocations(), 0);
        assert_eq!(
            bytes(null.eval_args(EvaluatedArgs::Bytes(None)).unwrap()),
            None
        );
        assert_eq!(null.kernel_invocations(), 1);
        assert!(null.is_healthy());

        let mut head = prepare(
            EvaluatedBytesOp::UnixTimestampParseNative,
            ExecutionLimits::default(),
        );
        let head_storage = head.retained_storage().unwrap();
        for invalid in [
            EvaluatedArgs::BytesInt(Some(b"1970-01-01 01:00:01".to_vec()), Some(0)),
            EvaluatedArgs::TemporalParseText {
                value: vec![255],
                is_float: false,
                zone: NativeSessionTimeZone::utc(),
            },
        ] {
            assert!(matches!(
                head.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(head.kernel_invocations(), 0);
        }
        let continued = bytes(
            head.eval_args(text("1970-01-01 01:00:01", NativeSessionTimeZone::utc()))
                .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            decode_native_unix_timestamp_result(&continued),
            Some(NativeUnixTimestampResult::Continue(_))
        ));
        let mut value = prepare(
            EvaluatedBytesOp::UnixTimestampValueNative,
            ExecutionLimits::default(),
        );
        let value_storage = value.retained_storage().unwrap();
        let mut broken = continued.clone();
        broken.pop();
        for invalid in [
            EvaluatedArgs::Bytes(Some(continued.clone())),
            EvaluatedArgs::TemporalValue {
                value: broken,
                zone: NativeSessionTimeZone::utc(),
            },
            EvaluatedArgs::TemporalValue {
                value: actual_time(
                    Time::native_core_from_fields(1970, 1, 1, 1, 0, 1, 0),
                    TimeType::Date,
                    0,
                ),
                zone: NativeSessionTimeZone::utc(),
            },
            EvaluatedArgs::TemporalValue {
                value: actual_time(
                    Time::native_core_from_fields(1970, 1, 1, 1, 0, 1, 0),
                    TimeType::DateTime,
                    7,
                ),
                zone: NativeSessionTimeZone::utc(),
            },
        ] {
            assert!(matches!(
                value.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(value.kernel_invocations(), 0);
        }
        // Pass the untouched, actual head-produced frame, with a freshly read
        // second zone. UTC yields 3601; +01:00 yields the known epoch second 1.
        for (index, (zone, expected)) in [
            (NativeSessionTimeZone::utc(), 3601),
            (
                NativeSessionTimeZone::Fixed {
                    name: "UTC".to_owned(),
                    offset_secs: 3600,
                },
                1,
            ),
            (NativeSessionTimeZone::utc(), 3601),
        ]
        .into_iter()
        .enumerate()
        {
            let output = bytes(
                value
                    .eval_args(EvaluatedArgs::TemporalValue {
                        value: continued.clone(),
                        zone,
                    })
                    .unwrap(),
            )
            .unwrap();
            int_result(&output, expected);
            assert_eq!(value.kernel_invocations(), index as u64 + 1);
            assert!(value.is_healthy());
            assert_eq!(value.retained_storage().unwrap(), value_storage);
        }
        assert_eq!(
            bytes(
                head.eval_args(text(
                    "0000-00-00 00:00:00.000",
                    NativeSessionTimeZone::utc()
                ))
                .unwrap()
            ),
            None
        );
        let terminal = bytes(
            head.eval_args(text(
                "2017-00-02 00:00:00.000",
                NativeSessionTimeZone::utc(),
            ))
            .unwrap(),
        )
        .unwrap();
        decimal_result(&terminal, 0, 3);
        assert_eq!(
            value.kernel_invocations(),
            3,
            "terminal head outcomes do not demand the value stage"
        );
        assert!(matches!(
            value.eval_args(EvaluatedArgs::TemporalValue {
                value: terminal,
                zone: NativeSessionTimeZone::utc()
            }),
            Err(LocalError::InvalidBatch(_))
        ));
        let warning = bytes(
            head.eval_args(text("bad", NativeSessionTimeZone::utc()))
                .unwrap(),
        )
        .unwrap();
        assert!(
            matches!(decode_native_unix_timestamp_result(&warning), Some(NativeUnixTimestampResult::Warning { code: 1292, message }) if message == "Incorrect datetime value: 'bad'")
        );
        assert_eq!(head.kernel_invocations(), 4);
        assert!(head.is_healthy());
        assert_eq!(head.retained_storage().unwrap(), head_storage);
        assert!(value.is_healthy());
        assert_eq!(value.retained_storage().unwrap(), value_storage);
    }

    #[test]
    fn unix_timestamp_legacy_raw_time_gap_policy_and_zone_owner_budget_remain_distinct() {
        let raw = Time::native_core_from_fields(1970, 1, 1, 0, 0, 1, 500_000);
        let gap = Time::native_core_from_fields(2025, 3, 30, 2, 30, 0, 0);
        let paris = || NativeSessionTimeZone::Named("Europe/Paris".parse().unwrap());
        for operation in [
            EvaluatedBytesOp::UnixTimestampIntLegacy,
            EvaluatedBytesOp::UnixTimestampDecLegacy,
        ] {
            let mut worker = prepare(operation, ExecutionLimits::default());
            let storage = worker.retained_storage().unwrap();
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::Bytes(Some(actual_time(
                    raw,
                    TimeType::DateTime,
                    6
                )))),
                Err(LocalError::InvalidBatch(_))
            ));
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::TemporalValue {
                    value: encode_native_identity(NativeIdentityRef::Int(1)).unwrap(),
                    zone: NativeSessionTimeZone::utc(),
                }),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            for (index, kind) in [TimeType::Date, TimeType::DateTime, TimeType::Timestamp]
                .into_iter()
                .enumerate()
            {
                let output = bytes(
                    worker
                        .eval_args(EvaluatedArgs::TemporalValue {
                            value: actual_time(raw, kind, 255),
                            zone: NativeSessionTimeZone::utc(),
                        })
                        .unwrap(),
                )
                .unwrap();
                // DATE's hidden clock/micros are preserved. Legacy signatures
                // ignore even raw FSP 255 and keep distinct INT / DECIMAL rules.
                if operation == EvaluatedBytesOp::UnixTimestampIntLegacy {
                    int_result(&output, 1);
                } else {
                    decimal_result(&output, 1_500_000, 6);
                }
                assert_eq!(worker.kernel_invocations(), index as u64 + 1);
                assert!(worker.is_healthy());
                assert_eq!(worker.retained_storage().unwrap(), storage);
            }
            for (index, (core, zone)) in [(0, NativeSessionTimeZone::utc()), (gap, paris())]
                .into_iter()
                .enumerate()
            {
                let output = bytes(
                    worker
                        .eval_args(EvaluatedArgs::TemporalValue {
                            value: actual_time(core, TimeType::DateTime, 255),
                            zone,
                        })
                        .unwrap(),
                )
                .unwrap();
                if operation == EvaluatedBytesOp::UnixTimestampIntLegacy {
                    int_result(&output, 0);
                } else {
                    decimal_result(&output, 0, 0);
                }
                assert_eq!(worker.kernel_invocations(), index as u64 + 4);
                assert!(worker.is_healthy());
                assert_eq!(worker.retained_storage().unwrap(), storage);
            }
        }
        let mut ordinary = prepare(
            EvaluatedBytesOp::UnixTimestampValueNative,
            ExecutionLimits::default(),
        );
        let storage = ordinary.retained_storage().unwrap();
        let output = bytes(
            ordinary
                .eval_args(EvaluatedArgs::TemporalValue {
                    value: actual_time(gap, TimeType::DateTime, 0),
                    zone: paris(),
                })
                .unwrap(),
        )
        .unwrap();
        // Existing native Paris spring-gap source table: ordinary conversion
        // returns the transition, whereas the strict legacy signatures give 0.
        int_result(&output, 1_743_296_400);
        assert!(ordinary.is_healthy());
        assert_eq!(ordinary.retained_storage().unwrap(), storage);

        for operation in [
            EvaluatedBytesOp::UnixTimestampParseNative,
            EvaluatedBytesOp::UnixTimestampValueNative,
        ] {
            let mut worker = prepare(
                operation,
                ExecutionLimits {
                    max_retained_bytes: 64 * 1024,
                    ..ExecutionLimits::default()
                },
            );
            let storage = worker.retained_storage().unwrap();
            let mut name = String::with_capacity(128 * 1024);
            name.push_str("UTC");
            assert!(name.capacity() > 64 * 1024 && name.len() == 3);
            let args = |zone| {
                if operation == EvaluatedBytesOp::UnixTimestampParseNative {
                    text("1970-01-01 00:00:01", zone)
                } else {
                    EvaluatedArgs::TemporalValue {
                        value: actual_time(raw, TimeType::DateTime, 0),
                        zone,
                    }
                }
            };
            assert!(matches!(
                worker.eval_args(args(NativeSessionTimeZone::Fixed {
                    name,
                    offset_secs: 0
                })),
                Err(LocalError::ResourceLimit(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            let output = bytes(
                worker
                    .eval_args(args(NativeSessionTimeZone::utc()))
                    .unwrap(),
            )
            .unwrap();
            if operation == EvaluatedBytesOp::UnixTimestampParseNative {
                assert!(matches!(
                    decode_native_unix_timestamp_result(&output),
                    Some(NativeUnixTimestampResult::Continue(_))
                ));
            } else {
                int_result(&output, 1);
            }
            assert_eq!(worker.kernel_invocations(), 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[cfg(test)]
mod from_unixtime_worker_tests {
    use tidb_query_datatype::codec::mysql::{Time, time::NativeSessionTimeZone};

    use super::*;
    use crate::{
        NativeFromUnixTimeResult, NativeIdentityRef, decode_native_from_unixtime_result,
        decode_native_identity, encode_native_identity,
    };

    fn prepare(operation: EvaluatedBytesOp, limits: ExecutionLimits) -> EvaluatedBytesWorker {
        prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            limits,
            usize::MAX,
        )
        .unwrap()
    }
    fn bytes(value: ComputedValue) -> Option<Vec<u8>> {
        let ComputedValue::Bytes(value) = value else {
            panic!("FROM_UNIXTIME must own Bytes");
        };
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        value.into_option()
    }
    fn decimal(negative: bool, coefficient: &[u8], scale: u32) -> Vec<u8> {
        encode_native_identity(NativeIdentityRef::Decimal {
            negative,
            scale,
            storage_scale: scale,
            declared_shape: None,
            coefficient,
        })
        .unwrap()
    }
    fn continuation(output: &[u8], seconds: i64, micros: u32, fsp: usize) {
        let Some(NativeFromUnixTimeResult::Continue(epoch)) =
            decode_native_from_unixtime_result(output)
        else {
            panic!("expected actual epoch continuation");
        };
        assert_eq!(
            (epoch.seconds, epoch.micros, epoch.fsp as usize),
            (seconds, micros, fsp)
        );
    }

    #[test]
    fn from_unixtime_heads_keep_actual_numeric_text_domains_and_truncate_handoff() {
        let mut numeric = prepare(
            EvaluatedBytesOp::FromUnixTimeNumericNative,
            ExecutionLimits::default(),
        );
        let storage = numeric.retained_storage().unwrap();
        for invalid in [
            EvaluatedArgs::Bytes(None),
            EvaluatedArgs::Int(Some(1)),
            EvaluatedArgs::Bytes(Some(vec![255])),
            EvaluatedArgs::Bytes(Some(
                encode_native_identity(NativeIdentityRef::Bytes(b"1")).unwrap(),
            )),
        ] {
            assert!(matches!(
                numeric.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(numeric.kernel_invocations(), 0);
        }
        let mut calls = 0;
        for (frame, expected) in [
            (
                encode_native_identity(NativeIdentityRef::Int(1)).unwrap(),
                Some((1, 0, 0)),
            ),
            (
                encode_native_identity(NativeIdentityRef::UInt(u64::MAX)).unwrap(),
                None,
            ),
            (decimal(true, b"1", 1), Some((0, 100_000, 1))),
            (
                encode_native_identity(NativeIdentityRef::Real((-0.1_f64).to_bits())).unwrap(),
                Some((0, 100_000, 6)),
            ),
            (
                encode_native_identity(NativeIdentityRef::Float32(16_777_217.0_f64.to_bits()))
                    .unwrap(),
                Some((16_777_217, 0, 6)),
            ),
            (
                encode_native_identity(NativeIdentityRef::Real(f64::NAN.to_bits())).unwrap(),
                None,
            ),
            (
                encode_native_identity(NativeIdentityRef::Real(f64::INFINITY.to_bits())).unwrap(),
                None,
            ),
            (
                encode_native_identity(NativeIdentityRef::Real(f64::NEG_INFINITY.to_bits()))
                    .unwrap(),
                None,
            ),
            (
                decimal(false, concat!("32536771199", "9999999").as_bytes(), 7),
                Some((32_536_771_200, 0, 6)),
            ),
        ] {
            let output = bytes(
                numeric
                    .eval_args(EvaluatedArgs::Bytes(Some(frame)))
                    .unwrap(),
            );
            match (output, expected) {
                (Some(output), Some((seconds, micros, fsp))) => {
                    continuation(&output, seconds, micros, fsp)
                }
                (None, None) => {}
                _ => panic!("numeric source must retain its original epoch/NULL policy"),
            }
            calls += 1;
            assert_eq!(numeric.kernel_invocations(), calls);
            assert!(numeric.is_healthy());
            assert_eq!(numeric.retained_storage().unwrap(), storage);
        }
        let mut text = prepare(
            EvaluatedBytesOp::FromUnixTimeTextNative,
            ExecutionLimits::default(),
        );
        let text_storage = text.retained_storage().unwrap();
        for invalid in [
            EvaluatedArgs::Bytes(None),
            EvaluatedArgs::Bytes(Some(vec![255])),
            EvaluatedArgs::Int(Some(1)),
        ] {
            assert!(matches!(
                text.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(text.kernel_invocations(), 0);
        }
        for (index, (input, expected)) in [
            ("-0.1", Some((0, 100_000, 6))),
            ("1.123456789tail", Some((1, 123_457, 6))),
            ("1.12345678xtail", None),
            ("32536771199.9999999", Some((32_536_771_200, 0, 6))),
        ]
        .into_iter()
        .enumerate()
        {
            let output = bytes(
                text.eval_args(EvaluatedArgs::Bytes(Some(input.as_bytes().to_vec())))
                    .unwrap(),
            );
            match (output, expected) {
                (Some(output), Some((seconds, micros, fsp))) => {
                    continuation(&output, seconds, micros, fsp)
                }
                (None, None) => {}
                _ => panic!("text fraction handling changed"),
            }
            assert_eq!(text.kernel_invocations(), index as u64 + 1);
            assert!(text.is_healthy());
            assert_eq!(text.retained_storage().unwrap(), text_storage);
        }
        // Unlike actual UInt(MAX), its textual spelling produces a truncation
        // report with a real epoch-zero payload. Do not fabricate that payload.
        let truncate = bytes(
            text.eval_args(EvaluatedArgs::Bytes(Some(b"18446744073709551615".to_vec())))
                .unwrap(),
        )
        .unwrap();
        let Some(NativeFromUnixTimeResult::Truncate { epoch, message }) =
            decode_native_from_unixtime_result(&truncate)
        else {
            panic!("text integer overflow must report truncation");
        };
        assert_eq!((epoch.seconds, epoch.micros, epoch.fsp as usize), (0, 0, 0));
        assert_eq!(
            message,
            "Truncated incorrect DECIMAL value: '18446744073709551615'"
        );
        let mut local = prepare(
            EvaluatedBytesOp::FromUnixTimeLocalNative,
            ExecutionLimits::default(),
        );
        let local_storage = local.retained_storage().unwrap();
        let mut invalid_message = truncate.clone();
        *invalid_message.last_mut().unwrap() = 255;
        assert!(matches!(
            local.eval_args(EvaluatedArgs::TemporalValue {
                value: invalid_message,
                zone: NativeSessionTimeZone::utc(),
            }),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(local.kernel_invocations(), 0);
        let output = bytes(
            local
                .eval_args(EvaluatedArgs::TemporalValue {
                    value: truncate,
                    zone: NativeSessionTimeZone::Fixed {
                        name: "UTC".to_owned(),
                        offset_secs: 3600,
                    },
                })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(output.as_slice(), b"1970-01-01 01:00:00");
        assert_eq!(local.kernel_invocations(), 1);
        assert!(local.is_healthy());
        assert_eq!(local.retained_storage().unwrap(), local_storage);
        assert_eq!(text.kernel_invocations(), 5);
        assert!(text.is_healthy());
        assert_eq!(text.retained_storage().unwrap(), text_storage);
        let mut null = prepare(
            EvaluatedBytesOp::FromUnixTimeNullNative,
            ExecutionLimits::default(),
        );
        assert!(matches!(
            null.eval_args(EvaluatedArgs::Bytes(Some(Vec::new()))),
            Err(LocalError::InvalidBatch(_))
        ));
        assert_eq!(null.kernel_invocations(), 0);
        assert_eq!(
            bytes(null.eval_args(EvaluatedArgs::Bytes(None)).unwrap()),
            None
        );
        assert_eq!(null.kernel_invocations(), 1);
        assert!(null.is_healthy());
    }

    #[test]
    fn from_unixtime_local_and_legacy_keep_actual_frames_precision_and_zone_owner_cleanup() {
        let mut head = prepare(
            EvaluatedBytesOp::FromUnixTimeNumericNative,
            ExecutionLimits::default(),
        );
        let actual = bytes(
            head.eval_args(EvaluatedArgs::Bytes(Some(
                encode_native_identity(NativeIdentityRef::Int(1)).unwrap(),
            )))
            .unwrap(),
        )
        .unwrap();
        continuation(&actual, 1, 0, 0);
        assert_eq!(actual.len(), 14);
        // Framing alone admits the full signed seconds domain. These modified
        // frames are validator probes only, never fabricated business inputs.
        let mut any_seconds = actual.clone();
        for seconds in [i64::MIN, i64::MAX] {
            any_seconds[1..9].copy_from_slice(&seconds.to_le_bytes());
            assert!(crate::from_unixtime_local_native_args_valid(Some(
                &any_seconds
            )));
        }
        let mut local = prepare(
            EvaluatedBytesOp::FromUnixTimeLocalNative,
            ExecutionLimits::default(),
        );
        let storage = local.retained_storage().unwrap();
        let mut short = actual.clone();
        short.pop();
        let mut bad_micro = actual.clone();
        bad_micro[9..13].copy_from_slice(&1_000_000_u32.to_le_bytes());
        let mut bad_fsp = actual.clone();
        bad_fsp[13] = 7;
        let mut trailing = actual.clone();
        trailing.push(0);
        assert!(matches!(
            local.eval_args(EvaluatedArgs::Bytes(Some(actual.clone()))),
            Err(LocalError::InvalidBatch(_))
        ));
        for frame in [short, bad_micro, bad_fsp, trailing] {
            assert!(matches!(
                local.eval_args(EvaluatedArgs::TemporalValue {
                    value: frame,
                    zone: NativeSessionTimeZone::utc()
                }),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(local.kernel_invocations(), 0);
        }
        for (index, (zone, expected)) in [
            (NativeSessionTimeZone::utc(), "1970-01-01 00:00:01"),
            (
                NativeSessionTimeZone::Fixed {
                    name: "UTC".to_owned(),
                    offset_secs: 3600,
                },
                "1970-01-01 01:00:01",
            ),
            (NativeSessionTimeZone::utc(), "1970-01-01 00:00:01"),
        ]
        .into_iter()
        .enumerate()
        {
            let output = bytes(
                local
                    .eval_args(EvaluatedArgs::TemporalValue {
                        value: actual.clone(),
                        zone,
                    })
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(output, expected.as_bytes());
            assert_eq!(local.kernel_invocations(), index as u64 + 1);
            assert!(local.is_healthy());
            assert_eq!(local.retained_storage().unwrap(), storage);
        }
        let mut legacy = prepare(
            EvaluatedBytesOp::FromUnixTimeLegacy,
            ExecutionLimits::default(),
        );
        let legacy_storage = legacy.retained_storage().unwrap();
        // Raw coefficients retain at least their storage width; the original
        // constructor's 0.000000001 has these nine coefficient bytes.
        let tiny = decimal(false, b"000000001", 9);
        let mut broken_decimal = tiny.clone();
        // Removing coefficient bytes is still a valid raw representation.
        // This invalid-frame probe must instead truncate the fixed header.
        broken_decimal.truncate(26);
        for invalid in [
            EvaluatedArgs::Bytes(Some(tiny.clone())),
            EvaluatedArgs::TemporalValue {
                value: broken_decimal,
                zone: NativeSessionTimeZone::utc(),
            },
            EvaluatedArgs::TemporalValue {
                value: actual.clone(),
                zone: NativeSessionTimeZone::utc(),
            },
            EvaluatedArgs::TemporalValue {
                value: encode_native_identity(NativeIdentityRef::Int(1)).unwrap(),
                zone: NativeSessionTimeZone::utc(),
            },
        ] {
            assert!(matches!(
                legacy.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(legacy.kernel_invocations(), 0);
        }
        for (index, (zone, hour)) in [
            (NativeSessionTimeZone::utc(), 0),
            (
                NativeSessionTimeZone::Fixed {
                    name: "UTC".to_owned(),
                    offset_secs: 3600,
                },
                1,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            // Actual Decimal 0.000000001: retain the legacy nanos*1000 path,
            // which produces one hidden microsecond while result FSP stays 0.
            let output = bytes(
                legacy
                    .eval_args(EvaluatedArgs::TemporalValue {
                        value: tiny.clone(),
                        zone,
                    })
                    .unwrap(),
            )
            .unwrap();
            let NativeIdentityRef::Time { core, kind, fsp } =
                decode_native_identity(&output).unwrap()
            else {
                panic!("legacy must return actual Time identity");
            };
            assert_eq!(
                core,
                Time::native_core_from_fields(1970, 1, 1, hour, 0, 0, 1)
            );
            assert_eq!((kind, fsp), (1, 0));
            assert_eq!(legacy.kernel_invocations(), index as u64 + 1);
            assert!(legacy.is_healthy());
            assert_eq!(legacy.retained_storage().unwrap(), legacy_storage);
        }
        assert_eq!(
            bytes(
                legacy
                    .eval_args(EvaluatedArgs::TemporalValue {
                        value: decimal(true, b"1", 0),
                        zone: NativeSessionTimeZone::utc(),
                    })
                    .unwrap()
            ),
            None
        );
        assert_eq!(legacy.kernel_invocations(), 3);
        assert!(legacy.is_healthy());
        assert_eq!(legacy.retained_storage().unwrap(), legacy_storage);
        for operation in [
            EvaluatedBytesOp::FromUnixTimeLocalNative,
            EvaluatedBytesOp::FromUnixTimeLegacy,
        ] {
            let mut bounded = prepare(
                operation,
                ExecutionLimits {
                    max_retained_bytes: 64 * 1024,
                    ..ExecutionLimits::default()
                },
            );
            let storage = bounded.retained_storage().unwrap();
            let frame = if operation == EvaluatedBytesOp::FromUnixTimeLocalNative {
                actual.clone()
            } else {
                tiny.clone()
            };
            let mut name = String::with_capacity(128 * 1024);
            name.push_str("UTC");
            assert!(name.capacity() > 64 * 1024 && name.len() == 3);
            assert!(matches!(
                bounded.eval_args(EvaluatedArgs::TemporalValue {
                    value: frame.clone(),
                    zone: NativeSessionTimeZone::Fixed {
                        name,
                        offset_secs: 0
                    },
                }),
                Err(LocalError::ResourceLimit(_))
            ));
            assert_eq!(bounded.kernel_invocations(), 0);
            assert!(bounded.is_healthy());
            assert_eq!(bounded.retained_storage().unwrap(), storage);
            assert!(
                bytes(
                    bounded
                        .eval_args(EvaluatedArgs::TemporalValue {
                            value: frame,
                            zone: NativeSessionTimeZone::utc()
                        })
                        .unwrap()
                )
                .is_some()
            );
            assert_eq!(bounded.kernel_invocations(), 1);
            assert!(bounded.is_healthy());
            assert_eq!(bounded.retained_storage().unwrap(), storage);
        }
    }
}

#[cfg(test)]
mod if_null_worker_tests {
    use tidb_query_datatype::codec::mysql::Time;

    use super::*;
    use crate::{
        NativeIdentityRef, NativeIfNullHeadResult, decode_native_if_null_head_result,
        encode_native_identity,
    };

    fn prepare(operation: EvaluatedBytesOp, limit: usize) -> EvaluatedBytesWorker {
        prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits {
                max_retained_bytes: limit,
                ..ExecutionLimits::default()
            },
            usize::MAX,
        )
        .unwrap()
    }
    fn bytes(value: ComputedValue) -> Option<Vec<u8>> {
        let ComputedValue::Bytes(value) = value else {
            panic!("IFNULL must own Bytes");
        };
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        value.into_option()
    }

    #[test]
    fn if_null_workers_dispatch_actual_null_and_preserve_opaque_identity_frames() {
        let mut head = prepare(EvaluatedBytesOp::IfNullHeadNative, 64 * 1024 * 1024);
        let head_storage = head.retained_storage().unwrap();
        for invalid in [
            EvaluatedArgs::Int(None),
            EvaluatedArgs::Bytes(Some(Vec::new())),
            EvaluatedArgs::Bytes(Some(vec![0])),
        ] {
            assert!(matches!(
                head.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(head.kernel_invocations(), 0);
        }
        let need = bytes(head.eval_one(None).unwrap())
            .expect("NULL input must produce a real NeedSecond report");
        assert!(matches!(
            decode_native_if_null_head_result(&need),
            Some(NativeIfNullHeadResult::NeedSecond)
        ));
        assert_eq!(need, vec![0]);
        assert_eq!(head.kernel_invocations(), 1);
        let mut finish = prepare(EvaluatedBytesOp::IfNullFinishNative, 64 * 1024 * 1024);
        let finish_storage = finish.retained_storage().unwrap();
        for invalid in [
            EvaluatedArgs::Bytes(Some(need.clone())),
            EvaluatedArgs::Bytes2(None, None),
            EvaluatedArgs::Bytes2(Some(vec![0, 0]), None),
            EvaluatedArgs::Bytes2(Some(vec![1]), None),
            EvaluatedArgs::Bytes2(Some(need.clone()), Some(vec![0])),
        ] {
            assert!(matches!(
                finish.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(finish.kernel_invocations(), 0);
        }
        let views = [
            NativeIdentityRef::MinNotNull,
            NativeIdentityRef::MaxValue,
            NativeIdentityRef::Int(0),
            NativeIdentityRef::Float32(16_777_217.0_f64.to_bits()),
            NativeIdentityRef::Decimal {
                negative: false,
                scale: 9,
                storage_scale: 9,
                declared_shape: None,
                coefficient: b"",
            },
            NativeIdentityRef::Decimal {
                negative: true,
                scale: u32::MAX,
                storage_scale: 9,
                declared_shape: Some((-1, 99)),
                coefficient: b"\xff\0+",
            },
            NativeIdentityRef::Time {
                core: Time::native_core_from_fields(1970, 1, 1, 0, 0, 1, 7),
                kind: 0,
                fsp: 255,
            },
        ];
        for (index, view) in views.into_iter().enumerate() {
            // Decimal tails are opaque even when formatting them would panic;
            // Float32 bits and DATE's hidden clock must not be interpreted.
            let frame = encode_native_identity(view).unwrap();
            let done = bytes(head.eval_one(Some(frame.clone())).unwrap()).unwrap();
            assert!(
                matches!(decode_native_if_null_head_result(&done), Some(NativeIfNullHeadResult::Done(actual)) if actual == frame.as_slice())
            );
            assert!(matches!(
                finish.eval_args(EvaluatedArgs::Bytes2(Some(done), None)),
                Err(LocalError::InvalidBatch(_))
            ));
            let output = bytes(
                finish
                    .eval_args(EvaluatedArgs::Bytes2(
                        Some(need.clone()),
                        Some(frame.clone()),
                    ))
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(output, frame);
            assert_eq!(head.kernel_invocations(), index as u64 + 2);
            assert_eq!(finish.kernel_invocations(), index as u64 + 1);
            assert!(head.is_healthy() && finish.is_healthy());
            assert_eq!(head.retained_storage().unwrap(), head_storage);
            assert_eq!(finish.retained_storage().unwrap(), finish_storage);
        }
        assert_eq!(
            bytes(
                finish
                    .eval_args(EvaluatedArgs::Bytes2(Some(need), None))
                    .unwrap()
            ),
            None
        );
        assert_eq!(finish.kernel_invocations(), 8);
        assert!(finish.is_healthy());
        assert_eq!(finish.retained_storage().unwrap(), finish_storage);
    }

    #[test]
    fn if_null_worker_output_and_actual_capacity_refusals_precede_dispatch() {
        let mut empty_budget = prepare(EvaluatedBytesOp::IfNullHeadNative, 0);
        let storage = empty_budget.retained_storage().unwrap();
        for _ in 0..2 {
            // No input payload exists, but NeedSecond still needs one output
            // byte. Refusal must happen before the real wrapper invocation.
            assert!(matches!(
                empty_budget.eval_one(None),
                Err(LocalError::ResourceLimit(_))
            ));
            assert_eq!(empty_budget.kernel_invocations(), 0);
            assert!(empty_budget.is_healthy());
            assert_eq!(empty_budget.retained_storage().unwrap(), storage);
        }
        let mut producer = prepare(EvaluatedBytesOp::IfNullHeadNative, 64 * 1024 * 1024);
        let need = bytes(producer.eval_one(None).unwrap()).unwrap();
        for operation in [
            EvaluatedBytesOp::IfNullHeadNative,
            EvaluatedBytesOp::IfNullFinishNative,
        ] {
            let mut worker = prepare(operation, 64 * 1024);
            let storage = worker.retained_storage().unwrap();
            let mut frame = encode_native_identity(NativeIdentityRef::MaxValue).unwrap();
            frame.reserve_exact(128 * 1024);
            assert!(frame.capacity() > 64 * 1024 && frame.len() == 1);
            let args = if operation == EvaluatedBytesOp::IfNullHeadNative {
                EvaluatedArgs::Bytes(Some(frame))
            } else {
                EvaluatedArgs::Bytes2(Some(need.clone()), Some(frame))
            };
            assert!(matches!(
                worker.eval_args(args),
                Err(LocalError::ResourceLimit(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            let args = if operation == EvaluatedBytesOp::IfNullHeadNative {
                EvaluatedArgs::Bytes(None)
            } else {
                EvaluatedArgs::Bytes2(Some(need.clone()), None)
            };
            let output = bytes(worker.eval_args(args).unwrap());
            if operation == EvaluatedBytesOp::IfNullHeadNative {
                assert_eq!(output, Some(need.clone()));
            } else {
                assert_eq!(output, None);
            }
            assert_eq!(worker.kernel_invocations(), 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}

#[cfg(test)]
mod if_worker_tests {
    use tidb_query_datatype::codec::mysql::Time;

    use super::*;
    use crate::{
        NativeIdentityRef, NativeIfBranch, decode_native_if_head_result, encode_native_identity,
    };

    fn prepare(operation: EvaluatedBytesOp, limit: usize) -> EvaluatedBytesWorker {
        prepare_evaluated_bytes(
            operation,
            LocalCompileContext::default(),
            ExecutionLimits {
                max_retained_bytes: limit,
                ..ExecutionLimits::default()
            },
            usize::MAX,
        )
        .unwrap()
    }
    fn bytes(value: ComputedValue) -> Option<Vec<u8>> {
        let ComputedValue::Bytes(value) = value else {
            panic!("IF stages must own Bytes");
        };
        assert_eq!(value.metadata(), ComputedBytesMetadata::OwnBytes);
        value.into_option()
    }

    #[test]
    fn if_workers_keep_canonical_native_conditions_and_actual_selected_identities() {
        let mut head = prepare(EvaluatedBytesOp::IfHeadNative, 64 * 1024 * 1024);
        let head_storage = head.retained_storage().unwrap();
        // Only this native ready-condition profile is canonicalized; these
        // refusals do not narrow any existing wire Int/IF signature's domain.
        for invalid in [
            EvaluatedArgs::Bytes(None),
            EvaluatedArgs::Int2(None, None),
            EvaluatedArgs::Int(Some(2)),
            EvaluatedArgs::Int(Some(-1)),
        ] {
            assert!(matches!(
                head.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(head.kernel_invocations(), 0);
        }
        let mut reports = Vec::new();
        for (index, (condition, expected)) in [
            (None, NativeIfBranch::Else),
            (Some(0), NativeIfBranch::Else),
            (Some(1), NativeIfBranch::Then),
        ]
        .into_iter()
        .enumerate()
        {
            let report = bytes(head.eval_args(EvaluatedArgs::Int(condition)).unwrap())
                .expect("even real NULL must produce a branch report");
            assert_eq!(decode_native_if_head_result(&report), Some(expected));
            assert_eq!(
                report.as_slice(),
                if condition == Some(1) {
                    &[0][..]
                } else {
                    &[1][..]
                }
            );
            reports.push(report);
            assert_eq!(head.kernel_invocations(), index as u64 + 1);
            assert!(head.is_healthy());
            assert_eq!(head.retained_storage().unwrap(), head_storage);
        }
        let mut finish = prepare(EvaluatedBytesOp::IfFinishNative, 64 * 1024 * 1024);
        let storage = finish.retained_storage().unwrap();
        for invalid in [
            EvaluatedArgs::Bytes(None),
            EvaluatedArgs::Bytes2(None, None),
            EvaluatedArgs::Bytes2(Some(Vec::new()), None),
            EvaluatedArgs::Bytes2(Some(vec![2]), None),
            EvaluatedArgs::Bytes2(Some(vec![0, 0]), None),
            EvaluatedArgs::Bytes2(Some(reports[0].clone()), Some(vec![0])),
        ] {
            assert!(matches!(
                finish.eval_args(invalid),
                Err(LocalError::InvalidBatch(_))
            ));
            assert_eq!(finish.kernel_invocations(), 0);
        }
        let views = [
            NativeIdentityRef::MinNotNull,
            NativeIdentityRef::MaxValue,
            NativeIdentityRef::Int(0),
            NativeIdentityRef::Float32(16_777_217.0_f64.to_bits()),
            NativeIdentityRef::Decimal {
                negative: false,
                scale: 9,
                storage_scale: 9,
                declared_shape: None,
                coefficient: b"",
            },
            NativeIdentityRef::Decimal {
                negative: true,
                scale: u32::MAX,
                storage_scale: 9,
                declared_shape: Some((-1, 99)),
                coefficient: b"\xff\0+",
            },
            NativeIdentityRef::Time {
                core: Time::native_core_from_fields(1970, 1, 1, 0, 0, 1, 7),
                kind: 0,
                fsp: 255,
            },
        ];
        let mut calls = 0;
        for report in reports {
            // Only the actual selected value is supplied; all payload bytes,
            // including raw Decimal tails and hidden temporal fields, survive.
            for view in views {
                let frame = encode_native_identity(view).unwrap();
                assert_eq!(
                    bytes(
                        finish
                            .eval_args(EvaluatedArgs::Bytes2(
                                Some(report.clone()),
                                Some(frame.clone())
                            ))
                            .unwrap()
                    ),
                    Some(frame)
                );
                calls += 1;
                assert_eq!(finish.kernel_invocations(), calls);
                assert!(finish.is_healthy());
                assert_eq!(finish.retained_storage().unwrap(), storage);
            }
            assert_eq!(
                bytes(
                    finish
                        .eval_args(EvaluatedArgs::Bytes2(Some(report), None))
                        .unwrap()
                ),
                None
            );
            calls += 1;
            assert_eq!(
                finish.kernel_invocations(),
                calls,
                "selected SQL NULL still invokes the finish wrapper"
            );
            assert!(finish.is_healthy());
            assert_eq!(finish.retained_storage().unwrap(), storage);
        }
    }

    #[test]
    fn if_workers_charge_output_and_both_ready_capacities_before_dispatch() {
        let mut head = prepare(EvaluatedBytesOp::IfHeadNative, 0);
        let storage = head.retained_storage().unwrap();
        for condition in [None, Some(0), Some(1)] {
            assert!(matches!(
                head.eval_args(EvaluatedArgs::Int(condition)),
                Err(LocalError::ResourceLimit(_))
            ));
            assert_eq!(
                head.kernel_invocations(),
                0,
                "every branch report needs one output byte before dispatch"
            );
            assert!(head.is_healthy());
            assert_eq!(head.retained_storage().unwrap(), storage);
        }
        let mut producer = prepare(EvaluatedBytesOp::IfHeadNative, 64 * 1024 * 1024);
        let actual_report =
            bytes(producer.eval_args(EvaluatedArgs::Int(Some(1))).unwrap()).unwrap();
        for enlarge_report in [true, false] {
            let mut worker = prepare(EvaluatedBytesOp::IfFinishNative, 64 * 1024);
            let storage = worker.retained_storage().unwrap();
            let mut report = actual_report.clone();
            let selected = if enlarge_report {
                report.reserve_exact(128 * 1024);
                assert!(report.capacity() > 64 * 1024 && report.len() == 1);
                None
            } else {
                let mut frame = encode_native_identity(NativeIdentityRef::MaxValue).unwrap();
                frame.reserve_exact(128 * 1024);
                assert!(frame.capacity() > 64 * 1024 && frame.len() == 1);
                Some(frame)
            };
            assert!(matches!(
                worker.eval_args(EvaluatedArgs::Bytes2(Some(report), selected)),
                Err(LocalError::ResourceLimit(_))
            ));
            assert_eq!(worker.kernel_invocations(), 0);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
            assert_eq!(
                bytes(
                    worker
                        .eval_args(EvaluatedArgs::Bytes2(Some(actual_report.clone()), None))
                        .unwrap()
                ),
                None
            );
            assert_eq!(worker.kernel_invocations(), 1);
            assert!(worker.is_healthy());
            assert_eq!(worker.retained_storage().unwrap(), storage);
        }
    }
}
