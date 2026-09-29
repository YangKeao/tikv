# `src/coprocessor` Maintenance Guide

## Purpose And Scope

This module implements the classic TiDB coprocessor path for built-in request
types:

- DAG requests
- analyze requests
- checksum requests

It is a read-heavy hot path and directly impacts query latency.

## Architectural Views

### Request pipeline view

- parse protobuf request
- build request context
- get snapshot
- build request handler
- execute in read pool
- collect stats and emit response

### Batched unary result merging

- Unary handlers return `Result<HandlerOutput>`. Response data is either ready
  or kept as an unserialized `MergeableResult`. Currently only full-sampling
  analyze produces mergeable results. All mergeable outputs of one request must
  have the same concrete type and must produce the same logical result
  regardless of merge order.
- Merging is enabled only when the client sets
  `Request::allow_batch_task_data_merge` and supplies batched tasks. Otherwise,
  every task is serialized in its own read-pool task, preserving the existing
  wire behavior.
- Execution scheduling is negotiated independently. When the client sets
  `Request::execute_batch_tasks_serially`, the top task and batched tasks are
  polled one at a time so batching does not increase scan concurrency. This
  relies on the `ReadPoolHandle::spawn` contract: on both backends a task is
  admitted and enqueued only when the returned future is first polled. Because
  neither admission nor queueing observes the deadline, serial collection is
  bounded by the top task's deadline: on expiry the stream is dropped, which
  abandons the in-flight child and never submits the rest, and only a
  top-level timeout is returned. When the field is unset, batched tasks retain
  the legacy concurrent polling behavior.
  TiDB correlates child responses by task ID, so scheduling does not depend on
  response order.
- A successful mergeable batched result is folded into an error-free mergeable
  top result. Its batch response contains no data, sets
  `data_merged_into_response`, and keeps its execution details. Failed or
  non-mergeable tasks keep normal per-task responses.
- Final merging and serialization run in the read pool under the request's
  deadline, resource-control settings, selected semaphore group, and tracker.
  Outputs are buffered until finalization, so they contribute to peak request
  memory; each buffered output rides in its memory-trace guard, and attachment
  rebuilds the combined response's guard (adopting a batch response's node when
  the top response is untracked, e.g. a top task error) so the retained data
  stays accounted until the response drops.
- Data, acknowledgments, response-byte accounting, and memory tracing are
  published only after the final deadline check. Admission failure, deadline
  expiry, or failure to serialize a top result that already consumed child
  results returns no partial data or acknowledgments, allowing every task to be
  retried safely.

The main contracts live in `HandlerOutput` and `MergeableResult` in
`src/coprocessor/mod.rs`; orchestration is in `src/coprocessor/endpoint.rs`;
collection and finalization are in `src/coprocessor/batch.rs`.

## Process Lifecycle And Startup Sequencing

- Endpoint and read pools are created during server startup.
- Runtime behavior depends on read-pool setup, memory quota, concurrency
  controls, and storage snapshot access already being available.
- The main runtime anchors are:
  `src/coprocessor/readpool_impl.rs::build_read_pool`,
  `src/coprocessor/endpoint.rs::Endpoint::new`, and
  `src/server/service/kv.rs` as the RPC entry path.
- On the Yatp path, unary and streaming heavy tasks are admitted through
  semaphores created in `Endpoint::new`: a shared semaphore for ordinary
  coprocessor work and a dedicated semaphore for Analyze requests that are
  intentionally throttled by the background quota limiter.
- The shared semaphore is controlled by
  `server.end-point-max-concurrency`. The dedicated background-limited
  semaphore is enabled only when
  `server.end-point-max-bg-concurrency` is explicitly set to a positive value;
  its capacity is then controlled by that value.
- When the dedicated setting is absent or `0`, Analyze and ordinary Cop
  requests share the legacy semaphore. When it is positive, the two semaphores
  are independent and both lanes can make progress concurrently.
- The dedicated cap does not automatically track unified read-pool worker
  autoscaling at runtime.
- `build_read_pool` sets TLS engine state and marks threads as
  `IoType::ForegroundRead`. Any change that moves blocking work into or out of
  this path should be reviewed against foreground IO expectations.
- Online config is limited but real: `Endpoint::config_manager()` exposes
  `CopConfigManager`, which currently updates memory quota. If config scope
  expands, update lifecycle and operational sections in this guide together.

## Data Model And Metadata Contracts

- `ReqContext` is the key runtime metadata contract:
  context, ranges, deadline, peer, start ts, lock-bypass sets, bounds, cache
  version, perf level.
- Request parsing contract differs by request type:
  DAG, analyze, checksum.
- `HandlerOutput` always owns the traced response and records separately
  whether its data is ready or remains as a mergeable result until
  finalization.
- `ReqContextInner::new` is where deadline, bypass/access locks, and derived
  lower/upper bounds are normalized. Reviewers should treat changes there as
  cross-cutting request-semantic changes.
- `endpoint.rs::parse_request_and_check_memory_locks` is the main admission and
  normalization contract. Request parsing, memory-lock checks, API-version
  dispatch, and handler construction are deliberately coupled there.
- The hot request handlers differ materially:
  DAG requests use `dag/*`, analyze requests use `statistics/analyze_context.rs`
  and `statistics/analyze.rs`, and checksum requests use `checksum.rs`.
- Cache-match version, flashback allowance, and lock-bypass/access sets are all
  correctness-sensitive metadata, not optional optimization flags.
- The DAG `flags` bitmask is a network-facing contract. Bit 12,
  `Flag::ENABLE_SHORT_CIRCUIT_EXPRESSION`, enables lazy `LogicalAnd`/`LogicalOr`
  evaluation through `EvalConfig::from_request` and `RpnExpressionBuilder`.
- Lazy evaluation is left-to-right, row-selective, and must preserve SQL
  three-valued logic; short-circuit nesting is capped at 32. If the bit is
  absent or unknown to the server, the expression is not eligible/profitable,
  or the cap is exceeded, the existing eager `FnCall` path is used.
- In short-circuit mode, skipped argument functions are not invoked, so their
  warnings/errors are suppressed, although referenced columns may still be
  eagerly decoded; when unavailable, the existing eager path and SQL-mode
  warning/error behavior are preserved.

### In-process expression-library consumers

`components/tidb_query_datatype` owns the scalar representations and collation
kernels; `components/tidb_query_expr` owns RPN construction and execution. These
libraries can also be called in-process without constructing a coprocessor
request or starting a storage/server runtime. This does not change the wire
short-circuit admission policy described above.

- The wire tree builder and `local::compile_local` share shallow typed
  `CallShape` / `CallBuild` descriptors, one function selector, and opaque
  `PreparedCall` metadata. Local construction must not fabricate protobuf
  expressions or maintain a second signature dispatch table. Prepared calls
  retain the original argument-index mapping: IN's legacy constant extraction
  can leave dynamic children in swap-remove order, not source order.
- A compiled `LocalProgram` is worker-owned. Its existing `Any + Send` function
  metadata is not `Sync`; share immutable input specifications, not one mutable
  compiled program between workers. General local routes borrow the caller's
  persistent `EvalContext` and must not reset statement diagnostics. The closed
  evaluated-ASCII worker below has a separately sealed context capability.
- `compile_local` admission remains exact signed LongLong: constants/slots, the initial
  arithmetic/NULLIF kernels, and explicit AND/OR/IF/IFNULL/searched CASE/COALESCE
  controls. A private selected-call descriptor attaches control identity in the
  canonical selector; public `RpnFnMeta` layout and macro construction are
  unchanged. Wire and local paths share one iterative
  Program/Control/Host/Ordinary frame driver and one prepared-kernel invocation
  helper, not another interpreter or native fallback.
- `LocalBatch` requires decoded columns, an explicit physical row universe and
  checked selection. `eval_with_bindings` instead invokes `read_input` only at
  a demanded ColumnRef. Static schema/selection checks precede effects; replies
  must be exactly one Int value. `InputRow` separates physical row from selection
  occurrence. Width-one scheduling preserves order/repeats and actual typed
  `LocalError` variants. Do not pre-convert dead branches or unselected rows.
- Strict local controls never switch to eager at depth 32. Metadata traversal
  and program/spec destruction are iterative, including rejected construction;
  derived deep Clone/Debug are not guaranteed. Arc-share immutable specs instead.
  Caller-owned decoded root vectors retain their borrow/selection contract;
  frame-owned selections are materialized before returning child results.
- Ordinary calls on the existing wire/`compile_local` routes remain eager
  postfix. The separate `compile_local_profiled` route admits exact PlusInt203,
  signed LongLong, Typed Int/NULL literals, identity conversion, and explicit
  TypedRow/PbRow facts only. Its Ordinary frame completes the typed left operand,
  skips the right on left NULL, otherwise evaluates the right and enters the same
  prepared-kernel helper as eager calls. It does not substitute the legacy222 ID.
- `OrdinaryProfileSpec` retains a flat immutable snapshot of the full schema,
  values, slots and field types. Call records strictly cover ALL-node source
  preorder ordinals (root0, arguments left-to-right), not call-only or row indices.
  Compilers revalidate snapshots and source/site assertions; no cache is implied.
  PB labels and raw signature consistency do not prove actual PB ingestion: the
  producer owns that proof and native Int/UInt kind checks before transport.
- The profiled route refuses AST-value and native numeric-batch consumers,
  controls/hosts, other operators and mixed carriers. Native batch operand-major
  order cannot be replaced by occurrence loops or whole-expression tiling.
  Source/site identity and preserved LocalError variants are not a site-aware
  native SQL diagnostic adapter. The trusted implicit-cast constructor is not an
  unchecked local entry; further domains require explicit admission.
- `NumericBatchFacts` / `compile_numeric_batch` are a distinct, closed signed
  LongLong/PlusInt203 domain. `LocalNumericBatchProgram` has no raw or row escape;
  compiled Row/ControlLineage/SqlNumericBatch/EvaluatedAscii identities are checked
  even for empty and leaf programs. A shared flat snapshot worker does not merge row/batch facts.
  One driver invocation completes the whole left child, whole right child, then
  parent kernel lanes in selected-occurrence order. There is no left-NULL stop,
  including width one, and no per-row tree replay or root tiling. The bound is
  1024 selected occurrences, not physical rows; repeats/reordering remain distinct.
  The caller must prove the genuine suite/global-vectorization entry on every
  invocation. Library facts do not prove native eligibility or activate SQL.
- Execution semantics (`Unannotated`, SQL control lineage, SQL numeric batch,
  evaluated ASCII) and
  retained-storage policy (`ConservativeInt`, `ExactRetained`) are independent.
  Numeric batches use actual retained owners, including both suspended operands
  and output capacity, checked before the next effect/publication. They do not
  gain control/result lineage, byte kernels, hosts or a hard allocator-peak bound.
  Reported numeric calls use the same fresh failure-only recorder and preserve
  genuine input versus kernel sites without replay or guessed SQL diagnostics.
- `prepare_evaluated_ascii` constructs only the canonical Bytes-slot/ASCII7003
  program and returns an opaque owned worker, not a raw program/context/graph.
  Its distinct compiled and execution domains require ready-Bytes input even
  for internal empty/no-read misuse. Existing legitimate wire ASCII stays valid.
  The frontend already evaluated/coerced its operand; this is no original SQL/PB
  provenance or full FieldType proof. The same compiler and frame/kernel driver
  consume one borrowed Bytes scalar without copying it to a Bytes vector. NULL
  still dispatches the official nullable wrapper; there is no NULL shortcut or
  native retry. Only a checked singleton computed Int with its own signed metadata
  may leave the worker; native result coercion remains a separate caller operation.
- This exact context-free ASCII body permits one private UTC/default/zero-detail
  context per created worker. No session/native context or callback is accepted.
  Fixed metadata caches are prewarmed before publication without executing a fake
  NULL call. Owner observation never initializes the cache and includes its Box
  and referenced-offset Vec capacity; canonical heap-free field/function metadata
  is established by construction, not serialization or logical equality.
  Warning count AND details must stay empty. An in-flight sticky state prevents
  reuse after a panic caught elsewhere; dirty/unmeasurable workers are disposed
  before native continuation. Cleanup/health failure cannot overwrite an original
  kernel error. No per-row context recreation, warning drain or silent repair.
- Evaluated input Vec capacity remains charged through its actual lifetime and
  overlaps the generated Int output check. Worker inline and owned-heap accounting
  are separate from per-call storage and caller pool/container/creation ledgers.
  The private Arc-config accounting proxy is pinned, uses no Arc memory access,
  and requires independent actual allocation-request validation on each compiler
  cohort. This is not portable Arc layout, allocator usable slack or peak memory.
  The caller admits creation after native coercion, keeps live/creating/idle and
  retirement debt charged, and owns scope/epoch closure and caught-panic placement.
- The worker's invocation counter observes the actual shared fn_ptr dispatch,
  including the nullable wrapper, not non-NULL body execution. The cfg(test)-only
  ASCII body hook has a separate isolated ignored test; run it alone with ignored
  tests explicitly enabled, never infer body coverage from a facade counter.
  This library route alone proves no native hot-entry activation, algorithm
  deletion, whole-family migration or performance acceptance.
- `eval_with_bindings_reported` uses the same evaluator and moves back the original
  `LocalError` with an optional owned failure site. Only actual ordinary-kernel
  errors and `read_input` errors capture a site; first capture wins on immediate
  propagation. Validation, budgets and checks after success remain unsited.
  An input's ResourceLimit or code 1690 is still an Input failure, not a kernel
  overflow. The typed code getter reads the existing EvaluateError code; it never
  parses Display text, and unannotated eager failures acquire no guessed site.
- Reports use fresh stack-local failure-only state, not a persistent last-site,
  ancestor lookup or replay. Input slots identify bindings, not universally unique
  native leaves. Caller-owned mappings and exact invocation coordinates establish
  that join. Reporting preserves warnings; callers can observe live count/length
  endpoints without assuming a configured cap or draining details. Panics are not
  converted to reports. This new reported entry refuses Host programs after
  schema/selection validation and before host hooks; the old entry remains intact.
- `compile_control_with_lineage` is a separate checked SQL TypedRow control-only
  admission over the same compiler/frame driver: Int/String IF, IFNULL, searched
  CASE and COALESCE, plus binary AND/OR. `ControlLineageFacts` snapshots full
  source/schema/producer facts in all-node preorder. It refuses ordinary-call
  composition, Host, PB/AST/native-batch domains, implicit casts and BinaryLiteral.
  Possible unsigned selected values remain valid Int values but are conservatively
  refused in PredicateInt roles; this does not mean native UInt truth is invalid.
- `LocalControlProgram` has no raw-program escape. Its `LineagedBatch` carries one
  caller-owned `ResultMetaId` per selected occurrence, including repeated rows.
  Selection forwards the chosen child's current ID, even for NULL. Exhausted
  COALESCE and CASE without ELSE generate their own NULL ID; AND/OR own their
  computed result ID. Parent SQL declaration and selected native Datum metadata
  are different facts. The caller binds its own program/table and validates native
  kind/collation on the same demanded read before type erasure; numeric IDs alone
  cannot authenticate a foreign table or recover erased String/Bytes/UInt kinds.
- The lineaged mode measures retained Int/Bytes owners, bitmap/offset capacity,
  frames, accumulators, output and ID vectors before a subsequent effect and
  publication. `BitVec`/`ChunkedVecBytes` heap getters count actual Vec capacities;
  checked reserve helpers preserve logical contents on error, not necessarily
  earlier increased capacities. Empty Bytes still needs its offset sentinel.
  An unknown provider reply is measured only after that read; no pre-callback or
  hard allocator-peak bound is claimed. Conservative minimum precharges may
  over-refuse. Old modes retain conservative payload accounting, but both charge
  actual frame layout, so enlarging a frame can change fixed-byte-cap refusal
  prefixes. Output materialization/opaque caller allocations need separate budgets.
- `compile_local_with_hosts` accepts an immutable typed `HostCatalog`; its opaque
  key is process-local identity, not signature equality. Slots cannot be forged
  or remapped between catalogs. Arguments/results remain exact signed LongLong
  and each host invocation is width one. `PreparedHostCall` contains no native
  evaluator or expression callbacks. Protocol definitions live in `local/host.rs`
  and lifecycle fixtures in `local/host_tests.rs`.
- `LocalRuntimeServices::host_services` defaults to None, preserving host-free
  callers, whose programs never request that view. An advertised provider must
  implement catalog/start/resume/cancel. The same frame driver evaluates requested
  children: Fresh recomputes without an old-value fallback; Reuse keeps only the
  most recent successful argument value within that invocation. Callback borrows
  cannot be retained; suspended adapter state is owned and keyed by task IDs.
- The driver reserves a potential task before start, even for immediate Ready.
  Pending IDs are tracked before request validation. Errors, limits and unwind
  cancel reachable tasks inner-first without replacing the primary error; cancel
  must be nonpanicking, diagnostic-free and idempotent, including completed IDs.
  Providers must release unpublished state on start error/immediate Ready and
  finish tasks before resume Ready. Cleanup requires a stable, contract-compliant
  provider: a vanished/changed namespace cannot be cleaned through another one.
- Per-invocation Demo limits bound steps, frames, task ledger and retained scratch;
  they are not an external cancellation or total allocator-memory guarantee.
  Defaults are unlimited steps, 1024 frames, 64 MiB retained scratch and 256
  potential live tasks. Adapters meter their own opaque allocations. Mutable
  services, tasks and evaluation frames never survive into reusable worker state.

### Shared collation and Decimal boundary contracts

- `Collator::write_sort_key_with_options`, owned keys and borrowed `Cow` keys
  use one unpadded writer after the collator's preprocessing. `KeyOptions::NoPad`
  preserves trailing ASCII spaces without changing the collation. Allocation
  estimates count the original input, not its trimmed prefix. A caller must
  preserve signed protocol collation IDs rather than applying `abs(id)`.
- `SortKey::new` validates the charset; raw key writers do not implicitly do
  so. UTF-8 raw decoding retains the Go-style one-byte replacement policy.
  `sort_hash(value)` is deliberately **not** `hash(sort_key(value))`; do not
  substitute it for a caller's existing group/join/key byte protocol.
- `codec::collation::pattern` owns one escape tokenizer and backtracking loop
  for raw and compiled LIKE paths. Byte, binary-rune and collator-defined modes
  are distinct (notably GB18030 binary versus GBK binary). Trailing escape is
  an explicit `Literal`/`Reject` policy; JSON search uses the latter. SQL front
  ends may retain immutable compiled-pattern caches, not a second matcher.
- `Decimal` is an owning, Clone/non-Copy value with nine inline `SmallVec`
  words and checked counters, not a C layout. Existing public arithmetic keeps
  Fixed(9); private Grow add/sub/mul/division/AVG/rounding share the same workers,
  not a second arithmetic engine. General wide publication is still closed while
  legacy trait/domain edges remain unresolved. Checked count/allocation failures
  stay outside SQL numeric dispositions. Status payloads can need more initialized
  backing cells than the fixed arithmetic limit. `DecimalWordsRef` exposes exact
  borrowed logical fields, including independent storage/result scale and
  initialized inactive words.
- Fixed multiplication's capacity selection operates on total aligned operand
  word windows: loss described as fractional can also remove low integer cells.
  Fixed rounding likewise retains its legacy Truncated partial-selection policy.
  Do not silently replace either with fraction-only clipping or Grow-then-clamp;
  exact Grow results are not oracles for these declared Fixed dispositions.
- `DecimalParts` remains checked bounded logical transport, **not** a raw
  40-byte layout or FFI type. `try_from_parts` validates capacity/ranges and
  partial-word shape. The non-consuming `try_to_parts` is fallible: empty active
  prefixes, oversized status payloads and noncanonical physical shapes do not
  necessarily fit the strict logical contract. Do not assume that every value
  visible through `words()` can pass the bounded import/export pair.
- SQL declared precision/scale belongs to the typed caller, separately from
  Decimal storage/result scale. Keep `Res::Ok`/`Truncated`/`Overflow` until that
  caller applies warning/error policy; transport must not erase disposition by
  unconditionally unwrapping it. Successful zero multiplication retains scale
  (`0 * -1.1` becomes `0.0`), but overflow negative zero remains meaningful.
  This correction is not a claim of complete Go multiplication equivalence:
  hidden storage/result-scale and truncated-zero policies still need review.
- `ChunkedVecSized<T>` stores initialized, owned `T::default()` values behind
  NULL bitmap entries, never generic all-zero memory. The five owned `Evaluable`
  kinds require `Default`; borrowed `EvaluableRet` does not. Clone, replacement,
  append, truncation and drop retain normal ownership, even for a hidden NULL
  payload, so heap accounting cannot ignore hidden owned allocations. Encoders
  consult validity: a NULL Decimal chunk cell is still 40 zero bytes, whereas a
  non-NULL default Decimal has its valid integer-digit count of one.
- The physical chunk cell remains exactly 40 bytes, encoded/decoded through
  explicit header bytes and native-endian words, never a copy of the owning
  Rust object. Its transport admission is separate: empty counts, noncanonical
  partial-word shape/padding, all nine inactive words, and result-header bytes
  0..255 are retained. Invalid bool, overcapacity active counts and out-of-base
  active words are rejected before constructing an owner. This is not full
  compatibility with unchecked raw-like-Go negative counts/arbitrary words.
  Display selects legacy signed-byte/Fixed30 formatting only when active extent
  is at most nine words and result scale at most 255; otherwise it uses the full
  result-scale writer. No hidden origin tag decides the policy. The reviewed
  overcapacity Overflow(81/2/2) negative-zero payload now displays `-0.00`
  instead of `0`, without changing its status/fields; this is not Go string
  equivalence (that Go state panics). Storage/result writers share one emitter
  with bounded 128-byte staging; a rejecting sink stops before huge zero padding.
  Fixed MOD now selects the full integer/fraction extent, including leading
  fraction gaps, and stores selected fractional words as digits (times nine).
  Its capped quotient/early/sign rules stay intact. Legacy zero visible scales
  through 255 retain their storage policy; newly wider visible scales use input
  storage scale rather than densely allocating from display metadata. These
  checked fixes are not a claim of equivalent arithmetic for every raw physical
  shape or permission for general wide publication.

- Fixed MAX factories require both precision >= scale and separately aligned
  integer/fraction words fitting nine words; precision <= 81 alone is insufficient.
  The checked conversion path validates before fast paths/narrowing and copies all
  initialized raw cells on a no-op, not just active logical cells. Wire encoding
  rejects precision < scale before writing a header. Its overflow/truncation logs
  intentionally emit bounded scalar shape metadata, never full Decimal Display.
- Native Decimal-to-f64 remains a storage-text conversion: count through the same
  emitter, reserve fallibly, then use Rust parsing. Visible padding is ignored;
  infinity and signed underflow zero retain existing behavior without new context
  warnings. This is not TiDB/Go result-scale projection or an inverse of the bounded
  finite-only from-f64 path.
- Generic Decimal `ConvertTo<String>` / `ConvertTo<Bytes>` Result wrappers consult
  a codec-private fallible adapter backed by that same checked STORAGE emitter.
  Bytes moves the returned String allocation. Public traits/bounds/defaultness and
  old infallible ToStringValue remain unchanged; non-Decimal default and specialized
  value formatting stay intact. Resource errors propagate as codec errors without
  context disposition. This is not RESULT Display, physical ENOMEM validation,
  general fallibility for other types, temporal input or public-wide admission.
- Inherent `Datum::to_string` (and its `into_string` consumer) renders Decimal
  RESULT text through one private fallible String sink and the untouched Display
  dispatch, not the STORAGE adapter above. Each append reserves before writing;
  failed output stays private and returns an outer codec error, never SQL Overflow.
  Existing legacy signed-byte/Fixed30 display behavior and physical owner state
  remain intact. A bounded isolated one-shot allocation refusal validates this
  target boundary; allocating the error message can still fail under sustained
  OOM. Other Datum formatting, temporal consumers and public-wide admission are
  not made generally fallible by this change.
- `produce_dec_with_specified_tp` checks fully specified declared targets before
  no-op/overflow effects: preserve precision < scale error priority, then checked
  narrowing and the existing separate-word Fixed9 limit. Either-UNSPECIFIED
  bypass stays unchanged. Codec-scoped checked MAX and borrowed Fixed-round
  helpers share the existing fill/round workers and copy all initialized raw
  cells fallibly. Overflow context disposition still precedes saturation;
  truncation/DML ordering and unsigned handling last are unchanged. Do not add a
  SQL65/30 cap, post-carry integer check, or delegate this policy to `convert_to`.
  This producer gate does not authorize wide diagnostic or temporal consumers.
- Contextual `Res` disposition has one shared worker. The eager overflow-error
  API remains compatible; `into_result_with_overflow_err_lazy` invokes its
  `FnOnce` factory only on Overflow. MOD/DIV supply their unchanged diagnostics
  lazily; Ok and Truncated do not render unused operand messages. Simple Decimal
  interval units likewise retain their existing round/as_i64 path without first
  formatting discarded Decimal text; Second/composite formatting is unchanged.
  This does not bound true overflow diagnostics or temporal/parser input, alter
  Display, or authorize a new-wide synopsis/resource policy.

Relevant targeted tests, from the TiKV repository root:

```sh
cargo test --locked -p tidb_query_datatype --lib
cargo test --locked -p tidb_query_codegen --lib
cargo test --locked -p tidb_query_expr --lib
cargo test --locked -p tidb_query_aggr --lib
```

These cover existing wire behavior, checked local controls/selection/demand,
owning Decimal and physical NULL contracts, plus aggregate ownership consumers.
They do not establish general SQL lowering, ordinary-call scalar NULL-stop,
wide Decimal arithmetic, complete raw physical compatibility or performance.

## Start Here

- `components/tidb_query_datatype/src/codec/collation/mod.rs` and `pattern.rs`
- `components/tidb_query_datatype/src/codec/mysql/decimal.rs`
- `components/tidb_query_expr/src/types/function.rs` and `local/`
- `src/coprocessor/mod.rs`
- `src/coprocessor/endpoint.rs`
- `src/coprocessor/batch.rs`
- `src/coprocessor/readpool_impl.rs`
- `src/coprocessor/dag/*`
- `src/coprocessor/statistics/*`
- `src/coprocessor/interceptors/*`
- `src/coprocessor/config_manager.rs`

## Must-Read File Order

1. `src/coprocessor/mod.rs`
2. `src/coprocessor/endpoint.rs`
3. `src/coprocessor/batch.rs`
4. `src/coprocessor/tracker.rs`
5. `src/coprocessor/readpool_impl.rs`
6. `src/coprocessor/interceptors/deadline.rs`
7. `src/coprocessor/interceptors/concurrency_limiter.rs`
8. `src/coprocessor/dag/mod.rs`
9. `src/coprocessor/statistics/analyze_context.rs`

## Main Responsibilities

- parse coprocessor protobuf payloads
- build `ReqContext`
- acquire snapshots and perform memory-lock checks
- construct request handlers
- execute handlers on read pools
- enforce request deadlines, concurrency limits, and memory quotas
- collect execution stats and produce coprocessor responses

## Critical Invariants

- Range bounds in `ReqContext` must stay aligned with the actual request.
- Memory-lock checks must happen before serving reads that could violate lock
  semantics.
- Handler execution must respect request deadline and cancellation behavior.
- Memory quota and concurrency limiters must remain cheap and correct.
- Request parsing and admission must stay aligned: when the dedicated setting
  is enabled, a request class that reports quota samples to the background
  quota limiter should use the dedicated background-limited semaphore instead
  of bypassing heavy-task admission. With the setting disabled, it intentionally
  shares the ordinary semaphore.
- When enabled, the dedicated background-limited semaphore protects all
  Analyze variants, including index, common-handle, column, mixed, and
  full-sampling Analyze, from unlimited fan-out. It is not part of the ordinary
  shared heavy-task budget; when disabled, these requests intentionally use the
  shared semaphore.
- Streaming and unary response handling must preserve stats and partial-progress
  semantics.
- Batched unary result merging must preserve task identity, retry semantics,
  deadline enforcement, response-byte accounting, and memory tracing.
- Serial batch execution must keep at most one task from the request active
  without changing result-merging or response-order semantics.

## Observability And Operational Signals

- wait-time and snapshot-time metrics
- request-type metrics and execution summaries
- slow-log behavior driven by endpoint thresholds
- Resource metering / TopSQL records the per-request RocksDB PerfContext
  `block_read_count` delta as `rocksdb_block_read_count` when both
  `resource-metering.enable-network-io-collection` and
  `resource-metering.enable-detailed-io-collection` are enabled. The field is
  used for the downstream `read_iops` dimension and relative attribution; it is
  not a device-level IOPS measurement. Unary and streaming handler futures must
  keep this PerfContext accounting poll-scoped so TLS metrics cannot be
  attributed to another request. Keep that poll observer separate from the
  streaming item lifecycle: one item can span multiple polls, but its
  `ExecDetails` process time must still cover the complete item.
- Start with `src/coprocessor/metrics.rs`. High-value signals include:
  `tikv_coprocessor_request_duration_seconds` family,
  `tikv_coprocessor_request_wait_seconds`,
  `tikv_coprocessor_request_handler_build_seconds`,
  `tikv_coprocessor_request_error`,
  `tikv_coprocessor_scan_keys`,
  `tikv_coprocessor_scan_details`,
  `tikv_coprocessor_response_bytes`,
  `tikv_coprocessor_waiting_for_semaphore`, and
  `tikv_coprocessor_semaphore_wait_time_duration_seconds`.
- The semaphore wait metrics use `group=shared|background_limited` to
  distinguish ordinary Cop request pressure from Analyze background-limited
  throttling. Dashboard queries should preserve this label when diagnosing an
  individual lane and aggregate it only when displaying total semaphore
  pressure.
- `tracker.rs` is the best place to understand slow logs, exec details, request
  lifetime accounting, and the distinction between schedule wait, snapshot
  wait, suspend time, and processing time.
- RU-v2 batch-selection work uses expression work units plus column-reference
  count. Expression work units expand flattened `AND`/`OR` chains back to their
  conceptual binary logical operations, so flattening must not reduce
  `tikv_coprocessor_executor_work_total_batch_selection`.
- Triage starting points:
  `endpoint.rs`, `tracker.rs`, `readpool_impl.rs`, `metrics.rs`,
  `interceptors/deadline.rs`, `interceptors/concurrency_limiter.rs`.

## Change Management Guidance

- If `ReqContext`, request parsing, timeout behavior, or resource-control
  integration changes, update this guide in the same patch.
- Hot-path changes should be reviewed together with performance-critical-path
  expectations.
- Treat `endpoint.rs` as both a correctness and latency hotspot. Extra parsing,
  allocation, or logging there needs justification.
- If lock checking or extra snapshot access logic changes, review the change
  with `src/storage` and concurrency-manager semantics in mind, not as a
  coprocessor-only patch.
- If a new request type or major execution mode is added, document its parser,
  handler builder, resource admission path, and observability surface here.

## Change-Impact Matrix

- Request parsing or context changes:
  inspect `mod.rs`, `endpoint.rs`, and request-type-specific builders
- Timeout or concurrency admission changes:
  inspect interceptors, `tracker.rs`, metrics, and read-pool behavior
- DAG execution changes:
  inspect `dag/*`, expression evaluation, snapshot/store setup, and query-side
  statistics paths
- Analyze or checksum changes:
  inspect `statistics/*` or `checksum.rs` plus exec-detail accounting

## Review Checklist

- Does the change touch `endpoint.rs` parsing or request-type dispatch?
- Does it affect `ReqContext`, deadline handling, or lock bypass/access sets?
- Does it change read-pool wiring or per-request resource control?
- Does it add extra allocation, parsing, or logging to the hot path?
- Does it change handler stats collection or slow-log behavior?

## Observability And Tests

- Inline tests exist across handler and statistics modules.
- Performance-sensitive behavior often needs bench or end-to-end query testing.
- Metrics live in `metrics.rs`, trackers, and read-pool tickers.
- `endpoint.rs` itself contains many targeted tests for parsing, lock checking,
  timeout, and snapshot-access behavior. It is one of the most important files
  to consult when changing request admission semantics.

## Common Failure Modes

- wrong request classification or parser fallback
- lock-check bypass on paths that should block
- deadline handling drift between streaming and unary paths
- memory quota leaks on early-return/error paths
- concurrency limiter behavior only applied to one pool mode

## Reading Map And Companion Docs

Suggested reading order:

1. `mod.rs`
2. `endpoint.rs`
3. `batch.rs`
4. `readpool_impl.rs`
5. `dag/mod.rs`
6. `statistics/analyze_context.rs`
7. `interceptors/*`

Companion docs:

- `repo-overview.md`
- `src/server.md`
- `src/storage.md`

## Glossary

- DAG request:
  built-in coprocessor request for pushed-down query execution
- ReqContext:
  immutable runtime request context shared through execution
- Light task threshold:
  the execution-time budget before a coprocessor future must acquire a
  semaphore permit in the Yatp path
- Snapshot wait:
  request time spent between scheduling and obtaining the storage snapshot

## Related Components

- `src/server/service/kv.rs` is the RPC entry point.
- `src/storage` provides snapshots and the lock-related behavior.
- `components/in_memory_engine` can accelerate snapshot-backed reads indirectly.
