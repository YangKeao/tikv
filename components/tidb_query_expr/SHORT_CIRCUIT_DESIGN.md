# Signature-driven lazy evaluation for RPN (short-circuit design)

Status: design only. No wire change; `tipb` is untouched. All references are to
this checkout at `f62bcfc6a1b136b5d4967673d3cbb803fa303861`
(`components/tidb_query_expr/...` unless stated otherwise).

Goal: make `IF`, `CASE`, `COALESCE`, `IFNULL`, `AND`, `OR` (and the other
Go-lazy signatures in §5) evaluate only the children they need, so a skipped
branch cannot raise an error, record a warning, or draw RNG state. The engine
must select laziness from the **signature** it already mapped through
`map_expr_node_to_rpn_func` (`lib.rs:436`), not from a new node kind.

Upstream context: `pingcap/tidb#70156` proposes new short-circuit wire nodes and
a switch; it is unimplemented. This design keeps `ExprType`/`ScalarFuncSig`
frozen and adds the lazy path inside `tidb_query_expr`, matching milestone C of
`../tidb/rust/docs/tikv-expression-removal-execplan.md:187`.

---

## 1. Where children are evaluated today

There is no recursive evaluator: the tree is flattened into a post-order node
array at build time, and evaluation is a single flat pass over that array.

* **Build (the real recursion).** `append_rpn_nodes_recursively`
  (`types/expr_builder.rs:251-271`) visits children first (`:325-327`) and then
  pushes the parent `FnCall` (`:328-333`). `handle_node_fn_call` (`:296-335`)
  maps the signature (`:307`), runs `func_meta.validator_ptr` (`:310`),
  constructs metadata with `func_meta.metadata_expr_ptr` (`:319`), and records
  `args_len` (`:321`). RPN `[A, B, C, Foo]` is therefore "children already on
  the stack when `Foo` runs".
* **Column decoding.** `RpnExpression::eval` (`types/expr_eval.rs:205-225`)
  first calls `ensure_columns_decoded` (`:229-246`), which loops over *all*
  `ColumnRef` nodes and decodes each referenced column, then calls
  `eval_decoded` (`:218`). `eval_decoded` (`:264-279`) delegates to
  `eval_decoded_impl::<false>` (`:272`); the standalone checked entry
  `eval_decoded_with_finite_reals` (`:285-300`) delegates to the same impl with
  `CHECK_FINITE_REALS = true` (`:293`).
* **The evaluation loop.** `eval_decoded_impl::<CHECK_FINITE_REALS>`
  (`:302-382`):
  * `assert!(output_rows > 0 && output_rows <= BATCH_MAX_SIZE)` (`:310-311`);
    `stack = Vec::with_capacity(self.len())` (`:312`).
  * `for node in self.as_ref()` (`:314`) walks every node exactly once.
  * `Constant` pushes `Scalar` (`:316-318`); `ColumnRef` pushes
    `Vector { Ref { physical_value: input_physical_columns[offset].decoded(),
    logical_rows: input_logical_rows } }` (`:319-330`) and asserts
    `input_logical_rows.len() == output_rows` (`:322`).
  * `FnCall` (`:331-360`): `stack_slice_begin = stack.len() - args_len`
    (`:342-344`), then
    `(func_meta.fn_ptr)(ctx, output_rows, stack_slice, &mut call_extra,
    &**metadata)?` (`:346-352`), truncate, push the result as
    `Generated { physical_value: ret }` (`:353-359`).
  * After every node, when `CHECK_FINITE_REALS`, the produced node's REALs are
    rejected if nonfinite (`:362-377`).
  * `assert_eq!(stack.len(), 1)` (`:380`).

**What a lazy variant must replace.**

1. `func_meta.fn_ptr` cannot be the only execution hook. A lazy signature must
   receive *unevaluated* children plus a way to request a specific child for a
   specific subset of rows.
2. The flat loop must stop evaluating a lazy call's child subtrees. The
   children occupy the contiguous RPN ranges immediately before the `FnCall`
   node (`args_len` roots `r_N = fc-1`, `r_{k-1} = subtree_start(r_k)-1`).
   `subtree_start(i)` is derivable from `args_len` alone; no new node field and
   no wire change are needed.
3. The finite-REAL check (`:362-377`) must run for every node the lazy
   recursion *actually* executes, including a child materialized for a lazy
   kernel (which never becomes a stack node), and must **not** run for skipped
   children.

**`BATCH_MAX_SIZE` and decoded columns.**

* `BATCH_MAX_SIZE = 1024` (`tidb_query_datatype/src/codec/data_type/logical_rows.rs:5`).
  The batch split happens outside the evaluator: the standalone facade chunks
  `step_by(BATCH_MAX_SIZE)` (`standalone.rs:414-432`); executors chunk in their
  runners. Lazy child requests are indices into the *current* call's rows
  (`0..output_rows`), so a child's `output_rows = positions.len() <=
  output_rows <= BATCH_MAX_SIZE`; the `:310` assert holds. A branch is never
  deferred across batches — a row that needs its branch is evaluated in the
  batch that contains the row.
* `LogicalRows` (`logical_rows.rs:30-33`) cannot represent a subset:
  `Identical { size }` is dense and `as_slice` panics for `size >=
  BATCH_MAX_SIZE` (`:49-59`). Subset selections must be `LogicalRows::Ref`
  slices; a lazy child's dense output is exactly `Generated`, whose
  `logical_rows_struct()` is synthetically `Identical { len }`
  (`types/expr_eval.rs:87-95`). Use `logical_rows_struct().get_idx` (`:62`) /
  `get_logical_scalar_ref` (`:180`), never `logical_rows()`/`as_slice()` on a
  possibly-1024-row generated vector (latent panic at `logical_rows.rs:52`).
* Decoded columns: `ColumnRef` nodes hold `&VectorValue` into the decoded
  `LazyBatchColumnVec` (`:321`). `ensure_columns_decoded` stays eager over all
  referenced columns; that is a decode cost (and a decode-error surface) for
  columns that only a skipped branch uses, but it is not SQL expression
  evaluation. Lazy *decoding* would need `&mut LazyBatchColumnVec` (or
  interior mutability) inside the handle and is explicitly out of scope. The
  handle only needs the shared `&LazyBatchColumnVec`, exactly what
  `eval_decoded` already passes (`:268`); the `eval`/`eval_decoded` split at
  `:217-218` is unchanged.

---

## 2. Minimal lazy dispatch design

### 2.1 The lazy signature marker

Add one optional function pointer to `RpnFnMeta`
(`types/function.rs:36-61`), next to the existing
`borrowed_fn_ptr: Option<super::borrowed::BorrowedFn>` (`:48`):

```rust
pub type LazyFn = fn(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    extra: &mut RpnFnCallExtra<'_>,
    metadata: &(dyn Any + Send),
) -> Result<VectorValue>;

pub struct RpnFnMeta {
    pub name: &'static str,
    pub validator_ptr: fn(expr: &Expr) -> Result<()>,
    pub metadata_expr_ptr: fn(expr: &mut Expr) -> Result<Box<dyn Any + Send>>,
    pub borrowed_fn_ptr: Option<super::borrowed::BorrowedFn>,
    /// `Some` iff the signature must see unevaluated children.
    pub lazy_fn_ptr: Option<LazyFn>,
    pub fn_ptr: fn(...) -> Result<VectorValue>,   // unchanged eager kernel
}

impl RpnFnMeta {
    pub const fn with_lazy(mut self, f: LazyFn) -> Self { self.lazy_fn_ptr = Some(f); self }
}
```

`RpnFnMeta` stays `Copy` (function pointers are `Copy`; `#[derive(Clone, Copy)]`
at `:35`). The dispatcher in `lib.rs` changes one token per lazy signature:

```rust
ScalarFuncSig::IfInt => if_condition_fn_meta::<Int>().with_lazy(lazy_if::<Int>),
```

This keeps `name`, `validator_ptr` and `metadata_expr_ptr` from the generated
meta, so build-time validation (`expr_builder.rs:310`) and metadata construction
(`:319`) are unchanged.

**Alternatives rejected.**

* *Separate lazy meta table keyed by `ScalarFuncSig` inside the evaluator.*
  `RpnExpressionNode::FnCall` stores `func_meta` but **not** the signature
  (`types/expr.rs:12-30`), so the evaluator cannot key a table without adding a
  field to the node or matching on `fn_ptr` pointer identity (fragile).
* *New `RpnFnMeta` variant (enum).* `RpnFnMeta` is a `Copy` value used
  everywhere (codegen `const fn` constructors, `Debug` at `function.rs:63`,
  `fn_call_func` at `expr.rs:68`) and `handle_node_fn_call` destructures the
  fields directly (`expr_builder.rs:307-333`). An enum forces every one of
  those sites plus the codegen to handle two shapes for no benefit.
* *New RPN node kind / wire type.* Forbidden: `tipb` is a separate repository.

**Codegen impact.** Three production sites construct the struct literal:
`rpn_function.rs:1199` (varg constructor, `generate_constructor` at `:1043`),
`:1341` (raw-varg constructor, `:1261`), `:1817` (normal constructor, `:1761`).
Add `lazy_fn_ptr: None,` to each. The codegen's own token-snapshot tests
(`#[cfg(test)]` around `:1975`, `:2010`, `:2187`) assert generated source text
and must be updated for the new line.

### 2.2 The interface lazy kernels get

Pull model: the kernel drives, because only the kernel knows the branch logic
(`CASE` pairs, `ELT`'s index → child mapping, `FIELD`'s first-match scan).

```rust
/// Access to the unevaluated children of one lazy call.
pub trait LazyChildren<'a> {
    fn len(&self) -> usize;
    /// Child result/argument field type (Constant/FnCall node type, or
    /// schema[offset] for a ColumnRef). Borrows the compiled program.
    fn field_type(&self, arg: usize) -> &FieldType;
    /// Fast path: a Constant child needs no evaluation.
    fn scalar_value(&self, arg: usize) -> Option<&ScalarValue>;
    /// Evaluate child `arg` over exactly `positions`; `positions` are indices
    /// into this call's row space (`0..output_rows`). Returns a dense owned
    /// vector of `positions.len()` elements in the same order.
    fn eval(&mut self, ctx: &mut EvalContext, arg: usize, positions: &[usize])
        -> Result<VectorValue>;
}
```

* Only the selected rows are evaluated; a skipped child is never entered, so no
  error, warning, or RNG draw is attributed to it.
* The result is **owned and dense**, which intentionally side-steps the
  lifetime problem in §6.2: no `RpnStackNode` may borrow a temporary selection.
* `eval` with empty `positions` returns an empty typed vector without calling
  the subtree (the `output_rows > 0` assert at `expr_eval.rs:310` forbids a
  zero-row recursive call); kernels should still skip the call when the set is
  empty.
* Each `(arg, positions)` pair is evaluated at most once per call; kernels must
  not re-request a child (double warnings/RNG).

### 2.3 Evaluator change (recursive subtree walk)

Replace the flat loop body with a subtree evaluator. Public signatures
(`eval`, `eval_decoded`, `eval_decoded_with_finite_reals`) are unchanged.

```rust
impl RpnExpression {
    fn eval_subtree<'x, const CHECK: bool>(
        &'x self, root: usize, ctx: &mut EvalContext, schema: &'x [FieldType],
        cols: &'x LazyBatchColumnVec, logical_rows: &'x [usize], output_rows: usize,
        scratch: &mut Vec<RpnStackNode<'x>>,
    ) -> Result<RpnStackNode<'x>> {
        assert!(output_rows > 0 && output_rows <= BATCH_MAX_SIZE);
        match &self[root] {
            Constant { value, field_type } => Ok(RpnStackNode::Scalar { value, field_type }),
            ColumnRef { offset } => {
                assert_eq!(logical_rows.len(), output_rows);
                Ok(RpnStackNode::Vector {
                    value: RpnStackNodeVectorValue::Ref {
                        physical_value: cols[*offset].decoded(), logical_rows,
                    },
                    field_type: &schema[*offset],
                })
            }
            FnCall { func_meta, args_len, field_type, metadata } => {
                let roots = self.child_roots(root, *args_len); // see below
                let mut extra = RpnFnCallExtra { ret_field_type: field_type };
                let ret = match func_meta.lazy_fn_ptr {
                    Some(lazy) => {
                        let mut children = ChildHandle::<CHECK> {
                            expr: self, schema, cols, logical_rows, roots: &roots,
                            scratch: Vec::new(),
                        };
                        (lazy)(ctx, output_rows, &mut children, &mut extra, &**metadata)?
                    }
                    None => {
                        let base = scratch.len();
                        for &r in &roots {
                            let n = self.eval_subtree::<CHECK>(r, ctx, schema, cols, logical_rows, output_rows, scratch)?;
                            scratch.push(n);
                        }
                        let ret = (func_meta.fn_ptr)(ctx, output_rows, &scratch[base..], &mut extra, &**metadata)?;
                        scratch.truncate(base);
                        ret
                    }
                };
                let node = RpnStackNode::Vector {
                    value: RpnStackNodeVectorValue::Generated { physical_value: ret },
                    field_type,
                };
                if CHECK { ensure_finite_real_node(&node, output_rows)?; }
                Ok(node)
            }
        }
    }

    /// Node index of each child subtree, in argument order.
    fn child_roots(&self, fc: usize, args_len: usize) -> Vec<usize> {
        let mut roots = vec![0; args_len];
        let mut r = fc - 1;
        for k in (0..args_len).rev() {
            roots[k] = r;
            if k > 0 { r = self.subtree_start(r) - 1; }
        }
        roots
    }

    /// First node index of the subtree whose last node is `root`.
    fn subtree_start(&self, root: usize) -> usize {
        let mut pending = 1usize;
        let mut i = root;
        loop {
            pending -= 1;
            if let RpnExpressionNode::FnCall { args_len, .. } = &self[i] { pending += *args_len; }
            if pending == 0 { return i; }
            i -= 1;
        }
    }
}
```

`child_roots` is O(size of the lazy call's subtree), paid once per lazy call per
batch; it is negligible next to evaluating that subtree, and it avoids adding
an in-memory plan field to `RpnExpressionNode` (which is also reachable for
mutation via `DerefMut`/`AsMut`, `types/expr.rs:100-122`, so a cached plan
would need invalidation). If profiling shows it, cache it in `RpnExpression`
at construction (`From<Vec<RpnExpressionNode>>`, `expr.rs:106-110`).

`ChildHandle` converts to owned values:

```rust
impl<'e, 's, const CHECK: bool> LazyChildren<'s> for ChildHandle<'e, 's, CHECK> {
    fn eval(&mut self, ctx, arg, positions) -> Result<VectorValue> {
        let ft = self.child_field_type(arg);
        if positions.is_empty() {
            return Ok(VectorValue::with_capacity(0, ft_eval_type(ft))); // vector.rs:32
        }
        self.scratch.clear();
        self.scratch.extend(positions.iter().map(|&p| self.logical_rows[p]));
        // A lazy boundary materializes immediately, so the child's own node
        // stack is local to this call and never escapes.
        let mut local_stack: Vec<RpnStackNode<'_>> = Vec::new();
        let node = self.expr.eval_subtree::<CHECK>(
            self.roots[arg], ctx, self.schema, self.cols,
            &self.scratch, positions.len(), &mut local_stack)?;
        match node {
            RpnStackNode::Scalar { value, .. } => Ok(VectorValue::from_scalar(value, positions.len())), // vector.rs:42
            RpnStackNode::Vector { value, .. } => {
                let v = value.take_vector_value()?;                 // expr_eval.rs:164 -> :48-84
                if CHECK { ensure_finite_real_vector(&v, positions.len())?; }
                Ok(v)
            }
        }
    }
}
```

`take_vector_value` (`expr_eval.rs:48-84`) already produces exactly the dense
copy needed for a `Ref` node; a `Generated` child (nested `FnCall`) is passed
through. The scratch stack in `eval_subtree` keeps eager-only expressions at
one allocation per `eval` (today's `Vec::with_capacity(self.len())`,
`expr_eval.rs:312`); lazy children allocate one small `Vec<usize>` + their own
scratch.

**Rollout switch.** `RpnFnMeta` always carries both pointers, so eager behavior
is one `if` away. Give the entry a mode (`Lazy`/`Eager`) or simply report
capability: `RpnExpression::has_lazy_nodes()` (scan for
`lazy_fn_ptr.is_some()`) surfaced as `PreparedExpression::supports_lazy()`. The
TiDB-side session variable from #70156 can then gate admission of non-leaf lazy
shapes without any wire change; the engine itself defaults to lazy once tested.

---

## 3. Row selection and three-valued-logic merge

### 3.1 The model

"Vector of rows" is the only evaluator mode in `tidb_query_expr`: a call runs
with `output_rows = logical_rows.len()`, where `logical_rows` are physical
indices into the batch (`expr_eval.rs:210-211`, `:268`, `:322`). The
"row-at-a-time path" is the same code with `output_rows == 1` and
`logical_rows == &[0]`, which is what `RpnFnScalarEvaluator::evaluate_raw`
does (`types/test_util.rs:163`), and what the generated `#[rpn_fn]` kernels
loop over internally (`rpn_function.rs:1588-1652`). So laziness is implemented
once in terms of selections; `k == 1` falls out and needs no second code path.

Positions are indices in `0..output_rows`; the handle maps
`position -> physical = logical_rows[position]`. The lazy result is `Generated`
and therefore dense/identity over the call's rows, so downstream consumers see
an ordinary vector and nothing else in the engine changes.

### 3.2 AND / OR

Truth table (0 = false, 1 = true, N = NULL), matching the existing eager kernels
(`impl_op.rs:9-27`):

```
AND | 0  1  N        OR | 0  1  N
 0  | 0  0  0         0  | 0  1  N
 1  | 0  1  N         1  | 1  1  1
 N  | 0  N  N         N  | N  1  N
```

```rust
// AND(lhs, rhs); both Int, "true" = != 0.
let lhs = children.eval(ctx, 0, all)?;                  // all rows
let mut out: Vec<Option<i64>> = vec![None; n];
let mut need = Vec::new();
for p in 0..n {
    match int_at(&lhs, p) {
        Some(0) => out[p] = Some(0),                    // rhs not needed
        _ => { need.push(p); out[p] = None; }           // lhs is 1 or NULL
    }
}
if !need.is_empty() {
    let rhs = children.eval(ctx, 1, &need)?;            // rhss skipped entirely when needed==[]
    for (j, &p) in need.iter().enumerate() {
        out[p] = match (int_at(&lhs, p), int_at(&rhs, j)) {
            (_, Some(0)) => Some(0),
            (Some(_), Some(_)) => Some(1),              // lhs was 1, rhs nonzero
            _ => None,                                  // any NULL
        };
    }
}
```

For OR evaluate `rhs` only over `need = { p | lhs[p] != Some(1) }`; rows with
`lhs == Some(1)` are `Some(1)`; for the rest, `rhs == Some(1) -> Some(1)`,
`rhs == Some(_0) -> lhs == Some(0) ? Some(0) : None`, `rhs == None -> None`.

`XOR` is also lazy in Go: `builtinLogicXorSig.evalInt` returns on a NULL lhs
without touching rhs (`pkg/expression/builtin_op.go:224-230`), so evaluate
`rhs` only over `{ p | lhs[p].is_some() }` and coalesce to `None` for the rest.

Notes.
* `need` is a row set; RPN child order is preserved because the handle receives
  positions and returns values in that order.
* If a child is a Constant (`children.scalar_value(0).is_some()`), read it
  directly: `AND(0, x)` / `OR(1, x)` then never request child 1 at all.
* Go's *vectorized* AND/OR is not itself selection-based: it evaluates arg1 for
  all rows and falls back to its row loop on error or warning
  (`pkg/expression/builtin_op_vec.go:368` and `:76`). Our design is strictly
  closer to the row semantics because it never enters unnecessary rows.
* Output is constructed as `Vec<Option<T>>` and converted with
  `ChunkedVec::from_vec` + `cast_chunk_into_vector_value`
  (`data_type/mod.rs:244`, `:224`); `ChunkedVec` has no random-access `set`
  (`:242-255`).

### 3.3 IF / IFNULL / COALESCE / CASE

All four are "evaluate selector, then a subset", differing only in the subset
and merge rule. Let `p` range over `0..output_rows`.

| Function | Selector pass | Then | Merge |
| --- | --- | --- | --- |
| `IF(c,a,b)` (`impl_control.rs:90`) | `c` over all rows | `a` over `{p: c[p]!=0}`; `b` over `{p: c[p]==0 or NULL}` (Go: `isNull \|\| condition==0` -> false branch, `builtin_control.go:769-779`) | copy `a`/`b` per position |
| `IFNULL(a,b)` (`:9`) | `a` over all rows | `b` over `{p: a[p] is NULL}` | `a` where non-NULL else `b` |
| `COALESCE(a0..an)` (`impl_compare.rs:241`, `:252`, `:263`) | `a0` over all | for k=1.. : `ak` over rows still NULL | first non-NULL wins; remaining stay NULL |
| `CASE` (`impl_control.rs:36`, `:54`, `:72`) | `cond_k` over rows still undecided, pairs in order | `res_k` over `{p: cond_k[p]!=0}` | mark those rows done with `res_k`; after all pairs, if `args_len` is odd evaluate the else over the remainder (Go: `builtin_control.go:407-429`); leftover -> NULL |

`COALESCE` stops requesting args once its remaining set is empty; `CASE` stops
at the first true condition per row and never evaluates later conditions for
that row. NULL conditions are false (not three-valued) in all four, matching
Go.

Concrete `IF` sketch:

```rust
let cond = children.eval(ctx, 0, all)?;
let (mut t_pos, mut f_pos) = (Vec::new(), Vec::new());
for p in 0..n { if int_at(&cond, p).map_or(false, |v| v != 0) { t_pos.push(p) } else { f_pos.push(p) } }
let tv = if t_pos.is_empty() { None } else { Some(children.eval(ctx, 1, &t_pos)?) };
let fv = if f_pos.is_empty() { None } else { Some(children.eval(ctx, 2, &f_pos)?) };
// scatter into out[p] per position; None branch result means the branch was empty
```

For `CASE` the else arm is `children.len() - 1` when `args_len % 2 == 1`; the
generated validator `case_when_validator` (`impl_control.rs:130-140`) still runs
at build time and enforces the same chunked types.

---

## 4. Deliberate non-lazy choices

* `LogicalXor` is lazy only in the "arg0 IS NULL" sense (skip rhs). Included, it
  is two lines.
* `INTERVAL` (`impl_compare.rs:286`, `:338`) is lazy only when arg0 is NULL (Go
  returns -1 without evaluating the list). Args `1..` are all needed otherwise
  (`binary_search`; Go `binSearch`/`linearSearch`), so the only win is the
  NULL-target rows. Phase 2.
* `GREATEST`/`LEAST` are lazy in Go because they stop at the first NULL
  (`builtin_compare.go:596-612`: arg0 NULL returns immediately, each later NULL
  returns immediately). TiKV's `do_get_extremum` (`impl_compare.rs:523-548`)
  also returns NULL on any NULL but only after visiting prior args, so a later
  arg can error/parse-fail before the NULL is seen. A lazy kernel keeps a
  per-row running extremum and an active set, dropping rows at their first NULL.
  The `_cmp_string_as_date/time` variants (`:354`, `:390`, `:427`, `:463`)
  additionally call `ctx.handle_invalid_time_error` per element, so skipping is
  observable in warnings.
* `ELT` (`impl_string.rs:693`) and `FIELD` (`:623`, `:637`) are per-row
  *dynamic* child selection, not just prefix skipping:
  * `ELT`: evaluate child 0 for all rows; for each k in `1..n` evaluate child k
    over `{p: idx[p]==k}`; rows with `idx < 1 || idx >= n` are NULL and evaluate
    nothing (Go `builtin_string.go:3345`). This is the case that forces
    `LazyChildren::eval(arg, positions)` rather than a fixed child order.
  * `FIELD`: evaluate child 0; for k=1.. evaluate child k over rows not yet
    matched, mark matches (`i64(k)`), stop when none remain. Uses the same
    equality as the eager kernel (`PartialEq` / `Collator::sort_compare`).
* `MakeSet` is not lazy in Go; keep eager.

---

## 5. Initial lazy set

Every entry below is an existing `lib.rs` arm; the eager kernel stays as the
fallback and the lazy kernel is attached with `.with_lazy(...)`.

**Tier 1 — required for `AND`/`OR`/`IF`/`IFNULL`/`COALESCE`/`CASE`.**

| Group | `lib.rs` | Kernels | Notes |
| --- | --- | --- | --- |
| `LogicalAnd`, `LogicalOr` | `:776-777` | `impl_op.rs:9`, `:19` | selection set + 3VL merge |
| `LogicalXor` | `:778` | `impl_op.rs:31` | skip rhs when lhs NULL |
| `IfInt/Real/Decimal/Time/String/Duration/Json` | `:623-629` | `impl_control.rs:90`, `:104`, `:118` | one of two branches |
| `IfNullInt/Real/Decimal/Time/String/Duration/Json` | `:616-622` | `impl_control.rs:9`, `:18`, `:27` | rhs only where lhs NULL |
| `CoalesceInt/Real/Decimal/Time/String/Duration/Json` | `:600-606` | `impl_compare.rs:241`, `:252`, `:263` | varg; stop per row |
| `CaseWhenInt/Real/Decimal/Time/String/Duration/Json` | `:630-636` | `impl_control.rs:36`, `:54`, `:72` | raw varg; paired children |

Tier 1 is exactly the 30 non-time entries the TiDB coverage report flags
`lazy_children_eager_rpn` (`../tidb/rust/docs/tikv-expression-coverage.json`,
`engine_lazy_risk: 34`), minus `LogicalXor` (not in that list) plus nothing.

**Tier 2 — Go-lazy signatures not needed by the six constructs above.**

| Group | `lib.rs` | Kernels |
| --- | --- | --- |
| `Elt` | `:837` | `impl_string.rs:693` (`elt_validator` `:709`) |
| `FieldInt/FieldReal/FieldString` | `:834-836` | `impl_string.rs:623`, `:637` |
| `Greatest*` / `Least*` (Int/Real/Decimal/String/Time/Date/CmpStringAsDate/CmpStringAsTime/Duration) | `:540-558` | `impl_compare.rs:274-334`, `:354`, `:390`, `:427`, `:463`, `:500-518` |
| `IntervalInt`, `IntervalReal` | `:550`, `:559` | `impl_compare.rs:286`, `:338` |

**Tier 3 — null-shortcut time signatures.**

* `AddTimeDateTimeNull`, `AddTimeDurationNull`, `AddTimeStringNull`
  (`lib.rs:877-879`, `impl_time.rs:405`, `:411`, `:417`) are `#[rpn_fn()]`
  non-nullable kernels that always return NULL but are *entered after* their
  children were evaluated by the flat loop (`expr_eval.rs:346`), so an error in
  a child aborts where Go returns NULL. Their Go bodies never touch `b.args`
  (`pkg/expression/builtin_time.go:4983`, and the `builtinAddTime*NullSig`
  declarations). Lazy replacement: return an all-NULL
  `VectorValue` of the declared ret type and never request a child. Keep the
  generated validator (two children typed `DateTime, DateTime`), which matches
  Go's sig selection for `AddTime(datetime, datetime)`.
* `NullTimeDiff` (`lib.rs:873`, `impl_time.rs:320`) is **already lazy**: its
  TiKV meta is 0-arity, so there are no children to skip (test at
  `impl_time.rs:2516` passes no params). Nothing to do unless the wire is later
  shown to carry Go's 2-arg `NullTimeDiff` (`builtin_time.go:477-486` builds it
  with `arg0Tp, arg1Tp`); in that case the lazy meta accepts 2 children and
  ignores them, with a validator change only.

Precedence rule: attach a lazy kernel only where the Go row evaluator can skip
a child. Everything else stays on `fn_ptr`.

---

## 6. Error and warning contract

1. **Skipped means not entered.** A child is evaluated only through
   `LazyChildren::eval`; if no `eval` call names it, its subtree is never
   walked, so no kernel runs, `ctx.warnings` is untouched, and
   `ctx.handle_invalid_time_error` is not called. Static enforcement: the flat
   loop no longer visits child subtrees; there is no other path from the
   evaluator to a node's `fn_ptr` except the recursive `eval_subtree`, which is
   only reached from `eval` requests.
2. **Needed rows still error.** A child request evaluates the child for exactly
   the requested rows; if the kernel returns `Err`, `?` propagates and the whole
   batch fails. Partial output is discarded by the caller (the standalone
   facade documents this at `standalone.rs:385` and discards via `?` at `:426`).
   This matches Go: the error belongs to a row that needed the branch, so Go's
   row loop errors at that row too. It differs from Go's *vectorized* AND/OR
   only in that Go would first evaluate the branch for undetermined rows and
   then retry row-wise on error (`builtin_op_vec.go:368`), which can mask an
   error that our design never raises — ours is the stricter, correct behavior.
3. **Warnings are per evaluated row.** Warning counts and order now reflect
   only evaluated branches; this is the point of the change and matches Go's
   per-row laziness. It is observable output, so every lazy kernel needs a
   "skipped branch records no warning" test.
4. **No retry across implementations.** Do not implement laziness by running the
   eager kernel and re-running lazily on error: warnings/RNG/locks are not
   replayable (execplan decision log,
   `../tidb/rust/docs/tikv-expression-removal-execplan.md:95`).
5. **`CHECK_FINITE_REALS`.** The checked entry
   (`eval_decoded_with_finite_reals`, `expr_eval.rs:285`) must reject a
   nonfinite REAL produced by any *executed* node, including a materialized lazy
   child (the child check lives in `ChildHandle::eval` because the child never
   becomes a stack node). A skipped branch's nonfinite output must not error —
   that is the desired MySQL behavior and a required test
   (`IF(0, <overflow>, 7)` under `eval_decoded_with_finite_reals`).

---

## 7. Risks

1. **`metadata_expr_ptr` and `validator_ptr` are still eager at build.**
   `handle_node_fn_call` validates (`expr_builder.rs:310`) and builds metadata
   (`:319`) for every node, including nodes under a lazy call. That is
   intentional and required: field types, metadata and arity must be known
   before evaluation, and Go type-checks the whole tree at planning time. Two
   consequences: (a) lazy metas must keep the generated validator/metadata so
   compile-time errors do not move to run time; (b) `handle_node_constant`
   (`:338-395`) decodes constants eagerly, and
   `extract_scalar_value_date_time` (`:450-463`) can push warnings through
   `ctx` at build time even for a skipped branch. That is plan-time behavior,
   not per-row evaluation; document it and do not try to defer constants.
2. **`RpnStackNode` lifetimes.** `RpnStackNodeVectorValue::Ref`
   (`expr_eval.rs:27-36`) carries `physical_value: &'a VectorValue` and
   `logical_rows: &'a [usize]` under **one** lifetime, and `RpnStackNode<'a>`
   (`:107-121`) ties the node to the expression, schema and columns (`:205-212`).
   A subset selection is a locally built `Vec<usize>`, so a child's `Ref` node
   cannot be returned from the handle. Invariant: `LazyChildren::eval` returns
   an owned dense `VectorValue`; `eval_subtree` visits that child with a local
   selection and materializes before the selection drops
   (`take_vector_value`, `:48-84`). Never add a `Ref`/selection-owning variant
   to the public node types without an arena; the recursion must stay
   "materialize at each lazy boundary".
3. **`DerefMut`/`AsMut` on `RpnExpression`** (`types/expr.rs:100-122`) means the
   node vector is mutable after build. Any cached `child_roots` plan must be
   invalidated or recomputed; the design computes ranges per call for this
   reason.
4. **`BATCH_MAX_SIZE` traps.** `output_rows > 0` assert (`:310`); empty position
   sets must short-circuit in the handle. `logical_rows.rs:52` panics for an
   `Identical` slice of size `>= BATCH_MAX_SIZE`, so lazy code must not call
   `RpnStackNodeVectorValue::logical_rows()`/`as_slice()` on a generated vector;
   use `logical_rows_struct().get_idx` (`:62`) or `get_logical_scalar_ref`
   (`:180`).
5. **Column pool / scratch reuse.** `VectorValue` results are freshly allocated
   per call (no pool) and the evaluator's stack (`:312`) is local, so a lazy
   callback cannot alias them. The thread-local `VARG_PARAM_BUF*` buffers
   (`function.rs:342-357`) are reused by generated varg/raw-varg kernels: a lazy
   kernel must not hold a `&[Option<&T>]` or `&[ScalarValueRef]` derived from
   them across an `eval` call, or a nested child kernel will overwrite it. The
   hand-written lazy kernels in `impl_control.rs`/`impl_op.rs` must evaluate
   first and read after. `EvalContext` is `&mut` and threaded through
   `LazyChildren::eval`, so warning order stays deterministic.
6. **Checked-Real path.** `CHECK_FINITE_REALS` cannot be a generic parameter of
   `LazyFn` (it is a plain `fn` pointer), so the const generic stays on
   `eval_subtree` and the handle, and the handle performs the child-side check.
   Extract the current node check (`:362-377`) into one helper used by both
   places. The standalone input preflight (`standalone/safety.rs`,
   `finite_at` in `types/borrowed.rs:87`) still validates all referenced input
   columns, including columns used only by a skipped branch — an over-rejection
   that is pre-existing and independent of laziness.
7. **Borrowed path.** Lazy metas must keep `borrowed_fn_ptr: None`, so
   `PreparedExpression::supports_borrowed` (`standalone/borrowed.rs:33-49`)
   returns false and callers fall back to the copying path. Otherwise
   `eval_borrowed` (`types/borrowed.rs:168-206`) would eagerly evaluate children
   and `expect("borrowed function support validated")` (`:198-200`) would look
   for a loader that does not exist. Add a `debug_assert!(lazy_fn_ptr.is_none())`
   at the top of `eval_borrowed`, or implement a parallel lazy hook there later.
8. **Nondeterministic kernels.** Skipping `RAND*`, `UUID`, `SYSDATE*` in a
   skipped branch changes the number of draws / current-time reads versus the
   eager engine, which is the intended MySQL behavior but requires the host
   capability trait before such a branch can be *taken* (execplan milestone C,
   `:197-212`).
9. **Performance.** Lazy evaluation trades kernel-local vectorization for
   selection bookkeeping: one `Vec<usize>` per request and extra `VectorValue`
   materialization per lazy boundary. Keep the eager path's single scratch
   stack; only build a fresh selection when the requested set is not already
   contiguous/full; skip empty sets. Control functions are not the throughput
   hot path, and the eager kernels remain available behind the switch.

---

## 8. Implementation order and tests

Each step is additive; no step removes an eager kernel, so the rollout switch
can always revert to eager.

**Step 0 — baseline.** `cargo test -p tidb_query_expr --lib`. Record counts.

**Step 1 — infrastructure, zero lazy registrations.** Add `LazyFn`,
`LazyChildren`, `RpnFnMeta::lazy_fn_ptr` + `with_lazy`, update the three codegen
literals and the codegen token tests; replace `eval_decoded_impl`'s loop with
`eval_subtree`/`child_roots`; extract `ensure_finite_real_*`. With every
`lazy_fn_ptr == None` the behavior is identical, so the entire existing crate
suite is the test (including the finite-Real tests and
`types/test_util.rs`-based kernel tests). Smallest reviewable change.

**Step 2 — first lazy signature, `IfNullInt`.** Implement
`lazy_if_null::<Int>` and register
`if_null_fn_meta::<Int>().with_lazy(lazy_if_null::<Int>)`. Tests (in
`impl_control.rs` tests or `expr_eval.rs` tests):
* `IFNULL(1, err)` where `err` is a node that fails only when run. Use
  `UnaryMinusInt(i64::MIN)` (`impl_op.rs:90-101` returns
  `Error::overflow`). Build with `ExprDefBuilder::scalar_func` +
  `RpnExpressionBuilder::build_from_expr_tree`, eval with `output_rows = 1`:
  expect `Some(1)` and no error; with `IFNULL(NULL, err)` expect `Err`. This
  pins both "skipped is not entered" and "needed rows still error".
* Warning variant: the skipped child is a lossy cast in a non-strict
  `EvalContext`; assert `ctx.warnings.warning_cnt == 0` for the skipped case and
  `> 0` for the needed case.
* Differential: for all `(lhs, rhs)` in the existing `test_if_null` cases
  (`impl_control.rs:150-165`), the lazy result equals the eager result.

**Step 3 — `If`, then `Coalesce`, then `CaseWhen`.** Same test shape as step 2,
plus a nested-lazy test (`IF(1, IF(0, err, 7), err) -> 7`, works because
`eval_subtree` recurses for the inner lazy node) and a multi-row selection test
using `LazyBatchColumn` columns and a non-identity `input_logical_rows` (proves
position→physical mapping).

**Step 4 — `LogicalAnd`, `LogicalOr`, `LogicalXor`.** Table-driven 3VL test over
columns containing `{0,1,NULL}` (9 rows) compared against hand-computed MySQL
3VL, evaluated both with a dense selection and with a permuted/reduced
`logical_rows`. Error-skip test: `AND(Col0, UnaryMinus(Col2))` with
`Col0 = [0, 1]`, `Col2 = [i64::MIN, 1]` → `[Some(0), Some(1)]` with no error;
flip `Col0 = [1, 0]` → `Err`. Also assert no warning is produced for the
skipped row.

**Step 5 — `Elt`, `Field*`, `Greatest*`/`Least*`, `Interval*`.** `Elt` needs a
many-child test proving only the selected child runs: `ELT(1, ok, err)` →
`ok`; `ELT(2, ok, err)` → `Err`; `ELT(0, err, err)` → `NULL` with no error.
`Field` first-match test with a later erroring child. `Greatest`/`Least` NULL-
prefix test with a later erroring child and a warning-recording
`CmpStringAsDate` element.

**Step 6 — null-shortcut time signatures.** `AddTime*Null` lazy kernels return
all-NULL and never evaluate children: `AddTimeDateTimeNull(err, err) -> NULL`.
Keep the existing `impl_time.rs:2782-2815` tests green (they push constants).
Decide `NullTimeDiff`'s arity separately (see §5, Tier 3).

**Step 7 — facade and switch.** Add `RpnExpression::has_lazy_nodes()` /
`PreparedExpression::supports_lazy()`; assert
`supports_borrowed() == false` for a lazy program in
`standalone/borrowed/tests.rs`; add a standalone test that
`IF(0, <overflowing real expression>, 7)` succeeds under
`eval_decoded_with_finite_reals` (`standalone.rs:426`) with zero warnings. Add a
test-only eager mode and re-run every lazy test in eager mode to prove the
fallback.

**Acceptance (milestone C wording).** `IF(0, <overflow>, 7)` and
`IF(1, 1, <overflow>)` both return the taken branch with no error and no
warning; an unused branch containing a nondeterministic/erroring kernel does not
run; `AND`/`OR` agree with the eager kernels on all non-error rows and with Go's
3VL on all rows; `RpnExpression` still contains only
`Constant`/`ColumnRef`/`FnCall`, and no `tipb`/protobuf file is modified.

---

## 9. Non-goals

* No wire/`tipb` change and no dependency on the #70156 switch existing.
* No lazy column decoding (`ensure_columns_decoded` stays eager).
* No lazy support in the borrowed standalone path; it refuses lazy programs
  until a borrowed lazy hook exists.
* No change to the public `eval`/`eval_decoded`/`eval_decoded_with_finite_reals`
  signatures.
* No new type support (e.g. `CoalesceVectorFloat32`/`GreatestVectorFloat32`,
  which TiKV does not dispatch today).

---

## 10. Corrections applied during implementation

Steps 1-2 (lazy plumbing plus `IfNullInt`) found two errors in this design.
They are recorded here so the remaining steps do not repeat them; the code and
its tests are the authority.

1. **`child_roots` underflow for nullary calls.** The sketch's
   `let mut r = fc - 1;` runs before the loop, so an `args_len == 0` call
   underflows `usize`. Sixteen existing tests failed with `attempt to subtract
   with overflow` (nullary kernels such as `pi`, `rand`, `uuid`, the
   `*_any_value` family, `json_array`/`json_object`, `coalesce`,
   `null_time_diff`, and the standalone coverage tests). Fixed with an early
   return when `args_len == 0`.

2. **The finite-Real check must cover every executed node, not just calls.**
   The sketch places `CHECK_FINITE_REALS` in the `FnCall` branch, but the old
   flat loop also checked `Constant` and `ColumnRef` nodes. Keeping the check
   only in the call branch would have changed step 1's behavior. Every executed
   node is checked, a materialized lazy child is re-checked, and a skipped
   branch is never checked because it is never entered.

Two smaller corrections: the design cites an `eval_decoded_with_finite_reals`
that no longer exists after the shareable-program change (it is
`eval_decoded_with_finite_reals_into`, taking a caller-owned RPN stack), and
its `LazyFn` metadata parameter omits `+ Sync`, which `RpnFnMeta` now requires.
The irregular-program rejection (`subtree_start(len-1) == 0`) is preserved,
because the recursive walk would otherwise ignore unused nodes.
