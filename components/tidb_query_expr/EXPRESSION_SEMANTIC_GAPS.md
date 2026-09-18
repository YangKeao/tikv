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

### Suggested upstream fix

1. Validate the UUID textual form in `uuid_version` / `uuid_timestamp` and
   return the 1411 error, matching `builtin_miscellaneous.go`.
2. Decide whether `ORD(NULL)` should be NULL; if the current 0 is a
   compatibility choice, document it, because it differs from Go.
3. Make the integer comparison mappers use the unsigned-aware comparers that
   already exist for `Eq`/`Lt` (`compare_fn_meta::<UintIntComparer<_>>` style),
   or dispatch `GreatestInt`/`LeastInt` by signedness like `plus_mapper`.


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

Status: **guarded** by the embedder (only leaf arguments are admitted), but the
engine itself is still eager. See issue
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
| 12 | `RoundWithFrac*`, `Truncate*` | `TRUNCATE(0.0, 309)` / `ROUND(1.0, -400)` build `Inf`/`NaN` internally and unwrap `Real::new` | guarded (digit preflight) |
| 13 | real arithmetic over produced `Inf` | `ROUND(f64::MAX, -308) * 0` panics in `NotNan` | guarded (checked RPN entry) |
| 14 | `VecL2Distance`, `VecL2Norm`, vector inner products | finite inputs overflow to `Inf`, a following node panics | guarded (checked RPN entry) |
| 15 | `CastStringAsReal` | `flen=1, decimal=2` underflows `truncate_f64`; `flen=255` asserts | guarded (Real metadata preflight) |
| 16 | `ToBinary`, `LikeSig` mapper | indexes `children[0]`/`children[1]` before the generated arity validator | guarded (arity preflight) |
| 17 | regexp raw-varg validators | check argument count but not argument types | guarded (type preflight) |
| 18 | `MysqlEnum` literal | indexes `elems[value-1]` unchecked | guarded (bounds preflight) |
| 19 | zero-digit `Decimal` | `digit_bounds` computes `word_count - 1` unchecked; `shift(1)` underflows | guarded (canonicalize zero, reject nonzero inactive words) |

## 6. Not yet investigated

* Collation coverage beyond `binary` / `utf8mb4` / `latin1` / `gbk`.
* `Set` / `Geometry` / array column types (no native type or codec support).
* The `eager` interaction between window functions and the expression engine.
