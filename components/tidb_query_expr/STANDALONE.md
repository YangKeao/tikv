# Standalone RPN embedding PoC

`tidb_query_expr::standalone` embeds the **existing** TiKV expression engine in
process. It invokes `RpnExpressionBuilder::build_from_expr_tree` once, then
a checked entry to the original `RpnExpression::eval_decoded` loop for each
nonempty internal batch. It does not
reimplement any scalar functions, extract a second engine, or call a TiKV server.

## API

```rust,ignore
use tidb_query_expr::standalone::{Column, Context, PreparedExpression};

// Compile once. `PreparedExpression` is `Send + Sync` (asserted at compile
// time) and evaluation only needs `&self`, so one program can be shared across
// threads.
let program = PreparedExpression::compile(
    &serialized_tipb_expr,
    &serialized_tipb_field_types, // &[Vec<u8>], one FieldType per input column
    Context { flags: 1 << 5, ..Context::default() }, // IN_SELECT_STMT
)?;
let output = program.eval_shared(
    &[Column::Int(vec![Some(10), None, Some(30)])],
    3,                         // physical row count, also for zero-column batches
    Some(&[2, 0, 2]),           // optional selection, repeats allowed
)?;
```

`eval`, `eval_shared` and `eval_with_state` compute the same result. `eval`
keeps the original `&mut self` receiver for existing callers; it is a thin
wrapper over `eval_shared(&self, ..)`, which allocates fresh scratch per call.

## Shared compiled form and execution state

The facade separates the immutable compiled program from the mutable scratch a
single evaluation needs:

- **Compiled program** (`PreparedExpression`): the RPN nodes, metadata, schema,
  eval types, fixed `EvalConfig` and preflight guards. Compilation is the only
  place that builds metadata and decodes constants. There is no per-execution
  or thread-affine state, so it is `Send + Sync`; a compile-time
  `assert_impl_all!(PreparedExpression: Send, Sync)` fails the build if that
  ever regresses. It can live behind an `Arc` and be evaluated concurrently.
- **Execution state** (`ExecutionState`): mutable scratch owned per evaluator.
  It carries an `EvalContext` (the warning buffer) and a reusable row-selection
  buffer. `PreparedExpression::execution_state()` binds one to the program's
  fixed configuration; `eval_with_state(&self, &mut ExecutionState, ..)` resets
  the warning state at the start of every call so reuse never leaks warnings
  between batches. One state must not be used by two evaluations at once, but
  one program can serve any number of states (for example one per worker).

The RPN stack, the decoded input columns and the output column are allocated
inside each call because they borrow call-scoped inputs; they are not, and
cannot be, stored in the compiled program. This is why `eval_shared` allocates
scratch per call while `eval_with_state` only lets the caller amortize the
warning and selection buffers. The checked evaluator exposes
`eval_decoded_with_finite_reals_into`, which takes that call-scoped RPN stack
from the caller. The stock server path (`eval_decoded`) still allocates its own
stack and is behaviorally unchanged.

The borrowed API follows the same split: `eval_borrowed(&mut self, ..)` is kept
for existing callers and `eval_borrowed_shared(&self, ..)` evaluates a shared
program.

`Column` has nullable vectors for all nine usable engine types: `Int(i64)`,
`Real(f64)`, `Bytes(Vec<u8>)`, `Decimal(Decimal)`, `DateTime(DateTime)`,
`Duration(Duration)`, `Json(Json)`, `Enum(Enum)`, and `VectorFloat32(VectorFloat32)`.
Native value types are re-exported from `standalone`; no generated protobuf type
is exposed. Unsigned integers retain their `i64` bits; FieldType carries flags,
collation, temporal kind, precision and scale. Byte strings are not UTF-8 converted.
`Set` exists in EvalType but is not supported by the engine's FieldType conversion
or codecs, so it remains rejected (as do Geometry and unsupported SQL types).

**Breaking PoC change:** Decimal uses native values, not strings. This preserves
all decimal words, stored fraction and result fraction independently: e.g. division
can retain more digits than Display prints. Neither inputs nor outputs round-trip
through text or f64. Native inputs are assumed to represent the supplied SQL
schema; the facade does not apply SQL casts or enforce declared decimal scale.

Checked transport helpers use the existing engine codecs:

- `decimal_from_chunk(&[u8]) -> Result<Decimal, Error>` and
  `decimal_to_chunk(&Decimal) -> Result<Vec<u8>, Error>` preserve the native-endian
  40-byte TiDB/TiKV decimal layout (including both fraction fields). Decode checks
  header/word invariants and Rust bool validity **before** the engine's trusted
  unsafe chunk decoder. Never call that decoder directly on untrusted bytes.
  One representation exception is intentional: Go's zero-digit default zero
  becomes TiKV's one-integer-digit zero, preserving result scale. TiKV's native
  shift routine assumes at least one word and otherwise underflows. Zero-digit
  headers containing nonzero word storage are rejected; ordinary decimal headers
  and hidden scales are unchanged.
- `date_time_from_chunk` / `date_time_to_chunk` use the eight-byte little-endian
  native Time chunk. They preserve wall time, type and FSP without timezone
  conversion. This differs from serialized `MysqlTime` constants, whose packed
  timestamp decoding applies the compilation context timezone.
- `json_from_binary` / `json_to_binary` use a type byte plus the engine's binary
  payload. They preserve number tags, opaque data and temporal values. Structural
  validation bounds nesting at 64 and checks lengths, tags, offsets, sorted keys,
  nonoverlapping forward payloads, UTF-8 strings and finite numbers before native
  JSON access. Raw engine JSON constructors/decoders trust their inputs.

These are version-pinned embedding representations, not stable or portable ABIs.
All selected REAL values/constants must be finite (NotNan alone accepts infinity
and can panic on Inf*0). Selected native JSON/vector values are revalidated because
those types expose mutable payload bytes. DateTime components are range checked;
SQL zero dates and invalid calendar dates remain representable. Unselected values
are not converted or validated.

Finite inputs do **not** guarantee finite intermediates: vector distance between
`[3e38]` and `[-3e38]`, or `ROUND(f64::MAX,-308)`, can produce infinity. The
standalone-only checked evaluator rejects every nonfinite REAL produced by a node
before another kernel can consume it (e.g. multiply by zero would panic in NotNan).
It also rejects nonfinite root results. The ordinary engine `eval_decoded` path
and all kernels remain unchanged. These runtime errors must not trigger replay.

ROUND/TRUNCATE fractional-digit arguments have an additional pre-execution safety
contract: signed values must be within `[-308,308]`, unsigned values within
`[0,308]`; NULL is allowed. Constants are checked during compilation. A direct
integer input column is allowed and all selected digit values are checked across
the whole caller batch before any kernel runs. Derived digit expressions are
rejected at compilation: checking them would require speculative evaluation.
This covers RoundWithFracInt/Dec/Real and TruncateInt/Uint/Real/Decimal. Even finite
zero with a large positive exponent can otherwise panic on `0*Inf`; very negative
exponents can panic on `0/0`. Guarded digit columns remain ineligible for borrowed
evaluation. This is a conservative safety subset, not a new SQL implementation.

REAL FieldType flen/decimal must each be `-1` (unspecified) or `0..=254`, with
`flen >= decimal` when both are specified. These limits guard the trusted float
cast routine's assertions and unsigned subtraction; they describe facade safety,
not global MySQL metadata legality.

Schema and expression messages are serialized tipb wire bytes so a prost caller
does not share generated Rust types with the rust-protobuf engine. Each
expression ColumnRef node's FieldType must equal its schema FieldType, including
its protobuf presence bits. Callers should serialize both from the same field
metadata. The expression must supply its kind and FieldType. Compilation
validates exact function argument/return eval types and arity, column offsets,
constant payload shape, decimal metadata and a maximum depth of 64. Malformed
wire messages and unsupported expressions return errors before evaluation.

### Admitted functions

There is no facade scalar signature whitelist. `map_expr_node_to_rpn_func` and
its generated/handwritten validators, followed by the original metadata builders,
are authoritative. At baseline `521ac733` the mapper lists **510 signatures**:

| Kernel module | Signatures |
| --- | ---: |
| arithmetic | 25 |
| cast | 53 |
| compare | 83 |
| compare_in | 7 |
| control | 21 |
| encryption | 8 |
| json | 22 |
| vec | 7 |
| like | 1 |
| regexp | 6 |
| math | 47 |
| miscellaneous | 19 |
| op | 36 |
| other | 1 |
| string | 61 |
| time | 113 |

This is a mapper inventory, **not** proof every enum member or arbitrary metadata
shape is supported, nor proof of semantic parity with another SQL evaluator.
`scalar_function_signature(name)` resolves the engine enum to its wire number
without requiring a consumer's proto enum to be expanded. A returned number does
not establish support: compile the complete expression to check actual capability.
Normal constant kinds (including MysqlTime/Duration/Json/Enum/Bit and vector),
typed NULLs and column references use the existing builder.

The boundary supplements trusted-plan assumptions, rather than replacing kernel
validators: ToBinary/LIKE mapper arity guards, regexp raw-varg argument types,
column/schema equality, payload checks, enum index bounds and metadata ranges.
Cast InUnionMetadata remains in Expr.val; IN retains original hash extraction and
child mutation; regex constants are precompiled; date arithmetic and TimestampDiff
retain their constant-unit metadata requirements. Unknown signatures are errors.

**Evaluation remains TiKV eager RPN.** There are only Constant, ColumnRef and
FnCall nodes, not lazy branch nodes. All IF/IFNULL/CASE/COALESCE/AND/OR children
are evaluated before the parent kernel. An unreachable overflowing branch still
errors, and unreachable warning-producing/volatile expressions still run. Consumers
requiring lazy semantics must keep the whole unsafe tree in their native evaluator,
or establish safety before compilation; this facade neither rewrites kernels nor
silently retries after evaluation starts. Volatile functions such as RAND, UUID,
RandomBytes and SYSDATE preserve their existing TiKV behavior, not another engine's
statement/session state. Constant metadata errors (e.g. invalid regex) occur during
compilation even for empty input or otherwise unreachable branches.

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

## Borrowed packed-input API

The copying `eval` call shape remains unchanged (its Column type surface expanded). The optional
`supports_borrowed()` / `eval_borrowed` path removes **bulk input payload
materialization** and the owned facade output column. It is not end-to-end
zero-copy: scalar values must be loaded, kernel results/intermediates are still
owned `VectorValue`s, and the caller writes results to its output storage.

```rust,ignore
use tidb_query_expr::standalone::{ColumnRef, ScalarRef};

assert!(program.supports_borrowed());
let inputs = [ColumnRef::Int {
    values: &packed_native_endian_bytes,
    validity: &valid_bits, // LSB-first, 1 = non-NULL
}];
let diagnostics = program.eval_borrowed(&inputs, physical_rows, selection, |value| {
    match value {
        ScalarRef::Null => output.append_null(),
        ScalarRef::Int(value) => output.append_int(value),
        ScalarRef::Real(value) => output.append_real(value),
        ScalarRef::Bytes(value) => output.append_bytes(value),
    }
    Ok(())
})?;
```

`ColumnRef::Int` and `Real` borrow native-endian packed 8-byte values and a
validity bitmap. `Bytes` additionally borrows `offsets: &[i64]`; payloads retain
their byte identity (including invalid UTF-8/NUL). Unaligned numeric buffers are
supported with `from_ne_bytes`, without unsafe casts or materializing a numeric
column. Length/offset/selection checks and a scan of **all selected REAL values**
happen before any kernel or sink call. Byte offsets must contain rows+1
nonnegative, nondecreasing in-bounds positions. Empty selections invoke no sink.
Selection, offsets and validity stay borrowed; rows are not densely gathered.

`Diagnostics` contains retained warnings and total warning count. The sink is
higher-ranked over its temporary `ScalarRef` lifetime: it cannot keep a borrowed
result reference after returning. The evaluator retains no input references
between calls. Callers holding backing-store read guards must keep them alive
for the whole call and avoid attempting writes to aliased locked storage in the
sink (detach output or choose the copying path before acquiring guards).

A runtime or sink error can occur **after callbacks from earlier internal
batches**. The caller must discard/reset its partial output, not replay the
expression natively. Warnings on a failed call are discarded, matching the
copying facade's failure contract.

### Kernel reuse and limits

The same compiled `RpnExpressionNode` sequence and selected kernel specialization
are traversed in the same expression-major order. A separate borrowed stack
holds input views, references to constants, and original owned generated
vectors. Only explicitly marked `#[rpn_fn(borrowed)]` ordinary scalar kernels
have an optional generated loader. That loader creates stack-local primitive
holders / borrowed byte arguments and calls the **original scalar function**;
there is no handwritten SQL-arithmetic dispatcher and no kernel body change.
The original `fn_ptr`, `ArgConstructor`, and `eval_decoded` paths are unchanged.

Currently opted in: arithmetic/arithmetic_with_ctx, numeric comparisons,
integer/real ABS (including unsigned ABS), and byte LENGTH. Removing the copying
API's whitelist does not opt additional kernels into borrowed loaders. Decimal/other input or intermediate types,
varargs, writers, metadata-based/custom evaluators and unmarked functions are
not admitted to this path. An explicit macro opt-in must not be applied to a
kernel whose scalar body differs semantically from its specialized evaluator.
`supports_borrowed()` declines unsupported programs before execution.

Tests cover exact input-pointer preservation for byte column output, unaligned
numeric input, NULLs and duplicate/reordered selections, large split batches,
constant broadcast, full preflight before sinks, invalid metadata, eager nested
overflow under NULL parents, warning caps, sink cancellation, and partial output
on late runtime failure. Differential checks use the unchanged copying API.

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
cargo +nightly-2026-08-22 test -p tidb_query_expr --lib standalone -j1 -- --test-threads=1
```

These flags are native dependency compatibility workarounds, not kernel or
expression behavior changes. A normal older supported TiKV build environment
may not need them. The standalone tests cover actual RPN nullable numeric
operations, selection normalization, split/empty batches, bytes and decimal
conversions, MySQL errors, bounded warnings, and malformed/unsupported input.
Coverage tests also exercise all nine type identities/NULLs/split selections,
every mapped kernel family, exact hidden decimal scale, temporal/binary JSON
transport, eager unreachable-branch errors and compile-time regex errors. Bounded
native red witnesses demonstrate mapper/JSON panics and missing regexp validator
type checks while the public boundary rejects the same requests. The malformed
bool decimal test only invokes checked ingress, never the unsafe native decoder.

On shared development hosts use one heavy command globally, `-j1`, serial tests,
and an external process-tree RSS/address-space/host-reserve guard. The development
run used `limited-run.py --rss-mib 6144 --as-mib 8192 --min-available-mib 8192`.
