# Copying TiKV expression C ABI (experimental, Linux ELF)

This crate adds a **C ABI, not an evaluator**, around the original
[`PreparedExpression`](../tidb_query_expr/src/standalone.rs). It does not modify
that facade, borrow engine inputs, implement SQL kernels, start a server, perform
RPC, or embed a second process. This is an opt-in PoC, not a supported general SQL
or cross-version ABI. TiFlash integration belongs to the separate TiFlash tree.

## Interface and ownership

[`include/tikv_expr.h`](include/tikv_expr.h) is the canonical ABI v1 contract.
Include it from C or C++; only native-endian C scalars, pointers, counts and opaque
handles cross the boundary. `size_t` and architecture must match. Compile once
using serialized tipb `Expr`/`FieldType` messages, and reuse the program exclusively
(no concurrent calls on one program). Context is fixed at compile time:

- flags and SQL mode are passed unchanged; the original facade truncates unknown
  bits like TiKV DAG requests;
- nonempty UTF-8 timezone name takes precedence over offset seconds east of UTC;
- division precision increment is 0..30; warning detail cap is 0..65535;
- obtain defaults with `tikv_expr_context_default` (flags/mode/offset 0, precision
  4, warning cap 64).

ABI types are Int64, Float64 and arbitrary binary Bytes, each nullable. Unsigned
integers retain their i64 bit pattern; unsignedness/collation and protobuf presence
bits remain in the supplied metadata. The ABI rejects all other field types at
**every node**, including decimal intermediates. The unchanged facade owns all
signature, arity, metadata consistency and SQL semantic validation; see its
[`STANDALONE.md`](../tidb_query_expr/STANDALONE.md) for the restricted function set.

Input buffers are call-scoped. Int64/Float64 columns point to value arrays. Bytes
columns point to a `tikv_expr_bytes` slice descriptor per row: bytes exclude any
storage terminator, but may contain embedded zero/non-UTF-8 data. TiFlash can use
`ColumnString::getDataAt` rather than repacking a payload to remove terminators.
The optional NULL map uses one byte per row, 0=valid, 1=NULL. Inactive typed pointer
fields must be NULL. Every column descriptor describes the full physical batch.

A NULL selection descriptor means all physical rows; a live descriptor with
`{NULL,0}` means **no rows**. Repeated/reordered indices are legal. Before calling
unchanged `PreparedExpression::eval`, the ABI:

1. validates descriptor sizes, full physical lengths/types, NULL-map bytes,
   selection bounds, numeric/descriptor alignment and checked address arithmetic;
2. validates the slice shape of every non-NULL string row, without reading
   unselected payloads;
3. gathers only selected values into **original owned `Column` vectors**, copying
   each selected non-NULL byte payload (including repeated rows);
4. invokes original `eval(&owned, logical_row_count, None)`.

The original facade then copies into engine vectors in batches of at most 1024
and copies the evaluated output back. The ABI never lends foreign payloads to
`eval_decoded`. Unselected nonfinite floating-point values remain ignored, whereas
selected values are rejected by the facade. Constants broadcast even for
zero-column batches. Empty selections still validate input descriptor shapes but
never evaluate values.

`tikv_expr_result_get_view` returns the entire dense column and warnings in one
call. Bytes descriptors refer to the original owned output byte vectors; numeric
outputs have contiguous exported arrays and an explicit NULL map. All view
pointers are immutable and valid until `tikv_expr_result_free`, independently of
input buffers, subsequent eval calls, and the program lifetime. Do not mutate a
result or free it while another thread uses its view. Every allocation must be
freed by the same loaded library's matching `*_free`; all frees accept NULL. Keep
the library loaded until all calls and handles have finished.

## Failure contract and safety limits

compile/eval return status 0 on success; nonzero status distinguishes invalid ABI
arguments, compile/admission error, evaluation error, Rust panic, and a program
poisoned by an earlier evaluation panic. Required output handle slots are reset
to NULL before work (assuming valid output slots). `out_error` is optional; when
supplied, it receives an independently owned error with status, native MySQL code,
and a length-delimited UTF-8 message. ABI/panic failures have MySQL code 0.
Facade/native failures preserve their original code/message. Free existing
handles before reusing output slots; the API never implicitly frees old pointers.
No partial output or warning details survive an evaluation error.

Warnings expose both retained details and total count (which may be larger than
the cap), preserving engine order and native codes/messages. Warning state is
fresh per call, with one cap across internal batches. Caller owns statement-level
aggregation. No TLS last-error state, global runtime, or process-wide panic hook
is installed. Embedding caller owns cancellation, scheduling and memory limits.

`catch_unwind` prevents **Rust unwinding** from crossing C and poisons an affected
program; recompile before using it again. Build with `panic=unwind` (workspace dev
and release already do). Panic payloads are deliberately forgotten to avoid a
second panic from their destructor. Rust's existing process panic hook may still
print diagnostics; this library does not replace it. Allocator OOM, `abort`,
signals and invalid memory access are not recoverable errors.

Pointer checks only reject structural mistakes: NULL with nonzero required
length, misalignment, length multiplication beyond `isize::MAX`, and wrapping
address additions. They **cannot establish allocation validity/provenance**.
The caller must provide live initialized accessible storage for full descriptors
and arrays, immutable throughout the call, correct handle types, and disjoint
writable output slots. Zero-length buffers accept NULL and are never read. For
NULL rows, string payload descriptors are ignored; for unselected non-NULL rows,
payload contents are not read. Dangling/forged pointers, wrong or already-freed
handles, races and insufficient allocations are UB, not safely catchable errors.
The ABI does not provide an allocation quota; logically huge but structurally
valid batches can exhaust memory. Never silently retry in another evaluator after
evaluation starts; compile-time admission is the only permitted fallback boundary.

## Linking: prefer a private cdylib over staticlib

The facade transitively builds TiKV native gRPC, OpenSSL and C++ utilities. A
staticlib would inject them into TiFlash's link namespace alongside TiFlash gRPC,
BoringSSL and libc++, creating symbol/ABI hazards even though evaluation uses no
network. This crate therefore builds `libtidb_query_expr_ffi.so` plus an rlib for
Rust tests; there is deliberately no staticlib target.

For Linux ELF, rustc's cdylib export list plus `--exclude-libs,ALL` localizes native
archive definitions. Default `vendored-openssl` enables **both**
`openssl/vendored` and `grpcio/openssl-vendored`, so legacy grpcio-sys discovers the
same private static OpenSSL archive. Do not set `OPENSSL_NO_VENDOR`, force dynamic
OpenSSL, use `--no-default-features`, or treat an unaudited alternative build as an
isolated artifact. Vendoring increases initial compilation cost.

**Export hiding alone is not proof of isolation.** Before integrating each build:

- inspect `nm -D --defined-only`/`readelf --dyn-syms`: only the nine `tikv_expr_*`
  API functions may be defined exported symbols (allow ELF version markers only
  after explicit review); in particular no gRPC/OpenSSL/abseil/C++ exports;
- inspect `readelf -d`: no `libssl`, `libcrypto`, gRPC, protobuf or abseil shared
  dependency; audit transitive `DT_NEEDED` as well;
- inspect undefined dynamic symbols for OpenSSL/gRPC/protobuf/abseil imports that
  could bind TiFlash's global namespace, including symbols exported by the host;
- system libc/libm/libgcc and a private dependency on the standard libstdc++
  runtime may remain. libc++ and libstdc++ may coexist only because no C++ objects,
  allocators, RTTI, exceptions or STL containers cross the C boundary. Shared
  runtime dependencies are not isolated merely by hiding exports;
- confirm the real TiFlash executable loads and runs the audited artifact. If
  loading dynamically, use normal `RTLD_NOW | RTLD_LOCAL`, not `RTLD_DEEPBIND`.
  Do not introduce broad `-Bsymbolic` without dependency-specific review.

`python3 components/tidb_query_expr_ffi/audit_elf.py /path/to/libtidb_query_expr_ffi.so`
checks the exact nine-symbol export set, direct shared dependencies and known
native import families, prints all undefined imports for review, and enforces its
own <=1 GiB AS limit. It fails closed on unexpected exports/direct dependencies.
It does not certify transitive runtime isolation or replace the real TiFlash
coexistence test.

The library uses ordinary host allocator/runtime facilities. It is symbol
encapsulation, not a security sandbox or separate dynamic-loader namespace.
Non-Linux targets currently fail the build script rather than pretending these
ELF guarantees apply elsewhere.

## Build and test (parent-coordinated heavy slot only)

Do **not** run these concurrently with another repository's heavy build. The
session requires one heavy slot, `-j1`, aggregate RSS <=6 GiB, process AS <=8 GiB,
and at least 8 GiB host memory reserved. A parent-supplied resource guard must wrap
every cargo/native build; `-j1`/ulimit alone do not enforce aggregate RSS/reserve.
For lightweight Python/helper commands use AS <=1 GiB. Prefer small benchmarks.

Once the parent grants the slot and supplies its guard, the inner commands are:

```sh
export CMAKE_BUILD_PARALLEL_LEVEL=1
export CMAKE_POLICY_VERSION_MINIMUM=3.5
export CXXFLAGS='-include cstdint -std=c++17'
# Run each under the parent's RSS/AS/reserve guard, never concurrently:
cargo +nightly-2026-08-22 test -p tidb_query_expr_ffi --lib -j1
cargo +nightly-2026-08-22 build -p tidb_query_expr_ffi -j1
```

Use the existing pinned workspace lockfile/patches. Cargo.lock adds only this
workspace member's package entry; no dependency version updates are required.
Do not update unrelated dependencies. Header parsing and Rust formatting can be
checked separately within the lightweight limit after coordinating native
compiler use with the parent.

`tests/header_smoke.c` can be compiled as C11 and C++17 against the built cdylib;
it checks public layouts and all nine exported entry points' basic ownership/error
contracts. Keep assertions enabled. This smoke test is not a replacement for the
embedding TiFlash executable's real expression/parity and coexistence tests.

Rust tests cover ABI layouts/defaults, original-facade parity, selected/repeated rows,
NULL and embedded-NUL/non-UTF-8 byte slices, output lifetimes, empty-vs-none
selection, zero-column broadcasts, internal split batches, selected/nonselected
nonfinite reals, native errors, bounded warnings, malformed metadata and context,
structural pointer validation, panic containment and poisoned handles. They do
not dereference invalid pointers to simulate catchable access violations.

### Scoped lint exception and regression selection

Repository guidance normally requires `make clippy`. It is not package-scopable:
that target runs global checks/tool installations, `scripts/clippy-all` drops
arguments and adds server test features, and `scripts/clippy` hardcodes
`--workspace --no-default-features`. Adding `-p` does not narrow a workspace-wide
selection. For this resource-bounded PoC, the parent approved a focused exception:
`cargo clippy --locked -p tidb_query_expr_ffi --all-targets --no-deps -j1`, with
**all exact `CLIPPY_LINTS` flags from `scripts/clippy`**, root `clippy.toml` (set
`CLIPPY_CONF_DIR` to the workspace root), the pinned toolchain/environment and the
same coordinated memory guard. Keep this crate's default vendored-OpenSSL
feature. This is not a claim that full `make clippy`, cargo-deny or CI passed.

To regress the original copying expression suite while preserving the same
vendored native feature graph, use the guarded command:

```sh
cargo test --locked -p tidb_query_expr -p tidb_query_expr_ffi --lib -j1 -- --test-threads=1
```

Always invoke Cargo from this copying worktree. Shared target directories may
contain old borrowed-branch test executables; never run those cached executables
directly and mistake their results for copying-source coverage.

### Observed validation on the development host

Using nightly-2026-08-22, the coordinated guard and `-j1`:

- all 420 original copying expression tests and 13 ABI tests passed together
  from this worktree (final cached build/test peak group RSS 760.3 MiB);
- focused Clippy with the exact repository lint policy passed with no owned-crate
  warnings (final cached peak group RSS 461.9 MiB; an existing `nom` dependency
  future-incompatibility notice remains); full `make clippy` was not run;
- optimized cdylib rebuild passed (sampled cached peak group RSS 342.4 MiB;
  initial full release build 2512.4 MiB);
- C11 and C++17 header/link smoke tests passed against the optimized cdylib;
- ELF audit found exactly the nine API exports, no detected private native
  imports, and only loader/libc/libm/libgcc_s/libstdc++ direct dependencies;
- the local transitive loader report likewise listed only those system runtimes.

These are local artifact checks, **not yet TiFlash executable coexistence/parity
validation**. The development artifact imports GLIBC_2.38 symbols and uses the
host GCC 16 libstdc++; rebuild and re-audit on the intended deployment baseline
rather than assuming compatibility with older hosts. Test/build logs are external
to the source tree under the coordinated session's `expression-reuse/logs/`:
`tikv-copying-ffi-regression-clean.log`, `tikv-ffi-clippy-clean.log`,
`tikv-ffi-release-lint-final.log`, `tikv-ffi-elf-audit-lint-final.log`,
`tikv-ffi-header-smoke.log`, and `tikv-ffi-runtime-deps.log`. Earlier
`tikv-ffi-tests*.log` and `tikv-ffi-release*.log` preserve intermediate/full runs.
