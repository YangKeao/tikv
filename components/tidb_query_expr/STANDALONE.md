# Standalone RPN embedding PoC

`tidb_query_expr::standalone` embeds the **existing** TiKV expression engine in
process. It invokes `RpnExpressionBuilder::build_from_expr_tree` once; the original
copying API invokes `RpnExpression::eval_decoded` for each nonempty internal batch.
The additive borrowed path below reuses the original scalar kernels. Neither path
reimplements scalar functions, extracts a second engine, or calls a TiKV server.

## API

```rust,ignore
use tidb_query_expr::standalone::{Column, Context, PreparedExpression};

let mut program = PreparedExpression::compile(
    &serialized_tipb_expr,
    &serialized_tipb_field_types, // &[Vec<u8>], one FieldType per input column
    Context { flags: 1 << 5, ..Context::default() }, // IN_SELECT_STMT
)?;
let output = program.eval(
    &[Column::Int(vec![Some(10), None, Some(30)])],
    3,                         // physical row count, also for zero-column batches
    Some(&[2, 0, 2]),           // optional selection, repeats allowed
)?;
```

`Column` contains nullable `Int(i64)`, `Real(f64)`, `Bytes(Vec<u8>)` or
`Decimal(String)` vectors. Unsigned integers use their `i64` bit representation;
FieldType metadata carries unsignedness. Decimal strings are parsed with TiKV's
existing decimal codec and results are formatted by that codec. Byte strings
are not converted through UTF-8. The decimal text boundary does **not** preserve
hidden storage-scale/result-fraction state from another engine's decimal
representation, nor enforce schema precision on input values. Callers must
restrict admission or normalize/test scale semantics; this is not a general
cross-engine decimal compatibility guarantee. All nonfinite real values (NaN,
positive infinity and negative infinity) are rejected for selected column values
and serialized constants before entering kernels. `Real::new` alone is not a
sufficient guard: it accepts infinity, but arithmetic such as `Inf * 0` or
`Inf - Inf` can panic in the underlying NotNan operators. Unselected nonfinite
column values are not converted and therefore remain permitted. Temporal, JSON,
enum, set and vector types are not admitted, including intermediate expression
types.

Schema and expression messages are serialized tipb wire bytes so a prost caller
does not share generated Rust types with the rust-protobuf engine. Each
ColumnRef's FieldType must equal its corresponding schema FieldType, including
its protobuf presence bits. Callers should serialize both from the same field
metadata. The expression must supply its kind and FieldType. Compilation
validates exact function argument/return eval types and arity, column offsets,
constant payload shape, decimal metadata and a maximum depth of 64. Malformed
wire messages and unsupported expressions return errors before evaluation.

### Admitted functions

The source's `validate_signature` is the authoritative admission list:

- Int: Plus, Minus, Multiply, Mod, IntDivide, comparisons including NullEq, Abs,
  unsigned Abs, unary minus/not, IsNull.
- Real: Plus, Minus, Multiply, Divide, Mod, comparisons including NullEq, Abs,
  unary minus, IsNull.
- Decimal: Plus, Minus, Multiply, Divide, Mod, comparisons including NullEq,
  Abs, unary minus, IsNull.
- Bytes: Length, BitLength, Ascii, IsNull, Concat.
- Typed NULL, integer/unsigned integer, real, bytes/string and decimal constants;
  column references.

This is deliberately smaller than the engine's own supported function set.
Admission is not proof of parity with another evaluator's SQL semantics. In
particular, an embedding caller controls whether to allow unsigned operations
or context-sensitive arithmetic in its own opt-in SQL dispatch policy.

### Context and results

`Context` is public, cloneable and equality-comparable. Defaults match TiKV:
flags=0, sql_mode=0, timezone name=None, offset=0 seconds, division precision
increment=4, maximum retained warnings=64. A nonempty timezone name takes
precedence over the offset. Unknown flags/SQL-mode bits are truncated exactly as
in TiKV DAG requests. The precision increment must be 0..=30 and warning limit
0..=65535. The context is fixed at compilation; recompile if it changes.

Each `eval` has fresh warning state. `EvalOutput` contains `column`,
`warnings: Vec<Warning { code: i32, message: String }>`, and `warning_count:
usize`. The count includes occurrences whose detail was dropped by the limit;
the limit applies across all internal batches, not per batch. Statement-level
warning accumulation across calls belongs to the caller. Current admitted
constants decode without warnings; a compile-time decoding warning is rejected
rather than silently dropped.

Evaluation validates column count, physical row counts, column variants and
selection bounds before entering the engine. It copies selected values to dense
TiKV columns, splits at `BATCH_MAX_SIZE` (1024), and copies results into one dense
owned column in selection order. Constants are broadcast. Empty batches or
selections return an empty typed column without invoking the evaluator. Values
in unselected rows are not converted. On error partial results and warnings are
discarded; native engine MySQL error code/message are preserved in `Error`.
Facade admission/shape errors use code 1105; existing builder/evaluator errors
retain their original code (including generic engine code 10000). A caller may choose native fallback on a
compile/admission error, but must not silently fall back after evaluation starts.

## Experimental C/C++ embedding

[`tidb_query_expr_ffi`](../tidb_query_expr_ffi/README.md) provides a Linux cdylib
C ABI for Int64/Float64/Bytes+NULL. Its header accepts call-scoped foreign arrays
and per-row byte slices, validates their shape, and gathers selected rows into
these original **owned** `Column` vectors before invoking unchanged `compile` /
`eval`. This original C entry point is not a borrowed evaluator. Results and diagnostics are owned handles
with bulk views and matching free functions. Decimal is not admitted by that ABI.
The ABI documentation specifies pointer obligations, panic containment, ownership,
and native symbol/dependency audits needed before loading alongside TiFlash.

## Additive borrowed-input path

The source of the packed borrowed evaluator is commit `521ac733` from the
separate borrowed PoC branch, ported without replacing the copying API above.
`PreparedExpression::supports_borrowed()` admits only explicitly opted-in original
kernels and Int/Real/Bytes types. `eval_borrowed(columns, row_count, selection,
sink)` returns owned `Diagnostics`; its `ScalarRef` callback values borrow only
for that callback. References are never retained by the program.

Existing `ColumnRef::Int/Real` use potentially unaligned native-endian packed
bytes plus LSB-first validity bitmap (1=valid). Existing `ColumnRef::Bytes` uses
packed payload plus signed start offsets (`rows+1`) and the same bitmap. These
variants and their validation/tests remain compatible.

Additional `NativeInt`, `NativeReal`, and `NativeBytes` variants consume native
numeric slices, optional byte NULL maps (ANY nonzero=NULL), and ColumnString
chars plus u64 END offsets. End offsets include exactly one terminal NUL per row;
only that final byte is removed from the borrowed slice, so embedded NUL and
empty strings work without packing. Native offsets must be strictly increasing,
in bounds, and end at chars length; every row's terminator is checked, including
NULL/unselected rows. Native `broadcast=true` requires exactly one stored row,
even with zero logical rows, and indexes it without expansion. Otherwise stored
count equals physical row count. Selection remains indexed against logical batch
rows, permits repeats, and distinguishes None from an explicit empty slice.

All shapes, selection indices and selected non-NULL REAL domains are checked
before any kernel/sink callback. The original compiled RPN ordering and kernel
specializations are reused: generated `#[rpn_fn(borrowed)]` loaders make scalar
primitive temporaries or borrowed byte arguments and call the original scalar
function. Kernel bodies and original `eval_decoded`/`fn_ptr` remain unchanged.
Original `VectorValue` intermediates/output batches remain Rust-owned; there is
no owned facade input/output column. This is not fully zero-allocation execution.
Opted-in kernels are arithmetic, numeric comparisons, Int/Real ABS and LENGTH;
unmarked/writer/vararg/custom-metadata kernels remain copying-only.

A later kernel or sink error can follow writes from earlier internal batches.
Callers must discard all partial output and never silently replay. Empty output
validates layouts but invokes neither kernels nor sink. The safe Rust API still
supports borrowed byte output; the additive C borrowed API intentionally admits
only numeric output (including LENGTH) into caller-owned buffers. See the C
header's separate version bootstrap, native layouts, checked UInt8/UInt64 sinks,
NULL scratch map, owned diagnostics/errors, and shared-program poison contract.

## Build and dependency contract

The PoC uses the existing whole expression/datatype crates and their transitive
utilities. It therefore still builds protobuf and native gRPC/OpenSSL/zlib
libraries even though evaluation performs no network IO. This is not a slim
standalone distribution or a stable ABI. Nightly Rust, a C/C++ compiler, CMake,
OpenSSL development headers and usual TiKV build prerequisites are needed.

For a consumer outside this workspace:

1. Depend on `tidb_query_expr` by development path or by a pinned revision of
   the owning fork. Do not rely on an unpinned git branch.
2. Maintain the consumer's own Cargo.lock. Dependency workspace lockfiles and
   `[patch]` sections are **not inherited**. Apply compatible root
   rust-protobuf/protobuf-codegen and raft/raft-proto patches as needed by the
   existing TiKV dependencies.
3. The workspace tipb and kvproto dependencies are revision-pinned to the
   engine's baseline lockfile versions so fresh external resolution cannot
   unexpectedly select an incompatible gRPC generation.

The PoC was developed with nightly-2026-08-22. On the development host's CMake 4
and GCC 16, legacy gRPC/abseil dependencies require these environment flags:

```sh
export CMAKE_POLICY_VERSION_MINIMUM=3.5
export CXXFLAGS='-include cstdint -std=c++17'
cargo +nightly-2026-08-22 test -p tidb_query_expr --lib standalone::tests -j 4
```

These flags are native dependency compatibility workarounds, not kernel or
expression behavior changes. A normal older supported TiKV build environment
may not need them. The standalone tests cover actual RPN nullable numeric
operations, selection normalization, split/empty batches, bytes and decimal
conversions, MySQL errors, bounded warnings, and malformed/unsupported input.
