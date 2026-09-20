# Expression semantic gaps observed from the standalone facade

Observed while embedding TiKV's RPN engine in the TiDB Rust port
(`feat/standalone-expression-coverage`, commit `6806dc4d`). These are
differences between existing TiKV kernels and Go TiDB semantics, or latent
robustness holes. They are recorded so they are not rediscovered and so the
embedder can decide whether to guard, avoid, or fix upstream.

Status legend:

* **guarded** — the standalone facade refuses the shape before a kernel runs;
  the kernel itself is unchanged.
* **avoided** — the embedder keeps the SQL shape on its own implementation.
* **open** — no guard; currently reachable only for shapes the embedder does
  not admit.

None of these were fixed in the kernel: the standalone changes are additive
and the server's `eval_decoded` path is unchanged.


## 1. Verified semantic divergences

| # | Signature / area | TiKV behavior | Go TiDB behavior | Status | Evidence |
| --- | --- | --- | --- | --- | --- |
| 1 | `UuidVersion`, `UuidTimestamp` (`impl_miscellaneous.rs`) | accepts malformed UUID strings and returns a value | raises error 1411 `Incorrect string value ... for function uuid_version` | avoided | enrolled mysql replay: `expression/uuid` diverged only with the engine enabled |
| 2 | `Ord` (`impl_string.rs`) | `ORD(NULL)` returns 0 | returns NULL | guarded | native `test_ord` pins `(None, ..., Some(0))`; the adapter wraps with a leaf-only `IF(StringIsNull(x), NULL, ORD(x))` |
| 3 | `GreatestInt` / `LeastInt` (`impl_compare.rs`) | compares raw `i64`, ignoring the unsigned flag | orders `UInt` values above `i64::MAX` correctly | guarded | differential matrix: `greatest(uint)` / `least(uint)` diverged |
| 4 | `IsIPv4`, `IsIPv6`, `IsIPv4Compat`, `IsIPv4Mapped` (`impl_miscellaneous.rs`) | returns 0 for a NULL input (the kernels return `Some(0)` for `None`) | returns NULL | guarded | new differential fixture `IS_IPV4(NULL)`: native `Null`, engine `Int(0)`; the adapter now wraps with a leaf-only `IF(StringIsNull(x), NULL, kernel(x))` |
| 5 | `FromDays` (`impl_time.rs`) | `FROM_DAYS(1)` yields the zero date `0000-00-00` | returns NULL | avoided | differential fixture: native `Null`, engine `Date(0,0,0)`; valid inputs such as `FROM_DAYS(739000)` agree, so the admission row was changed to `Excluded` |
| 6 | `JsonArrayAppend` (`impl_json.rs`) | appending an array value through a nested path appends the array's *elements* | appends the array itself | avoided | `JSON_ARRAY_APPEND('[1,2,3]', '$[0]', '[9]')`: native `[[1, [9]], 2, 3]`, engine `[[1, 9], 2, 3]`; scalar values through `$`/`$[0]` agree, so the admission row was changed to `Excluded` |

A second class surfaced while adding fixtures: names the engine lowers but the
*Rust* evaluator has no implementation for, so no differential baseline can be
built (`casewhen` — the parser normalizes it to `case`; `position` — the
rewriter emits `locate`; `rlike` — native implements `regexp` only;
`json_memberof` — native implements `JSON_MEMBER_OF`; `isfalse_with_null` —
native implements `isfalse`/`istrue_with_null`; the bare `cast` name — the
rewriter emits `cast_signed`/`cast_decimal`/...). These are native coverage
gaps, not engine divergences, and they do not block the engine.

### Suggested upstream fix

1. Validate the UUID textual form in `uuid_version` / `uuid_timestamp` and
   return the 1411 error, matching `builtin_miscellaneous.go`.
2. Decide whether `ORD(NULL)` should be NULL; if the current 0 is a
   compatibility choice, document it, because it differs from Go.
3. Make the integer comparison mappers use the unsigned-aware comparers that
   already exist for `Eq`/`Lt` (`compare_fn_meta::<UintIntComparer<_>>` style),
   or dispatch `GreatestInt`/`LeastInt` by signedness like `plus_mapper`.
4. Return `None`/NULL from the `is_ipv4`/`is_ipv6`/compat/mapped kernels when
   the argument is NULL, so the embedder's NULL mask can be removed.
5. Decide `FROM_DAYS`'s out-of-range result (zero date versus NULL) and make
   the JSON array-append path append the array value rather than its elements.


## 2. Lazy / eager control flow

TiKV RPN has only `Constant`, `ColumnRef` and `FnCall` nodes and the builder
evaluates children eagerly. Signatures that Go treats as lazy therefore run
their unused branches here:

* `If`, `IfNull`, `Coalesce`, `CaseWhen`, `LogicalAnd`, `LogicalOr`,
  `LogicalXor`, `Elt`, `Field`, `Interval`, `Greatest`, `Least`.
* The 34 dispatcher entries classed as lazy-risk in
  `rust/docs/tikv-expression-coverage.json` (TiDB side), including the
  `AddTime*Null` and `NullTimeDiff` forms that Go answers without evaluating
  arguments at all.

Status: the Tier 1 control/logical families (`If*`, `IfNull*`, `Coalesce*`,
`CaseWhen*`, `LogicalAnd/Or/Xor`) are now dispatched to lazy kernels; the
Tier 2/3 families above remain eager. The facade answers "is this program safe
to admit" with `PreparedExpression::has_lazy_nodes()` (presence) and
`eager_lazy_risk()` (the sound signal: kernel names of lazy-sensitive nodes
dispatched eagerly), so an embedder can admit a non-leaf lazy shape only when
the risk list is empty (see `SHORT_CIRCUIT_DESIGN.md` §11). See issue
[pingcap/tidb#70156](https://github.com/pingcap/tidb/issues/70156): it is an
open proposal whose TiKV work item is "introduce short-circuit expression
nodes whose child expressions are evaluated on demand instead of eagerly"; the
rollout switch it mentions does not exist in either checkout yet.

## 3. JSON semantics

| # | Area | Difference | Status |
| --- | --- | --- | --- |
| 4 | `JsonSet`, `JsonInsert`, `JsonReplace` | SQL `NULL` base becomes JSON `null` instead of SQL NULL | avoided (non-NULL base required) |
| 5 | `JsonMerge` | no deprecation warning on a non-NULL result; `NULL` operands merge as JSON null | avoided (only `json_merge_preserve` with non-NULL typed docs) |
| 6 | `JsonQuote` | emits `\a` / `\v` for control bytes, which is not valid JSON | avoided (immutable, control-free UTF-8 literals only) |
| 7 | JSON binary structure | helper must reject truncated/overlapping offsets before native decode | guarded |
| 8 | Temporal JSON encoding | TiDB embeds bare `CoreTime`; TiKV embeds `Time` with type/FSP low bits | avoided (temporal JSON and casts producing it are refused) |

## 4. Session settings the kernels cannot see

| # | Area | Difference | Status |
| --- | --- | --- | --- |
| 9 | `WeekWithoutMode` | hardcodes mode 0; TiDB uses session `default_week_format` | avoided (only explicit-mode `WEEK` admitted) |
| 10 | `Concat`, `ConcatWs`, `Repeat`, `Space`, `Lpad`, `Rpad`, `Insert`, `ToBase64`, `FromBase64`, `MakeSet` | no `max_allowed_packet` seam, so the engine cannot apply the warning/error/truncation policy | avoided |
| 11 | Current-date `Duration` → date/time casts | need "today", which the engine context does not carry | avoided |

## 5. Robustness holes on valid finite input

| # | Area | Symptom | Status |
| --- | --- | --- | --- |
| 12 | `RoundWithFrac*`, `Truncate*` | `TRUNCATE(0.0, 309)` / `ROUND(1.0, -400)` build `Inf`/`NaN` internally and unwrap `Real::new` | guarded (digit preflight). The preflight must see the digit as an `Int64`/`Uint64` literal or an input column, so a digit that is a computed node -- including the `CastStringAsInt` a rewriter inserts over `round(x, '2')` -- is refused at compile time; pass `TIKV_EXPR_DEBUG_COMPILE=1` to print the refused program |
| 13 | real arithmetic over produced `Inf` | `ROUND(f64::MAX, -308) * 0` panics in `NotNan` | guarded (checked RPN entry) |
| 14 | `VecL2Distance`, `VecL2Norm`, vector inner products | finite inputs overflow to `Inf`, a following node panics | guarded (checked RPN entry) |
| 15 | `CastStringAsReal` | `flen=1, decimal=2` underflows `truncate_f64`; `flen=255` asserts | guarded (Real metadata preflight) |
| 16 | `ToBinary`, `LikeSig` mapper | indexes `children[0]`/`children[1]` before the generated arity validator | guarded (arity preflight) |
| 17 | regexp raw-varg validators | check argument count but not argument types | guarded (type preflight) |
| 18 | `MysqlEnum` literal | indexes `elems[value-1]` unchecked | guarded (bounds preflight) |
| 19 | zero-digit `Decimal` | `digit_bounds` computes `word_count - 1` unchecked; `shift(1)` underflows | guarded (canonicalize zero, reject nonzero inactive words) |

## 6. Not yet investigated

* Collation coverage beyond `binary` / `utf8mb4` / `latin1` / `gbk`.
* `Geometry` / array column types (no native type or codec support). `Set` now
  has a native type, codec and `Column::Set` facade carrier, so identity and
  `CAST(set AS SIGNED)` (the selection bit mask) are covered; Set string
  conversion, comparison and aggregation are not.
* Fixed while wiring `Set`: `validate_expr_return_type` accepted `Enum` in an
  `Int`/`Bytes` parameter slot but not `Set`, so a string kernel called with a
  `Set` column (`LENGTH(set_col)`) failed validation even though the hybrid
  codec supports it. Both carriers are now accepted.
* Fixed in TiDB's suite dispatch: a mandatory-engine context could still run
  the native row-major branch (for example `getvar`). A regression first failed
  because evaluation succeeded; the dispatch now records `NotAdmitted` and
  returns a structured engine error before native user-variable/sequence side
  effects. Optional-engine contexts retain the native path with a refusal
  receipt. Row-major engine execution itself is still unsupported.
* The `eager` interaction between window functions and the expression engine.
  `WindowExec` drains all child rows, but expressions are demanded only at the
  current comparison/target. TiDB partition/order keys now use retained suites
  with a single-row selection at each original left/right demand point. Tests
  assert engine receipts, cache reuse, skipped later-key errors and restoration
  of the dense buffer after errors. FIRST/LAST/NTH_VALUE and LAG/LEAD
  argument/default reads now use the same single-row selection helper, with
  native-versus-engine emission tests for missing targets and skipped defaults.
  RANGE calculation/comparison expressions now use retained per-bound suites at
  the original current/candidate-row demand points too; ascending/descending,
  comparison short-circuit and bound-without-expression paths have engine
  receipts. This removes direct native calls from TiDB's `window.rs`, not the
  suite's feature/admission-gated native paths. A naive whole-chunk result cache
  would surface later-row errors before an earlier frame/key short-circuit;
  avoiding that is a
  semantic-ordering guard, not a performance fallback. The no-wire-change
  per-input-column table (`physical_value` plus an ordered/repeatable
  `logical_rows` slice) is now threaded through `eval_decoded_into`,
  `eval_subtree` and `ChildHandle`; `SelectedColumnRef` exposes the corresponding
  borrowed standalone facade. The legacy shared-selection APIs wrap it, and lazy
  child materialization continues to own its dense boundary value. Independent
  selections can represent join left/right rows without concatenating columns,
  and `eval_borrowed_selected_columns_shared` now accepts a separate physical
  row count per column, validating each selection against its own column.
  Tests cover 1-versus-3-row inputs across batches, repeated/reordered/nullable
  rows, dense-plus-selected inputs and invalid shapes before sink callbacks.
  The old shared-count API remains compatible. Borrowed evaluation still
  rejects lazy programs. TiDB Join CNF now uses suites over the existing row
  cursor with an explicit physical selection, preserving NULL-from-IN and
  per-condition short-circuit; semi-family joiners retain shared programs.
  `JoinExec` datum/index-pair matching and scalar index-hash tasks also retain
  and share condition programs. Ordinary matching preserves NULL-immediate
  rejection (unlike anti-semi CNF continuation), and merge-key reselection
  refreshes the residual cache. Serial chunk-backed and specialized/general
  parallel residual probes now also share that cache at the original candidate
  demand points; Next-loop tests prove engine rows and one compilation across
  task windows. Local index-lookup filters now also retain ordinary-match
  programs shared with fork templates/rebuilt tasks. Tests cover local Next,
  cache sharing, template isolation after filter replacement, and NULL/FALSE
  skipping a later error; the new tests do not open remote cursors. Index-probe
  bounds now also share per-bound programs over existing selected chunk rows.
  Tests pin invalid-key/NULL rejection, evaluation before deduplication, skipped
  overflowing rows, cross-batch cache reuse and recovery after a demanded error.
  The adapter must disable column-swap mode for these calculated-value calls;
  the first regression caught and corrected that construction mistake. Other
  batch/filter work and admitted-path versus native-fallback separation remain.
  Existing joined scratch-row copies still exist, and Join has not yet adopted
  the independent-column facade to eliminate them. Other `eval_bool` callers
  still construct temporary programs. A naive full-chunk cache remains forbidden.
- Aggregate arguments and order keys now use per-expression programs retained
  in TiDB's aggregate input plans, shared by clones/bindings across groups.
  Native/engine tests pin NULL short-circuit, AVG/JSON_OBJECTAGG extra arguments
  before the primary, FIRST_ROW skipping later errors, and physical selections.
  GROUP_CONCAT sort keys after a NULL argument preserve current native behavior;
  this ordering has not been independently verified against Go in this change.
  Typed aggregate kernels are unchanged and do not contribute expression-engine
  row receipts. Window aggregates now retain input plans across frames, while
  each frame gets a fresh accumulator. Emission tests pin one compilation,
  overlapping/empty/FIRST_ROW frames and delayed overflow demand; the existing
  recomputation algorithm remains unchanged. Admission/fallback remains.

- The filtering facades previously bypassed mandatory-engine admission and
  could execute native user-variable reads. A regression failed before the
  fix; engine requests now enter retained/single-expression suites, so required
  row-major refusals happen before native effects and optional refusals use the
  existing fallback diagnostics. Selection retains the programs across child
  chunks and no longer owns duplicate NULL/string-IN kernels. Tests pin engine
  receipts, cache reuse, NULL/false short-circuit and physical-mask intersection
  after evaluation. Non-requesting contexts retain native typed vector kernels;
  other convenience filter callers still need cross-call program retention.

- UnionScan generating expressions now retain programs and borrow the existing
  mutable-row chunk. Each cast, NOT NULL zero substitution and writeback finishes
  before the next dependency; tests pin real Next receipts, reuse, and error
  stopping/rebinding. Post-generation CNF conditions also retain their cache.
  Native cast support and expression fallback remain. Sort merge keys now use
  validated materialized-cell transfer instead of a native Column AST call;
  this is not engine execution, and deferred constants remain unevaluated.

- Late table casts need scalar Datum semantics, not a typed projection carrier.
  During generated/default routing, `0x10` changed from Int(16) to Int(0) after
  losing its BinaryLiteral tag; UnionScan instead raised an IncorrectValue
  error. Red/green tests pin both. `eval_selected_for_cast` shares engine
  admission/error dispatch while preserving native fallback Datum kinds.
  Binary-literal admission is still declined; this is NOT new engine support.
  Required-engine refusals still error and engine errors never replay.
- `ENUM_SET_AS_INT` is a scalar-result metadata contract. Plain Sort cell
  transfer and the first late-cast implementation returned Enum carriers where
  UInt ordinal/bitmask values were required. Separate failing regressions now
  pin ENUM/SET metadata adaptation in both paths; ordinary typed projection
  behavior is unchanged. Sort transfer is not kernel execution.
- General generated/default compatibility helpers now route through the facade,
  but create temporary programs. Statement-owned retention must account for
  schema/name rebinding and zone/LIKE rewrite changes; public mutable column
  descriptors are not safe immutable cache owners. Existing dependency gather
  copies, native conversion support, and optional native fallback remain.

- Pushed-scan filters no longer bypass engine dispatch through local IN/LIKE
  shortcuts. Retained condition programs share compilation across clone/conjoin;
  remapped columns rebuild programs. Tests pin receipts, NULL/FALSE stopping and
  skipped errors. Removing local shortcuts has unmeasured performance risk.
- Boolean coercion needs scalar Datum kinds too: typed output turned binary
  literal 0x10 false. Pushed matching and FilterProgram row/vector regressions
  fail before using the scalar-preserving entry and pass afterward. Binary
  admission remains declined/native; required-engine refusals stay errors.
- Statistics probes now use the same facade, with one shared program across an
  estimate's TopN/bounds/NULL samples. Tests pin seven engine sample rows, cache
  reuse after overflow and missing-engine refusal through erased contexts. Error
  results remain unknown selectivity (the NULL probe retains its special rule),
  not native replay. This is not an engine-only/native-deletion claim.

- UPDATE scalar/physical inputs and the expression after DML Apply now share
  retained programs, but bind fresh row data per execution. Tests pin cache reuse,
  physical selection, missing physical input and overflow recovery. Scalar
  assignment results preserve binary-literal kinds; admission is still declined
  for those literals. Apply evaluation order is unchanged.
- Sparse partition dependencies previously bypassed mandatory engine dispatch;
  empty dependency rows could panic through the dense helper. Red/green tests
  cover both, plus integer conversion and engine errors. Partition helpers now
  use the scalar facade with original indexes and explicit virtual empty rows.
  Programs remain temporary across bounds. Neither change removes native
  fallback or proves engine-only coverage.
- The general eval_row_values helper also had incorrect operand remapping
  (-7 instead of 7), empty-row handling and caller-native fallback requests.
  It now preserves original indexes and shares scalar-preserving dispatch with
  eval_constant_row. Its compatible Option API returns Some on success and errors
  directly in both feature modes. Binary literal kinds survive subsequent casts;
  no additional binary-literal engine admission is claimed.
- Explicit INSERT VALUES now dispatch through prepared suites in original
  row-major order after explicit default preparation. SQL tests pin execution
  receipts, binary assignment and overflow stopping before later rows/writes.
  Program ownership is insert-local; performance and cross-statement retention
  are not established.

- Nonzero DATE/DATETIME constants and typed NULLs are now admitted locally
  when Datum kind/FSP match the declared metadata and physical fields fit the
  bridge. Tests pin existing MysqlTime packed bytes and 132 required-engine rows
  across offsets, SQL modes and transports, including partial/invalid-calendar
  dates and nested YEAR. This exposes existing TiKV support, not new wire types.
  Packed zero is now allowed only after warning-free compilation:
  Time::from_packed_u64 validates it under SQL mode and may warn/error, unlike
  native constant transfer. Restrictive profiles still compile-decline; warnings
  never reach the host, even when max_warning_count is zero. Tests cover DATE and
  DATETIME(0/6), mode changes on retained programs, permissive engine receipts,
  optional native fallback and required structured refusal. Admission counters
  include required-mode declines, not only actual native execution. TIMESTAMP
  is now admitted only for proven UTC compile contexts: absent/empty name with
  offset zero, or exact named UTC (name takes precedence over offset). The
  context-free encoder is unchanged; non-UTC/unknown aliases still decline.
  Tests cover TIMESTAMP(0/3/6), NULL, nested YEAR, both transports and retained
  programs across timezone changes (60 engine rows), plus zero-mode policies,
  wire metadata and kind/invalid-calendar rejection. Kind/FSP and physical-shape
  restrictions remain; non-UTC/DST transport is not yet unified.

## 7. Found by dual-running the Go source-port corpus

The TiDB adapter now evaluates every constant expression in the 33 source-port
test files natively AND through the engine and requires agreement, so these
surfaced automatically rather than silently:

| # | Area | Native | Engine | Handling |
| --- | --- | --- | --- | --- |
| 20 | `COT` | `0.6420926159343308` (`0x1.48c05d04e1cfep-1`) | `0.6420926159343306` (`0x1.48c05d04e1cfdp-1`) | the ENGINE is the one that differs. Go's `COT` is `1/math.Tan(x)` (`pkg/expression/builtin_math.go`, `builtinCotSig.evalReal`), and Go's pure-Go `math.Tan(1)` is `0x1.5574077246549021p+0` while libm's is `0x1.5574077246549023p+0`, one ULP above, so the reciprocal lands one ULP below Go. This port's `go_trig::go_tan` is a transcreation of Go's `Tan` and its `COT` is Go-exact, so `cot` stays native for the correct answer; matching Go in the engine would need Go's `Tan`, not libm's. Pinned by `math_fn::tests::cot_matches_go_and_libm_tan_does_not` |
| 21 | `CRC32` | `Datum::UInt(2501908538)` | `Datum::Int(2501908538)` | FIXED: the TiDB port declared CRC32 signed while Go marks it UNSIGNED; the declaration now carries `UNSIGNED|BINARY` and `crc32` is admitted |
| 22 | `OCT` over a binary literal | reads the bit value (`b'11111111'` -> `377`) | takes the string path (`0`) | `oct` excluded |
| 23 | `GREATEST`/`LEAST` over a non-binary collation | folds case/accents through the derived collation | compares bytes (`utf8mb4_general_ci`: native `B`, engine `a`) | string shapes require binary arguments |
| 24 | `FIND_IN_SET` over a non-binary collation | collation- and padding-aware (`2`) | bytewise (`1`) | string shapes require binary arguments |
| 25 | `LAST_DAY` over an implicit temporal cast | `2024-03-31` | `ExternalEngine` error "unsupported TiKV temporal value shape" | FIXED: the kernel returns `DateTime` kind for a DATE-declared result; the TiDB bridge now rebuilds the declared DATE the way Go's DATE decoder drops the time part, so the implicit cast, `last_day`, `date` and `month` are admitted |
| 26 | binary/bit literal in a numeric context | numeric (`b'1' + 0` -> 1) | bytes (`0`) | those constants declined |
| 27 | `ROUND`/`TRUNCATE` with a computed digit | `round(5, -100)` is `0`, `round(1.2345, '2')` is `1.23` | compile refusal "ROUND/TRUNCATE fractional digits require an integer literal or input column" | the row-12 panic guard needs the digit as a literal or column; the rewriter's constant is not folded and `-100` is `unaryminus(100)`, so these stay native. An embedder-side constant fold of the digit, or a runtime digit check inside the kernel, is the way in |

Item 20 is worth an upstream look for the opposite reason the older text
claimed: the engine's answer is the one that differs from Go, by one ULP, and
the fix would be to use Go's `Tan` rather than the system libm. Items 23-24 are
worth an upstream look too: they are a collation-semantics difference, not a
wording one. Items 21 and 26 are representation choices the
adapter could carry differently if the enum-style hybrid were extended to
binary literals.
