# TiKV expression C ABIs (experimental, Linux ELF)

This crate adds **C ABIs, not SQL kernels**, around the original
[`PreparedExpression`](../tidb_query_expr/src/standalone.rs). The original copying
ABI v1 remains unchanged and available. An additive, separately bootstrapped
borrowed ABI lends native TiFlash inputs to original kernel loaders and writes
numeric results directly to caller storage. Neither path starts a server,
performs RPC, or embeds another process. This is an opt-in PoC, not a supported
general SQL or cross-version ABI. TiFlash integration belongs to its own tree.

## Original copying interface and ownership

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

## Additive native borrowed ABI

Resolve and call `tikv_expr_borrowed_abi_version` **before passing any new
struct**. Only version 1 is supported. After normal copying-ABI compilation,
`tikv_expr_program_supports_borrowed` checks the actual original generated kernel
loaders and numeric output admission; zero means choose fallback before eval.
Programs and poison state are shared by both eval entry points.

`tikv_expr_borrowed_column` lends native aligned i64/f64 arrays; no packed numeric
copy is made. Optional native byte NULL maps accept any nonzero byte, including
255. Native ColumnString lends chars and u64 END offsets directly: offsets are
strictly increasing, each row includes its terminal NUL, final offset equals
chars length. The borrowed SQL value excludes only that terminal byte. Empty
strings/embedded NULs are preserved, with no per-row slice array or payload
packing. NULL/unselected row terminators are validated too. BROADCAST descriptors
store exactly one row even when logical row_count=0; repeated/selected logical
indices resolve directly to that row without expansion.

`tikv_expr_borrowed_output` points to caller-owned numeric and NULL-map arrays,
with capacities measured in logical output rows, not physical stored input rows.
Int64 and Float64 output match compiled types. UInt8 accepts only Int 0/1;
UInt64 accepts only nonnegative Int (for native ABS storage). Conversion failures
are runtime failures: partial output is discarded, never replayed. NULL numeric
slots receive zero, null bytes are canonical 0/1. The map is required even for
nonnullable output, so an O(rows) caller scratch map remains. Byte-output programs
are rejected before kernels; LENGTH remains supported because its output is Int.

Foreign payloads are never retained or modified. There is no owned facade input
or output vector; original kernels still own intermediate and per-batch output
`VectorValue`s, and Rust allocates descriptors, stack state and diagnostics. This
is borrowed input/direct sink, not a zero-allocation engine or SIMD rewrite.
Packed/bitmap borrowed Rust API compatibility is preserved separately.

Every shape/selection/output capacity/alignment/size/address/overlap check and
selected REAL domain check runs before the first payload write or kernel.
Output ranges (full declared capacities) cannot overlap inputs, descriptors,
selection or each other. Handle and handle-slot alias/lifetime obligations remain
the caller's responsibility. Handle slots initialize separately before validation;
no-write guarantees concern output payloads. Allocation validity cannot be proved
by pointer arithmetic. On later engine/sink failure all partial output must be
discarded. C++ exceptions never cross this interface (there are no callbacks).

Successful borrowed evaluation returns an independent owned diagnostics handle,
not an owned result column. Its bulk view reuses warning descriptors/counts and
expires at `tikv_expr_diagnostics_free`. Errors use the original owned error API.
Warnings preserve native codes/caps and fresh call state. Rust unwind containment
poisons the same program for both copying and borrowed APIs; aborted/invalid
memory operations remain unrecoverable. Existing copying status/layout/export
contracts are not changed.

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

- inspect `nm -D --defined-only`/`readelf --dyn-syms`: only the fourteen `tikv_expr_*`
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
checks the exact fourteen-symbol export set (nine copying plus five borrowed), direct shared dependencies and known
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
it checks public layouts and all fourteen exported entry points' basic ownership/error
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
`cargo clippy --locked -p tidb_query_codegen -p tidb_query_expr -p tidb_query_expr_ffi --all-targets --no-deps -j1`, with
**all exact `CLIPPY_LINTS` flags from `scripts/clippy`**, root `clippy.toml` (set
`CLIPPY_CONF_DIR` to the workspace root), the pinned toolchain/environment and the
same coordinated memory guard. Keep this crate's default vendored-OpenSSL
feature. This is not a claim that full `make clippy`, cargo-deny or CI passed.

To regress the original copying expression suite while preserving the same
vendored native feature graph, use the guarded command:

```sh
cargo test --locked -p tidb_query_expr -p tidb_query_expr_ffi --lib -j1 -- --test-threads=1
```

Always invoke Cargo from this `tikv-tiflash` worktree with its dedicated target
directory. Seeded target caches may contain executables from other branches;
never run those cached executables directly and mistake their results for
current-source coverage.

### Borrowed branch validation

The parent-approved, locked single-job regression command selecting
`tidb_query_codegen`, `tidb_query_expr`, and `tidb_query_expr_ffi` passed 21, 437,
and 24 tests respectively (482 total), including a final post-lint-fix rerun.
This preserves copying and packed/bitmap coverage plus eight native-layout tests
and eleven new C ABI tests. The corrected session-tracking guard sampled peak
session RSS 1026.4 MiB on the final run (initial run 1503.6 MiB), within 6144 MiB
RSS / 8192 MiB per-process AS limits and 8192 MiB host reserve. Log:
`expression-reuse/logs/tikv-tiflash-borrowed-tests-final.log`. Upstream unused feature,
deprecated constant and nom/procinfo future-incompatibility notices remain.
No full TiKV server build or TiFlash executable parity is implied by Rust tests.

The final isolated-target optimized cdylib build passed (sampled session peak
1662.9 MiB; initial build 1666.0 MiB). ELF audit found exactly fourteen exports and no detected private native
imports/dependencies; direct and local transitive loader lists contained only
loader/libc/libm/libgcc_s/libstdc++. C11 and C++17 header/link smokes passed with
`-Wall -Wextra -Werror`. The focused **all-three-crate** all-targets Clippy
exception above passed with every repository lint flag and no owned-crate warning
(869.7 MiB sampled session peak; existing nom future notice only). Closing this
coverage gap required test-only baseline hygiene in `standalone/tests.rs`:
14 Result-state assertions became `unwrap`/`unwrap_err`/`expect_err`, and seven
single-schema clones became `slice::from_ref`, preserving all cases and pass/fail
semantics. Equivalent borrowed/codegen test fixes and overflow-safe bitmap
`div_ceil(8)` remove newly introduced/ported lint findings. The first all-crate
failure log is retained; no lint was suppressed. Logs share prefix
`expression-reuse/logs/tikv-tiflash-borrowed-` with `release-final.log`,
`audit-smoke-final.log`, and `clippy-all-final.log`. The narrow audit/smoke guard was 1024 MiB
RSS/AS with the same 8192 MiB host reserve; its 0.8 MiB final sampled peak (1.1
MiB initial) is not a claim to have captured brief compiler peaks.

New release SHA256:
`e1f527637011a8c22a490e90042d2b2a0c4669195eb1109f2128f19bb22521cb`.
The original copying release SHA256 remained unchanged before/after validation:
`24ff6c1e1e718ac39deee00fb1119d293b50c23df614753cad6feb4a547f032d`.
These are local build/link checks, not deployment-baseline compatibility or real
TiFlash coexistence guarantees. Full `make clippy` was not run.

### Historical copying-only validation (not borrowed validation)

The following observations apply to the prior copying-only commit `da38d16`,
NOT the new borrowed source/artifact. Borrowed builds must use a separate
`CARGO_TARGET_DIR=/home/agent/tidb/expression-reuse/target-tikv-tiflash-borrowed`
and never overwrite the copying reports' release library. The current parent
coordinates every build through the session-tracking guard at
`/home/agent/tidb/expression-reuse/tiflash/programs/tikv-expression-poc/limited-run.py`;
older process-group-only guard reports do not establish descendant-session bounds.
No validation of the new source is claimed by these historical results.

Using nightly-2026-08-22, the then-current guard and `-j1`:

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
