# Standalone RPN embedding PoC

`tidb_query_expr::standalone` embeds the **existing** TiKV expression engine in
process. It invokes `RpnExpressionBuilder::build_from_expr_tree` once, then
`RpnExpression::eval_decoded` for each nonempty internal batch. It does not
reimplement any scalar functions, extract a second engine, or call a TiKV server.

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
`eval`. It is not a borrowed evaluator. Results and diagnostics are owned handles
with bulk views and matching free functions. Decimal is not admitted by that ABI.
The ABI documentation specifies pointer obligations, panic containment, ownership,
and native symbol/dependency audits needed before loading alongside TiFlash.

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
