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
  closed evaluated-arguments worker below has a separately sealed context capability.
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
  closed evaluated arguments, including the legacy ASCII entry) and
  retained-storage policy (`ConservativeInt`, `ExactRetained`) are independent.
  Numeric batches use actual retained owners, including both suspended operands
  and output capacity, checked before the next effect/publication. They do not
  gain control/result lineage, byte kernels, hosts or a hard allocator-peak bound.
  Reported numeric calls use the same fresh failure-only recorder and preserve
  genuine input versus kernel sites without replay or guessed SQL diagnostics.
- `prepare_evaluated_bytes` constructs one closed operation over canonical ready
  slots and returns an opaque worker, not a raw program/context/graph. Its legacy
  name also covers fixed Int, Decimal, full i128 and multi-input string operations.
  `EvaluatedArgs` admits only the closed nullable Bytes/Int combinations, raw IEEE
  singles/pairs or IEEE+Int, typed ready-count/packet operands, real Decimal or
  Decimal+Int inputs, canonical LE16 Int128, and an actual-NULL witness role. The operation fixes ordered slot
  types/arity and an exact call recipe; callers cannot supply arbitrary FieldTypes.
  Existing operations use one FnCall. Closed negated boolean tests use only
  base+UnaryNot, checking each stage's signature, name, function pointer, arity and
  metadata in order. Their source tree needs compile depth3, not a new evaluator.
  `eval_args` checks shape before invocation, then uses the existing compiler and
  frame/kernel driver with a fixed stack array and borrowed ready scalar slots.
  `eval_one` and `prepare_evaluated_ascii` are thin compatibility paths, not second
  implementations. Existing legitimate wire execution remains unchanged.
- The frontend has already applied its own child demand, coercion and byte/text
  normalization; ready values prove neither original SQL/PB origin nor complete
  SQL FieldTypes. NULL still invokes the official wrapper, including QUOTE(NULL)'s
  non-NULL string result. No native retry is available. Checked singleton Int or
  owned Bytes results carry their own computed metadata; original SQL charset,
  unsigned result packing and return casts remain the caller's responsibility.
  BitCount and the six bitwise operations use the same Int carrier: it preserves
  all64 bits, not a checked unsigned-to-signed numeric narrowing. Logical shifts
  and counts>=64 come from the official kernels. Native string/Decimal/Real
  normalization and coercion diagnostics happen before this ready-value boundary;
  OwnSignedInt describes computed transport, not the caller's UInt SQL result.
  Boolean operations consume frontend-normalized truth/presence as nullable Int0/1;
  this does not restrict original SQL operands to Int or silently coerce NaN to NULL.
  NULL IS NOT TRUE/FALSE requires base+UnaryNot, not substituting the opposite IS
  test. Both official wrappers really execute, so the dispatch counter is two.
  Structural factory prewarming still executes no kernel.
  MD5 and SHA1 use the existing official nullable Bytes-to-Bytes kernels, including
  their lowercase hex output and OpenSSL errors. Native text conversion/packing is
  outside the kernel; no retry or second digest implementation is provided here.
  SHA2 now uses the quiet native recipe described below; warning-producing
  compression operations remain outside these recipes.
  Logical AND/OR/XOR consume normalized ready Int2 and execute the official eager
  FnCall. Only closed-ready AND/OR may retain the exact signature/control tag and
  complete ordered arguments while bypassing FinishControl; ordinary Row/wire
  lowering keeps its original lazy control node. The frontend owns original child
  demand. It may supply an irrelevant RHS representative only after validating
  explicit AND(false, undemanded) or OR(true, undemanded); this is not a claim that
  RHS was evaluated as NULL. The real kernel still computes every result.
  INET_ATON, INET_NTOA, INET6_ATON and INET6_NTOA use the existing nullable
  Bytes/Int shapes and official parse/range/format kernels. The frontend retains
  text/raw-byte coercion, integer warnings, UInt bit transport and result tags.
  Malformed raw INET6 bytes reach the kernel unchanged. Four IS_IP predicates
  use private default-NULL-propagating wrappers calling the existing official
  functions for non-NULL inputs; wire NULL-to-zero remains unchanged. Only the
  native IPv4 input spelling removes redundant leading zeros from existing dot
  segments, preserving empty segments, separators and other characters; the
  kernel still decides validity. IPv6 and binary prefixes are not normalized.
  PI has a distinct NoArgs role: empty schema/ready slice, no column references,
  one zero-argument FnCall and one output row. Its private wrapper calls the
  existing pi function, so there is no second constant or dummy argument.
  ClosedPrivate selects these getters through common preparation; argument
  roles remain independent, and ordinary registry dispatch rejects private IDs.
  ASIN/ACOS/SQRT/SIGN/RADIANS/DEGREES additionally use a sealed IEEE754Bits role,
  with nullable eight-byte little-endian physical transport and owned bit results
  (SIGN remains signed Int). Ordinary Bytes cannot impersonate this role, even
  with length eight or NULL. Six private LocalFunctionId values are rejected by
  the ordinary registry; only the closed factory chooses their fixed getters via
  existing common preparation, validation and metadata construction.
  The six raw-f64 primitives live once in impl_math.rs and serve both original
  Real wrappers and private wrappers, without widening Real/NotNan. The frontend
  owns conversion and output policy: ordinary inverse trig maps NaN to NULL,
  legacy real consumers retain NaN, and native finite-result errors retain their
  original diagnostics. No alternate driver, callback or native fallback is added.
  ABS/CEIL/FLOOR/ROUND/TRUNCATE and three legacy ROUND policies add 23 private
  recipes in `impl_math.rs`. Decimal inputs/results use actual ScalarValue and
  VectorValue Decimal storage, preserving wide words and independent storage/result
  scales; there is no Display/parse or nine-word bridge. Decimal DI/DII recipes
  append the worker's finite remaining retained budget, not a SQL argument or a
  precision cap. Accounting includes initialized cells, bitmap and every owned
  Decimal spill, including NULL backing, plus extraction coexistence. This is not
  an allocator high-water claim. The controlled datatype facade shares the existing
  round/shift/ABS workers while preserving native unchecked scale arithmetic and
  distinct native-Go versus wire policies. Native wrappers remove the duplicated
  arithmetic; the separate native ceiling-rounding helper is not part of this batch.
  ComputedDecimal supplies a checked i64 view only for CEIL/FLOOR and the two
  constant integer-negation recipes; out-of-range values retain their Decimal
  representation. Int128 uses strict LE16 transport and a
  real identity kernel. Legacy real ROUND remains ties-away, distinct from native
  ties-even; legacy Decimal rounding occurs before its storage-value f64 conversion.
  MathNullWitnessNative accepts only an actually observed NULL, not a fabricated
  numeric operand, allowing PB demand/arity precedence to remain frontend-owned.
  `eval_args_reported` preserves the original LocalError and attaches a typed SQL
  view only after the sealed AbsIntNative wrapper actually fails with the explicit
  AbsSignedOverflow cause. It does not inspect SQL codes/messages or predict from
  input. Explicit Caused errors preserve Decimal bridge/resource causes without
  changing the legacy boxed-error conversion or wire behavior. The old `eval_args`
  maps the receipt back to its original error; no context-last-error channel,
  general graph admission, alternate driver or four-column expansion is added.
  Numeric unary plus/minus use eleven unit-metadata recipes in `impl_op.rs`.
  Int, full-f64 IEEE bytes, Bytes and actual-NULL witnesses reuse existing roles;
  both Decimal recipes retain the actual `[Decimal, Int budget]` two-slot shape.
  No factory bound, carrier, metadata binding or driver is expanded. Runtime plus
  copies actual kernel input; native packing preserves UInt/Float32(f64), string
  collation and plus-Decimal declared column shape, while SQL's existing plus
  rewrite still removes the operator. The shared checked integer-negation leaf
  serves wire and native policies. Column overflow carries an actual typed
  `NativeUnaryMinusError`; only the two matching column operations, signedness,
  and this call's dispatch witness authenticate the native SQL diagnostic.
  Constant kernels promote in TiKV and return real Decimal values with the narrow
  checked integer view, not host threshold decisions or answer-control bytes.
  `NativeDecimalOp::Negate` reuses the wire `Neg` leaf and native finish; the native
  public Decimal helper is a thin bridge. Native canonical zero and shape clearing
  do not alter wire raw zero policy. Existing coercion/warnings, NOT/BitNeg, SQL
  child demand and PB/legacy admission remain unchanged.
  Binary plus/minus/multiply add 43 unit recipes: 13 native integer, three each
  native IEEE/Decimal/vector, an actual-NULL witness, ten legacy i128 policies,
  three each legacy IEEE/Decimal, genuine missing-child NoArgs, and three Decimal
  batch-fast recipes. Kernels live in `impl_arithmetic.rs`, with vector layout and
  kernels in `impl_vec.rs`. DecimalBinary carries two actual Decimal values and
  the infrastructure budget; Int1282 carries two complete LE16 values. Existing
  factory bounds and the common driver remain unchanged. Vector inputs remain
  typed; output is actual serialized LE vector data through the existing decoder.
  Integer and floating arithmetic share leaves while retaining explicit policies:
  native unsigned multiplication uses both raw u64 operands, mixed wire multiply
  still rejects negative signed input, and native's existing zero-minus-MIN
  subtraction differs from wire signed overflow. Exact operation/domain/profile,
  actual typed `Caused` error and this invocation's witness authenticate native and
  legacy arithmetic SQL errors; resource and shape errors are not SQL overflow.
  Shared Decimal Exact and MySql policies use the existing word workers. MySql
  preserves leading-word overflow, nine-word projection, the unsigned-coefficient
  i128 fast condition, hidden scales, signed overflow zero and native rounding;
  wire Fixed behavior remains distinct. Native public arithmetic, coefficient
  helpers and codec projection are thin facades. Grow/native multiplication
  handles the additional carry reduction without changing the wire Fixed leaf.
  Batch-fast signed coefficient/scale arithmetic is a separate shared policy.
  Its sole new computed outcome distinguishes Unsupported from Value(NULL) and
  Value(actual coefficient/storage/visible scales), via a strict closed decoder;
  it is not a SQL-NULL fallback or a general JSON report. Native callers retain
  whole-left-batch then whole-right-batch demand and stage all cells before append.
  Native integer batch NULL cells also use the actual arithmetic NULL witness.
  Unsupported returns to the existing ordinary TiKV worker path, never native
  arithmetic; infrastructure errors propagate. Legacy integer child evaluation
  remains eager on both sides, unlike sequential legacy real/Decimal demand.
  PB admission, original warnings and caller diagnostic formatting are unchanged.
  Added vector layout copies and coefficient bridges do not establish zero-copy,
  physical-heap or allocation-peak guarantees. Gigabyte-scale release arithmetic
  wrapping into an unrepresentable Decimal shape is explicitly deferred rather
  than weakening all bridge validation or converting it into SQL overflow.
  The six native comparison predicates now use 13 explicit domain/profile
  identities carrying a finite `ComparisonOp`. Their selectors choose 78
  distinct unit-metadata kernels in `impl_compare.rs`; no operation code is an
  SQL operand or runtime metadata. Results are owned signed0/1, with separate
  actual-NULL and missing-child terminals. Native IEEE and legacy total float
  order are distinct profiles. Existing argument roles, input budgets and
  collation tags are reused, with strict nonnull admission only for the new
  value profiles. No driver, carrier, binding or ordinary PB/legacy admission
  is added. Native typed/batch/PB/legacy paths submit real values; row predicates
  compose scalar outputs and shared NOT. Row preparation explicitly activates
  statement truncation/date-mode/timezone/warning context previously lost to
  NoColumns; this is a compatibility change, not old-context equivalence.
  Default row collation/precision and NaN Eq-then-Lt/NOT behavior are retained;
  empty-row identities remain structural and NullEq is a separate family.
  The preceding shared datatype substrate remains the comparison foundation. `json/native_policy.rs` owns the native raw
  comparator and its serde/lossless decoder closure, with a data-only generic
  node and separate InvalidBinary/TooDeep error. Native duplicate-key vectors,
  count-before-key ordering, exact double versus mixed-number epsilon rules,
  opaque/temporal rank, malformed fallback and distinct decoder depth/slice
  behavior do not become wire `JsonRef::cmp` semantics. Encoding and parsing
  remain outside this ownership move. `Time::native_core_compare` reuses the
  existing raw ordering for native calendar bits without validating them or
  interpreting packed-wire time; only the low four metadata bits are ignored.
  Borrowed `NativeDecimalCmpParts` and `native_decimal_cmp` retain the native
  coefficient-storage policy without allocating an owned Decimal bridge or
  changing word-backed wire ordering. These helpers add no evaluator family,
  driver, result carrier, call binding or PB admission by themselves. The
  evaluator integration above is separate from that substrate ownership move;
  physical-heap/OOM, broader request-root closure and complete type-domain
  equivalence remain outside this checkpoint.
  AES adds 24 unit-metadata value profiles (two directions, three key widths,
  four SQL modes) and one genuine NULL witness. ECB uses the existing two Bytes
  inputs; CBC/OFB/CFB use three, with full passwords and IVs retained until the
  actual wrapper. Only these value profiles require nonnull operands at both
  admission layers. The shared `tidb_query_crypto::aes` module owns the original
  mode loops, padding, key folding and public CTR compatibility closure, backed
  by the already-used RustCrypto AES 0.9.1 primitive. Its opaque block interface
  keeps the native random-access I/O layer separate without exposing cipher
  variants; the unrelated native GCM primitive dependency remains unchanged.
  The actual wrapper checks/slices IVs before deriving keys. Exact operation and
  mode/key profile plus this-call dispatch witness authenticate short-IV causes
  only for the 18 IV profiles; ECB/NULL cannot manufacture that SQL diagnostic.
  Cipher-domain errors yield successful optional Bytes, never infrastructure
  errors disguised as NULL. Frontends retain original lazy demand and ignored-IV
  warning order. No new carrier, computed kind, metadata binding, ordinary PB
  admission or driver is needed; full heap/peak/OOM and FIPS remain unclaimed.
  True division adds four closed value profiles, native/legacy real and
  native/legacy Decimal. Real uses existing IEEE-bit transport; Decimal uses
  two official Decimal inputs and the existing third infrastructure budget slot.
  Full-u32 precision lives in a transient typed metadata binding, not an invented
  SQL operand. The actual wrapper enters once and records one disposition or
  infrastructure-error terminal; metadata never stores a second Decimal owner.
  Materialization checks this invocation's witness and actual official result
  presence, then consumes the status into an owned DecimalDivision report before
  the guard clears all binding state. Fixed metadata storage is accounted, and
  invalid transitions prevent reuse without hiding the original error.
  Native target-scale arithmetic after the real zero gate and legacy raw
  increments remain distinct profiles. The shared datatype native quotient
  wrapper reuses Grow long division with checked live scratch/output budgeting,
  preserving signed overflow saturation, visible-floor truncation and full
  stored precision. Native callers apply warning/error policy after the report;
  legacy callers retain the payload silently. Genuine NULL/missing use the old
  terminal recipes. Ordinary PB admission, wire Fixed9 policies and IntDIV
  warning/conversion flow are unchanged; no second evaluator is introduced.
  MOD adds eight unit-metadata value recipes in `impl_arithmetic.rs`: four
  native integer signedness profiles, one full-i128 legacy profile, native/legacy
  raw-real profiles and a shared exact Decimal profile. Only these new recipes
  require two actual non-NULL inputs at both admission layers. Existing genuine
  NULL/missing recipes remain separate: a successful value-recipe NULL means
  zero divisor, so native callers apply their original handler after consuming
  the computed result; legacy callers keep their silent NULL. No host divisor
  test or shared-context warning is introduced. Native real nonfinite results
  alone authenticate the exact Modulo/FloatOverflow cause; legacy IEEE results
  and original signed remainder quirks remain distinct. Datatype
  `Decimal::try_native_rem` validates native inputs and reuses the private Grow
  remainder loop, preflighting live scratch/output words before allocation.
  This known-word budget is not whole-heap/allocator accounting. The native
  public remainder becomes a thin bridge; DIV/IntDIV, wire behavior and old
  three-operation fast kernels remain unchanged. Existing Decimal2's third
  infrastructure budget slot suffices; no new role, result kind, typed binding
  or driver is added.
  LIKE/ILIKE add five closed recipes in `impl_like.rs`; `native_like.rs` owns
  compiled native patterns, ASCII ILIKE policy and live-cache invocations over
  the existing datatype wildcard engine. The three non-NULL recipes have three
  real Bytes/Bytes/Int inputs and exact typed `NativeLikeCallMetadata`, not an
  invented fourth SQL operand. Native escape is already normalized to u8; wire
  LikeSig keeps its original charset/collation and i64-to-u32 behavior. Actual
  NULL witnesses and missing legacy children have separate one/zero-input
  recipes, all returning the actual owned Int/NULL. Metadata construction shares
  cache handles without lookup; an independent binding guard resolves/compiles
  only inside the generated wrapper and detaches on return or unwind. Owner
  Clone resets while invocation Clone shares; context-only cache keys preserve
  old same-context hits even if incoming pattern bytes change. The observed
  compiled value/Vec capacities are checked without a second cache lookup;
  cache-lock/Arc headers and temporary folds remain outside this known-storage
  accounting, not a physical heap or preallocation/OOM bound. Legacy LIKE uses
  UTF-8-to-empty, the existing Go-simple Unicode leaf and Reject trailing escape,
  distinct from modern Literal trailing escape and ASCII-only ILIKE. Native
  AST/typed/legacy and scan/SHOW callers retain their demand and forward actual
  contexts/errors; pure bool utility/statistics SDK calls claim no worker receipt.
  Ordinary PB admission and unsupported Go vector tiers remain unchanged.
  Regexp algorithms remain shared in `components/tidb_query_expr/src/regexp_policy.rs`;
  `native_regexp.rs` owns the context cache, native compiler and typed causes.
  Cache-owner Clone resets, while explicit invocation handles share the actual
  state. Binding does not initialize or evict anything. Four private native
  recipes in `impl_regexp.rs` lazily compile after the original validation frontier
  and borrow the actual cached Regex/parts, including memoized compile failures.
  Statement-context and constness semantics remain frontend-owned; metadata RAII
  detaches the handles before postflight or pool return, including unwinding.
  These four exact regexp operation/role/kind combinations admit typed metadata
  and real 3/5/6-slot schemas; the three LIKE combinations above are separate
  typed holders, and other evaluated metadata remains unit. The common driver is
  unchanged, and ordinary wire/PB admission is not widened. Invalid INSTR return
  option alone authorizes an undemanded flags marker, never a fabricated NULL.
  Two nullable witnesses, two distinct legacy case policies and one genuine
  zero-operand missing-child recipe complete the nine-operation closed set.
  Legacy children retain their original demand; missing operands are not SQL NULL.
  Typed native SQL causes are authenticated only for the four main operations;
  binding, counting and resource failures never acquire a regexp SQL cause.
  The fixed metadata box is charged with the worker. Use-site observations charge
  known cache structures and actual parts/error-buffer capacities using checked
  sums; cache slots are not re-read after context replacement. Opaque engine/TLS
  storage and allocation peaks remain outside this partial logical accounting,
  including transient uncached engines; full memory accounting is deferred to M6.
  The public statistics helper is a shared pure SDK helper, not a pooled SQL path.
  Native per-expansion UTF-8 versus wire raw bytes, builder versus inline flags,
  diagnostic mapping and original NULL/error precedence remain distinct policies.
  Eight native VECTOR functions use nine private `impl_vec.rs` recipes. The
  aligned native-policy type and complete public helpers live in datatype
  `codec/mysql/vector_native.rs`; the older packed wire type retains its own
  validation, codec and equality contracts. Both views share ordered f32 metric
  loops, with explicit native powi-versus-wire multiplication in one norm loop.
  NativeVector/NativeVector2 roles carry actual VectorFloat32 scalar inputs,
  including raw mutable nonfinite bits, without constructor revalidation.
  FROM_TEXT owns strict UTF-8 and parsing; its actual LE serialized vector result
  is decoded only for layout into ComputedNativeVector, not used as a control
  payload. The result wrapper has bitwise transport Eq while the aligned public
  type keeps its original float PartialEq. Checked retained charges cover layout
  conversion coexistence, not measured allocator peaks or zero-copy performance.
  Exact operation/cause matching authorizes shared VectorError diagnostics for
  FROM_TEXT and the four binary metrics. Frontend conversions remain guarded,
  binary NULL demand remains left-first, and the worker retains NaN-to-NULL and
  infinity policy. No ordinary wire decoder or native PB admission is widened.
  ENCODE/DECODE, TIDB_SHARD/VITESS_HASH and FORMAT_BYTES/FORMAT_NANO_TIME
  use seven private recipes with existing roles and owned results. The pure
  `tidb_query_crypto` leaf owns the normalized MySQL RNG state, historical
  SQLCrypt permutation and original RustCrypto DES Vitess hash. Native utility
  facades and the existing TiKV MySqlRng facade delegate to those leaves;
  clock/seed policy and public utility callers remain intact. Native mathutil's
  mutex and raw seed setters/getters use the same wrapping step without silently
  normalizing their wider state domain. DES is an explicit
  shared dependency, not an OpenSSL/FIPS provider substitution. Crypt preparation
  occurs before capability/guard discovery, preserves data-first NULL demand and
  selects a real NULL witness rather than fabricating password data. Hashing and
  shard modulo execute in the worker; the signed C result retains all unsigned
  SQL bits. Scaled formatting keeps its original unit tables, fixed/scientific
  thresholds and nonfinite behavior, consuming actual IEEE754 bits. No new
  public input/result/report/driver shape or native PB/cop admission is required.
  UUID inspection/conversion and TRANSLATE use nine closed private recipes with
  existing Bytes, BytesInt, Bytes3 and actual-NULL witness roles. Native UUID
  parsing retains its 32/36/45/38-byte spellings and distinct trim/UTF-8 policies;
  wire UUID parsing and unsigned timestamp behavior are not broadened or repaired.
  Native timestamp extraction keeps signed pre-epoch arithmetic and exact six-place
  Decimal results. UUID_TO_BIN first computes actual UUID bytes before demanding
  the optional flag in a second complete call; the extra lease is an explicit cost.
  BIN_TO_UUID retains flag warnings before payload NULL/length checks. Five typed
  causes authorize native SQL errors only for matching sealed calls; an invalid
  binary payload is borrowed from its actual cause, not reconstructed from input.
  UUID host generators share the pure formatter and epoch constant without gaining
  whole-family migration credit. TRANSLATE has separate byte/rune recipes with
  first-duplicate precedence and deletion beyond the replacement length. Frontend
  preparation stops at the first NULL before scope discovery and selects a true
  NULL witness, never fabricated suffix operands; binary result metadata depends
  only on the first argument. No new result kind, driver or PB admission is added.
  DATE_FORMAT/TIME_FORMAT use a shared Time scanner and specifier renderer,
  retaining separate SQL-text, raw-core, wire, elapsed-text and raw-duration
  policies. Trailing percent signs, invalid-month errors, raw large years,
  fractional widths and hour normalization remain policy-specific. Native
  public temporal formatters are thin delegates; weekday/ordinal and duration
  microsecond projections also have one shared owner. The closed core/layout
  role carries a non-NULL LE8 core and an actually nullable layout, not generic
  Bytes2. Actual NULL times use a separate NULL witness without reading layout.
  Legacy DATE_FORMAT boolean demand still examines only the first datum's
  presence through the existing boolean bridge; a genuinely missing child uses
  a private NoArgs integer recipe, without widening ordinary graph admission.
  SQL DATE_FORMAT retains both original coercions. TIME_FORMAT completes a
  duration-text probe before demanding layout in a second complete call;
  repeated parsing and the second lease are explicit costs. LAST_DAY retains
  its distinct strict date/clock parser, not DATE_FORMAT's bad-clock fallback
  or the wire zero-date policy. Legacy formatter SQL errors remain NULL, but
  resource failures are not folded into SQL NULL. No result kind, report or
  PB signature is added by these recipes.
  MAKEDATE/FROM_DAYS share the native wide inverse civil-date helper in Time,
  not the wire bounded unsigned day-number domain. FROM_DAYS returns actual
  canonical date text; the consumer packs the computed zero-date value into
  its original typed zero Time, without classifying the input or reparsing it.
  MakeTimeParts is a distinct three-column role: a nullable nine-byte hour
  packet (LE i64 plus its unsigned-type flag), nullable Int minute, and nullable
  LE8 IEEE seconds.
  The worker returns actual signed total-second bits or NULL. MAKETIME finishes
  that complete driver call before demanding original input precision and
  invoking SecToTimeNative through the existing IEEE-plus-Int role. Its scale
  is actual nonnegative, usize-representable FSP, not restricted to six; NULL
  seconds carry an explicitly undemanded precision. Existing rounding roles
  retain their separate validators. Both time families share one formatter,
  preserving raw non-finite values, negative zero and the original fractional
  carry that does not normalize minutes. No frontend callback reenters the
  driver. First admission can precede undemanded precision; the extra lease and
  transport are explicit costs. Existing typed-result casts remain upstream
  consumer policy; no new wire/PB admission, result kind or report is added.
  WEEK/WEEKOFYEAR/YEARWEEK share one width-preserving week calculation
  with native const and wire helpers, retaining the non-leap week-year zero
  rule, mode normalization and distinct zero-date policies. WEEK/YEARWEEK
  perform two sequential driver calls: a worker validates date text and returns
  that original owned text, then the frontend demands mode coercion before the
  second worker computes the result. No callback recursively enters the driver.
  Invalid dates skip mode coercion; first admission can reject before an as-yet
  undemanded mode error. Repeated parsing and the second lease are explicit
  costs, not performance improvements. Existing PB WeekWithoutMode observed
  NULL uses a real NULL witness without prefix coercion, suffix demand or the
  default-mode getter; legacy uses actual nullable unary raw core bits, mode 0.
  No new argument role or PB signature is introduced. PASSWORD/SM3 private
  recipes delegate to the pure `components/tidb_query_crypto` leaf, also used
  by native parser-auth facades. It owns raw-SHA1 password encoding and the
  previously native incremental SM3 implementation, including Sum's state
  mutation. This is separate from the FIPS shim in `components/crypto`.
  Existing wire PASSWORD's double-hex/lowercase policy is not silently changed;
  native deprecation warnings and coercion remain before worker admission.
  DATEDIFF/TO_DAYS/TO_SECONDS/TIDB_PARSE_TSO_LOGICAL add six private
  Int-result recipes. DATEDIFF keeps separate SQL/PB text and legacy raw-core
  policies. PB's observed NULL uses the existing NullWitness role without
  coercing its prefix or reading its suffix. A new TimeCoreBits2 role carries
  two actual nullable LE8 core values; ordinary Bytes2/Int2 cannot impersonate
  it. Legacy date construction previously did not validate calendar fields,
  so raw zero/invalid fields and clock-independent date differences remain.
  Shared strict clock/datetime parsers preserve full-tail fraction validation
  before truncation; TO_DAYS validates the clock even though it returns days.
  One private day-number macro preserves the original i32 and i64 operation
  widths, order and const use. Native public CoreTime helpers and the existing
  extension day-number helper delegate to it, while wire get_daynr remains
  distinct. Neither widening then casting nor civil-day epoch substitution
  replaces these policies. Logical TSO keeps nonpositive inputs NULL and masks
  positive values' low 18 bits; it is not physical timestamp conversion.
  Results, reports, diagnostics and the common driver remain unchanged.
  DAYOFWEEK/WEEKDAY/DAYOFYEAR/DAYNAME add four private nullable text
  recipes in `impl_time.rs`: three ordinary Int results and one owned Bytes
  result. Frontend casts and string preparation remain separate from the
  worker's full native calendar validation. Year zero is valid when month/day
  are valid; wide native years are not narrowed through packed Time or chrono.
  Shared Time helpers own civil-day arithmetic, Sunday indexing, ordinal-day
  differences and full weekday names. Native calendar helper/formatter leaves
  and weekday Display/name methods delegate to them; CoreTime normalization,
  wire zero-date/chrono policies, inverse civil conversion and abbreviation
  methods remain unchanged. The calendar formatter's previously residual full
  month table also delegates to MONTH_NAMES; this is leaf deduplication, not
  DATE_FORMAT family credit. All four use existing Bytes input roles, with no
  new diagnostic receipts, driver, metadata or native PB/legacy admission.
  PERIOD_ADD/PERIOD_DIFF and GET_FORMAT add four private recipes in
  `impl_time.rs`. The period pair uses existing Int2 inputs and signed Int
  results; both nullable values are prepared before worker validation. Shared
  Time helpers retain distinct ordinary versus wrapping arithmetic, without
  changing wire narrowing or period-zero behavior. Two exact EvaluateError
  variants retain the original 1210 code/message; a failure-only receipt is
  granted only for the corresponding sealed operation and cause after exactly
  one wrapper invocation. Resource, shape and postflight failures cannot become
  period SQL errors. GET_FORMAT keeps byte-exact type selectors, ASCII-only
  case-insensitive locales and empty results for unknown combinations. Its
  two-byte-column recipe handles actual arguments; a separate one-NULL-input
  recipe preserves first-NULL demand without inventing a second argument,
  and rejects non-NULL witnesses. Both return existing ordinary owned Bytes.
  The frontend's special grammar-selector AST retains strict string coercion,
  distinct from scalar raw-byte preparation. No new role, result layout,
  driver, NoArgs case or native PB/legacy admission is introduced.
  MONTHNAME and TIME_TO_SEC use two private text recipes in `impl_time.rs`,
  reusing Bytes inputs and ordinary owned Bytes / signed Int results. Native
  MONTHNAME preserves its ETDatetime cast before string preparation; the worker
  validates the whole calendar date and uses the shared month-name lookup. Wire
  MONTHNAME retains its independent zero-date warning and indexing policies.
  The existing public full MONTH_NAMES table also serves native date formatting
  and parsing, without changing their guards or claiming those whole families;
  abbreviation tables remain separate. `Time::parse_native_duration_text` owns
  the distinct TIME_TO_SEC/TIME_FORMAT policy, including junk-with-no-digits as
  zero, >838 hours as NULL, arbitrary six-character fraction truncation and
  original unchecked arithmetic. It does not use the HMS clamp or construct
  a Duration from nanoseconds. Native TIME_FORMAT keeps its original consumer
  and mask demand through a thin parser delegate. No native PB/legacy admission
  is added. Arithmetic follows the consuming workspace's overflow profile;
  neither debug-assertions flags nor checked-arithmetic substitution infer it.
  HOUR/MINUTE/SECOND use six private recipes in `impl_time.rs`: three
  Bytes-to-Int text recipes and three Int-to-Int signed-nanosecond recipes.
  Native SQL retains its original string conversion, including Duration Display
  and FSP; no ETDuration cast is introduced. Datatype `Time::parse_native_hms`
  owns the original text parser and whole-value 838:59:59 clamp. Its date-prefix
  parser and shared split/pivot/leap/month-length helpers also serve thin native
  calendar delegates, retaining the full u32-year domain. Wire month-length
  handling still returns 31 for invalid months, unlike the native helper's 0.
  Legacy duration calls send actual signed nanoseconds, including NULL, and
  use shared const Duration projections without constructor validation,
  rounding, FSP normalization or SQL-text clamping. These are distinct existing
  domains, not interchangeable Time/Duration representations. Malformed text's
  quiet NULL is now computed after admission; original frontend UTF8/arity
  errors still precede admission. Existing Bytes/Int roles and signed result
  kinds suffice; no role, driver, NoArgs case or ordinary admission is added.
  YEAR/MONTH/DAYOFMONTH/QUARTER use four private core-field recipes in
  `impl_time.rs`. TimeCoreBits is a distinct nullable-u64 logical role, encoded
  as exactly eight little-endian bytes in one physical column; ordinary Bytes,
  IEEE754Bits and Int cannot impersonate it, including NULL. Results reuse signed
  Int. Datatype `Time` owns const raw-core field projections, shared by its
  bitfield getters and native CoreTime getters; quarter uses the same primitive.
  This is not a whole-Time bridge: no strict constructor, packed timestamp
  conversion, calendar validation or new warning policy is introduced. Zero and
  invalid stored fields survive; kind/FSP/clock are not observed by these four
  results. Original wire YEAR/DAYOFMONTH zero-date warnings remain independent.
  The original native ETDatetime casts stay frontend-owned. Only existing MONTH
  PB/legacy paths are connected; no ordinary admission, driver or context getter
  is added.
  JSON_STORAGE_FREE/SIZE reuse the closed JSON Int/error report recipes:
  parse the actual prepared document before returning zero or measured size.
  `native_policy` measures serde values through common `jcodec` container and
  literal-inline layout primitives, without encoding a u16-key binary value;
  string-prefix length comes from the existing varint encoder. JSON_QUOTE
  returns ordinary owned bytes through one shared quote traversal with explicit
  native/wire escape policies. Native standard JSON control escapes and raw
  HTML/U+2028/U+2029 remain distinct from wire's bell/vertical-tab escapes and
  other raw controls. These three entries add no role, kind, driver, NoArgs
  exception or PB admission. Typed-payload size and path-key quote helpers
  remain different APIs, not claimed equivalent to the SQL functions.
  JSON_VALID/TYPE/DEPTH use six private recipes in `impl_json.rs`. The datatype
  `codec/mysql/json/native_policy.rs` owns the existing native serde parser,
  opaque framing and uvarint policy; native and wire representations share one
  type-name selector and one depth recursion/child visitor. The native public
  `BinaryJSON::element_depth` helper also retains its original `to_node`
  validation and uses the narrow `native_json_depth_from_children` adapter;
  it does not keep a local depth/max calculation or assume codec equivalence.
  Native text is not
  re-encoded through binary JSON's u16 object-key limit. Preserve native signed
  i64::MAX preference, duplicate-key/recursion rules, exact-null literal policy,
  strict opaque length checks, and wire's different literal/opaque validation.
  VALID's text/binary/Others signatures return real worker Int results; Others
  alone adds a closed NoArgs whitelist entry and consumes no ignored payload.
  TYPE and DEPTH return `ComputedJsonReport`/`OwnJsonReport`: NULL, owned type
  bytes, inline depth, EmptyText or InvalidText. Only their selected recipes
  decode this canonical result; TYPE copies only payload with existing overlap
  checks, while depth/error states retain no allocation. Resource/transport
  errors never become JSON statuses. Source-type, UTF8 and numeric conversion
  errors remain before admission, but parsing and typed TYPE validation now
  occur inside the worker: zero slots therefore precede invalid JSON results.
  This changed resource-error priority is intentional, not claimed to preserve
  the old preparation order. No new PB admission, role, context getter, driver
  or fourth column is introduced. JSON_LENGTH/path handling remains separate.
  COMPRESS/UNCOMPRESS use two private nullable-Bytes unary recipes. The Go
  encoder has one production owner in `impl_encryption/native_go_flate.rs`;
  native bounded inflation belongs to `impl_encryption.rs`, while wire keeps
  its original zlib/reader policies. COMPRESS returns ordinary owned bytes.
  UNCOMPRESS has a distinct owned completed outcome: NULL, decoded bytes
  (including empty), corrupt input or output-limit failure. Only its sealed
  recipe can decode the canonical internal result envelope; malformed result
  framing remains InvalidBatch, not a SQL warning. The physical encoded output
  stays charged while Value copies its payload with fallible allocation and
  capacity-based overlap checks. Status outcomes retain no payload allocation.
  Native packing appends the original 1259/1258 warnings from that actual
  outcome, never from a resource error or a frontend re-decode. Worker-context
  warnings stay disabled, and no generic graph or driver is added. Narrow pure
  exports support unchanged native fixtures only; production uses the worker.
  EXP/LOG10 add two closed nullable raw-IEEE unary recipes backed by the
  relocated `impl_math/native_go_exp_log.rs` compatibility core. The existing
  FMA and logarithm arithmetic is preserved, not substituted with libm. Wire
  EXP/LOG10 are unchanged. The frontend records EXP's coerced input only for
  error formatting after a computed non-finite result; LOG10 retains its 3020
  warning before admission, sends the actual input bits, then consumes the
  computed result before applying its original domain policy. No new role,
  cause, pure-function export, PB/legacy admission or execution driver is added.
  SIN/COS/TAN/COT/ATAN and the ATAN2 alias add eleven closed raw-IEEE
  unary/binary recipes: six native-Go forms and five existing legacy-libm forms.
  `impl_math/native_go_trig.rs` is the single Go-compatible production owner;
  native golden tests use narrow pure-function exports without retaining a
  second algorithm. This is compatibility-core relocation, not a claim that
  libm has identical Go bit patterns. Wire and legacy share libm primitives but
  keep distinct result policies. Native computed-result finite checking remains
  frontend packing, as for RADIANS/DEGREES; legacy preserves raw NaN/Inf and
  wire retains its Real/error rules. Native ATAN2 prepares both operands even
  when the first is NULL, while legacy left-NULL permits an undemanded right.
  PB's existing first-NULL boundary uses the real NULL-witness recipe. No TAN
  PB/legacy admission, new argument carrier, failure marker or driver is added.
  CHAR adds a distinct packed nullable-i64 list, including zero items, and a
  new single-owner compatibility byte generator (not a pre-existing wire kernel).
  Each integer follows the original signed shift loop for at most four bytes;
  truncating to u32 before trimming would change values above 32 bits. NULL items
  are skipped; an empty/all-NULL list computes non-NULL empty bytes. Charset lookup
  remains in guarded frontend preparation, while decoding, warning1300 and strict
  mode handling consume the computed result in guarded packing.
  CONV adds native text and full-binary-literal BII recipes plus a legacy BBB
  recipe with canonical LE16 i128 bases. Native and legacy reuse the existing
  prefix/parse/radix primitives with explicit sign, base and overflow policies;
  wire wrapping/sign/error behavior stays distinct. Binary literals execute both
  original conversion stages in the kernel without pre-narrowing the payload.
  Legacy ready validation preserves text/from/to demand, retaining actual
  out-of-i64 values rather than fabricating NULL. ConvUnsignedOverflow receipts
  require one of the two sealed native operations, an actual invocation and the
  typed parse-overflow cause. The full sign-stripped digit payload is retained;
  generic resource failures and legacy NULL results are not reclassified by code
  or text. All four new operations use at most three columns and the same driver.
  SPACE/REPEAT/TO_BASE64/FROM_BASE64 add an independent Packet argument role,
  carrying real nullable values plus typed Allow/SuppressByPacket (physical
  non-NULL 0/1). The private wrapper is actually invoked for suppressed NULL;
  native1301 policy remains frontend-owned, never disguised as resource failure.
  REPEAT's ReadyIntArg distinguishes actual nullable Int from Undemanded;
  only RepeatNative + NULL bytes + Allow admits the latter, before invocation,
  then uses a documented irrelevant Some(0), not an evaluated SQL NULL.
  impl_string.rs owns unique repeat/encode/decode cores. Empty REPEAT has a fast
  return without changing wire values. Official Base64 retains its 16MiB,
  six-whitespace and invalid-length-empty policies; native wrappers retain their
  distinct limits, four-whitespace and invalid-length-NULL policies. FROM's
  value-only private entry uses ordinary Bytes without packet/raw-length guards;
  execution capability is independent of this policy. Silent length overflow
  reaches the appropriate kernel with real arguments and Allow, not fake NULL.
  LOWER/UPPER binary variants use their real wire no-op kernels. UTF8 variants
  bind the existing EncodingUtf8Mb4 getters privately: the canonical descriptor
  has empty charset and zero type heap, so invoking the charset-dependent wire
  selector would be incorrect. No Unicode case table or algorithm is added.
  Legacy ASCII LowerAsciiNative/UpperAsciiNative are separate private one-Bytes
  wrappers around the sole byte-ASCII transformations; high octets stay unchanged.
  They are not the wire binary no-ops. Legacy UTF8 reuses Utf8Ready after its own
  Rust grouped-lossy preparation. All four legacy NULL paths invoke the worker.
  SHA2's selector/digest/hex core is shared: wire invalid selectors still append
  warning1583 and return NULL; native private selection returns quiet NULL.
  The renamed ReadyIntArg also serves the independent BytesIntReady role;
  only Sha2Native with NULL bytes permits its undemanded integer representative.
  ORD shares one base256 fold. Wire retains return-collation decoding and NULL0;
  native preparation retains argument-charset behavior, including existing
  byte-preserving latin1. The public facade and both ready matchers validate
  prepared NULL or at most four bytes before invocation, never truncate input.
  TRIM has three native direction recipes over Bytes2. One trim core selects
  independent wire right bounds or native right bounds after removing the left
  prefix. SUBSTRING_INDEX adds ReadyBytesBytesInt: signed/unsigned recipes keep
  raw bits and original native MIN behavior, and use the same find/rfind scanner
  with native forward non-overlapping suffix policy. A genuinely NULL count
  remains NULL; an undemanded count needs checked NULL-string/empty-delimiter
  conditions, not fabricated NULL inputs.
  Four native pad recipes use PadPacket with physical Bytes/Int/Bytes/Int(flag).
  ReadyBytesArg strings are either both evaluated values or both undemanded;
  only NULL count, packet suppression or out-of-range count permits the latter,
  lowered after validation to irrelevant non-NULL empty-byte representatives.
  Zero/valid width still demands both strings. Three closed shape guards admit
  arity four/call one only for these four pad operations, the two INSERT
  operations and `Locate3Native`; two inline arrays hold four slots without
  extra operands on old recipes. Caller compile limits are five nodes only for these seven operations,
  otherwise four, with depth three unchanged.
  One quotient/remainder construction core retains Wire < versus Native <=
  truncation, wire empty-pad-growth NULL versus native empty, and wire UTF8 *4
  versus native character-count limits. Existing wire nonzero equal-length
  empty-pad division and SUBSTRING_INDEX MIN abs behavior are left unchanged;
  this is neither a hidden bug fix nor a new artificial panic. General graph
  admission, the driver, pool and canonical zero-heap metadata are unchanged.
  LN/LOG/LOG2/POW share their sole f64 primitives with wire wrappers; private
  kernels compute raw NaN/Inf/domain bits without wire filtering. Ieee754Bits2
  is distinct from ordinary Bytes2. Only PowNative permits one Undemanded side
  with a genuinely NULL peer, validated before lowering to irrelevant +0 bits;
  two undemanded operands and LOG demand markers are rejected. Native3020 and
  finite-result errors remain frontend policies; legacy POW retains raw NaN/Inf.
  UNCOMPRESSED_LENGTH has one empty/short/full-LE32 core: NULL stays NULL,
  empty/1..4 bytes yield0, and all32 length bits survive. Only wire appends1259;
  the native frontend preserves its own short-header warning, without packet policy.
  INSERT uses real Bytes/Int/Int/Bytes inputs, one range helper and one splice.
  Wire binary uses byte bounds; native UTF8 uses true character boundaries and
  never decodes replacement bytes. Original wire UTF8 strict decoding and its
  character-offset-as-byte-offset behavior remain. Packet refusal follows the
  actual result in the frontend, not a suppression flag or fake NULL input.
- FIELD uses separate prepared Bytes/collation, signedness-preserving integer
  and raw-IEEE domains. A single-candidate matcher can guide frontend coercion
  demand, but the final one-column recipe validates the complete observed prefix
  and computes the first-match index. MAKE_SET has its own ready selection payload;
  its native unchecked-shift selector follows the actual compilation profile,
  while wire retains the original rolling-mask behavior. Total SQL arity includes
  the needle/mask, including existing value-only arity-one calls. EXPORT_SET adds
  a new compatibility core rather than claiming an existing wire implementation:
  three physical columns preserve real NULL witnesses versus undemanded operands,
  omitted defaults, count clamping and the native signed bit63 comparison rule.
  Frontend whole-list NULL prechecks versus strict tuple coercion remain distinct.
- Full-arity CONCAT/CONCAT_WS use a dedicated opaque prepared prefix, not a
  general variadic graph or prejoined SQL result. One physical Bytes input holds
  true SQL arity, each demanded nullable operand and a checked terminal state.
  InputNull preserves the actual NULL slot; PacketExceeded retains the triggering
  value and first observed limit. The frontend owns child demand, coercion,
  packet getters and diagnostics; the backend validates the complete prefix and
  joins once using the same primitive as wire CONCAT/CONCAT_WS. WS packet sizing
  keeps original operand indices even across NULLs; output separators still join
  surviving values. No four-column whitelist expansion is needed.
  ELT's pure selector returns a SQL operand offset from index plus total arity
  including the index operand. It controls conversion of already-eagerly-evaluated
  values, not SQL-child evaluation; the final three-column recipe revalidates the
  chosen/undemanded value. OCT keeps raw64 integer formatting and explicit native
  Unicode-trim versus wire ASCII-trim policies over one decimal-prefix scanner.
- Seven collated-string recipes cover native STRCMP, LOCATE2/3, both extension
  LOCATE3 unit policies, dynamic FIND_IN_SET and prepared-key FIND_IN_SET.
  `NativeSearchPolicy::{Bytes,Utf8(NativeCollation)}` separates units from
  comparison: UTF8 with Binary comparison still returns character positions.
  Native search uses collated fixed-size windows, not wire lowercase/memmem
  semantics; only native3 applies the existing Go simple lowercase for CI.
  Its original unchecked position decrement differs from extension wrapping.
  Only `Locate3Native` adds four actual references/five compile nodes; no general
  graph or callback admission changes. Explicit policy tags are validated data,
  not normalized wire IDs or hidden operand provenance.
  Native FIND uses NoPad key equality, not compare. `prepare_find_in_set_keys`
  produces an opaque shared key snapshot, never a SQL answer. The frontend keeps
  context-once list evaluation, NULL demand, retries and cache invalidation;
  lookup selects its current policy without rebuilding the frozen keys. NULL
  cache alone permits an undemanded needle. An empty non-NULL cache still computes
  the real needle key before its zero result. Prepared payload validation and
  necessary Vec-column copying do not imply preserved HashMap O(1) performance.
  Actual pure-builder errors retain an unattributed phase at the native boundary,
  never a fabricated worker Prepare/Invoke phase or SQL overflow classification.
- SUBSTRING has eight closed Native/Legacy × two/three-argument × Bytes/UTF8
  recipes over one range core. Two arguments mean a true tail, not synthetic MAX:
  Native3 checked overflow yields empty, while legacy keeps its unchecked add.
  Native ready markers require a real NULL peer; the separate Legacy role keeps
  full i128 operands in validated 16-byte little-endian slots, not ordinary Bytes
  recipes. `legacy_substring_needs_len` only asks whether the length child is
  demanded; final admission checks that same predicate and the real kernel owns
  all NULL/empty/value results. Legacy position0/wide rejection, out-of-range
  skip-length and Rust-lossy grouping remain distinct. Only legacy materializes
  a character-unit slice to preserve its natural invalid-range behavior; wire
  and native retain iterator output. No graph/driver/four-column limit changes.
- These closed context-free kernels permit one private UTC/default/zero-detail
  context per created worker. No session/native context or callback is accepted.
  Fixed metadata caches are prewarmed before publication without executing a fake
  NULL call. Owner observation never initializes the cache and includes its Box
  and referenced-offset Vec capacity; canonical heap-free field/function metadata
  is established by construction, not serialization or logical equality.
  Warning count AND details must stay empty. An in-flight sticky state prevents
  reuse after a panic caught elsewhere; dirty/unmeasurable workers are disposed
  before native continuation. Cleanup/health failure cannot overwrite an original
  kernel error. No per-row context recreation, warning drain or silent repair.
- All ready Bytes Vec capacities are summed with checked arithmetic, including
  arguments following NULL, and remain charged through their actual lifetime.
  Result accounting includes Int/Bytes physical output and owned Bytes extraction
  overlap. Worker inline and owned-heap accounting
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

- `codec::collation::native::NativeCollation` owns the explicit native selector
  for compare/key, pattern, COW and capability helpers over existing primitives.
  Sixteen checked tags are policy identities, not registry IDs. Its `is_ci`
  preserves the old seven-identity set; Pinyin remains the old panic stub, not
  newly implemented support. Native global-mode resolution stays frontend-owned.
  DerivedBinary LIKE keeps rune semantics rather than becoming byte LIKE.
- `codec::collation::gb` owns shared GB compare/key and native codec leaves.
  `GbPolicy::{Wire,Native}` preserves existing contracts, not operand provenance.
  Native GB18030 key-only PUA NULs never become comparison bytes; NoPad is explicit.
  The original four wire data images and 2103-pair override table remain canonical.
  Native uses the matching CI weights and 2094-pair subset, excluding nine explicit
  wire-only codes; BIN and codec differences are not inferred away from CI table
  equality. A derived rune index replaces duplicate full mappings. Wire keeps git
  encoding_rs0.8.29; the native compatibility alias pins registry0.8.35.
  TiDB facades/generators consume or verify this owner instead of keeping copied
  tables. From this checkout, run the collation filter:
  `cargo test --locked -p tidb_query_datatype --lib codec::collation:: -- --test-threads=1`.
  Full codec-domain equivalence and the
  unrelated native default-registry assertion are not claimed green.

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

- `components/tidb_query_datatype/src/codec/collation/mod.rs`, `pattern.rs`, and `gb.rs`
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
- Closed in-process ready-argument changes:
  inspect `components/tidb_query_expr/src/local/{batch,compile,mod,tests}.rs`
  and `components/tidb_query_expr/src/types/expr_eval.rs`; check exact operation,
  canonical slot order/types, actual nullable dispatch, all live input capacities
  and extraction overlap. This is a library route, not a read-pool/RPC change.
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
