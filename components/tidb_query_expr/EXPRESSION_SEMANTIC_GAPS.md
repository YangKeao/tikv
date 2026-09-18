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
* The `eager` interaction between window functions and the expression engine.

## 7. Found by dual-running the Go source-port corpus

The TiDB adapter now evaluates every constant expression in the 33 source-port
test files natively AND through the engine and requires agreement, so these
surfaced automatically rather than silently:

| # | Area | Native | Engine | Handling |
| --- | --- | --- | --- | --- |
| 20 | `COT` | `0.6420926159343308` | `0.6420926159343306` (Go's `math.Cot`) | native port bug; `cot` stays native until the port is fixed |
| 21 | `CRC32` | `Datum::UInt(2501908538)` | `Datum::Int(2501908538)` | FIXED: the TiDB port declared CRC32 signed while Go marks it UNSIGNED; the declaration now carries `UNSIGNED|BINARY` and `crc32` is admitted |
| 22 | `OCT` over a binary literal | reads the bit value (`b'11111111'` -> `377`) | takes the string path (`0`) | `oct` excluded |
| 23 | `GREATEST`/`LEAST` over a non-binary collation | folds case/accents through the derived collation | compares bytes (`utf8mb4_general_ci`: native `B`, engine `a`) | string shapes require binary arguments |
| 24 | `FIND_IN_SET` over a non-binary collation | collation- and padding-aware (`2`) | bytewise (`1`) | string shapes require binary arguments |
| 25 | `LAST_DAY` over an implicit temporal cast | `2024-03-31` | `ExternalEngine` error "unsupported TiKV temporal value shape" | FIXED: the kernel returns `DateTime` kind for a DATE-declared result; the TiDB bridge now rebuilds the declared DATE the way Go's DATE decoder drops the time part, so the implicit cast, `last_day`, `date` and `month` are admitted |
| 26 | binary/bit literal in a numeric context | numeric (`b'1' + 0` -> 1) | bytes (`0`) | those constants declined |
| 27 | `ROUND`/`TRUNCATE` with a computed digit | `round(5, -100)` is `0`, `round(1.2345, '2')` is `1.23` | compile refusal "ROUND/TRUNCATE fractional digits require an integer literal or input column" | the row-12 panic guard needs the digit as a literal or column; the rewriter's constant is not folded and `-100` is `unaryminus(100)`, so these stay native. An embedder-side constant fold of the digit, or a runtime digit check inside the kernel, is the way in |

Items 23-24 are worth an upstream look: they are a collation-semantics
difference, not a wording one. Items 21 and 26 are representation choices the
adapter could carry differently if the enum-style hybrid were extended to
binary literals.
