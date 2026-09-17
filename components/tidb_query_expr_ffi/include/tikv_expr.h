/* Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0. */
#ifndef TIKV_EXPR_H
#define TIKV_EXPR_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define TIKV_EXPR_ABI_VERSION 1u
#define TIKV_EXPR_OK 0u
#define TIKV_EXPR_INVALID_ARGUMENT 1u
#define TIKV_EXPR_COMPILE_ERROR 2u
#define TIKV_EXPR_EVAL_ERROR 3u
#define TIKV_EXPR_PANIC 4u
#define TIKV_EXPR_POISONED 5u
#define TIKV_EXPR_INT64 1u
#define TIKV_EXPR_FLOAT64 2u
#define TIKV_EXPR_BYTES 3u

typedef struct tikv_expr_program tikv_expr_program;
typedef struct tikv_expr_result tikv_expr_result;
typedef struct tikv_expr_error tikv_expr_error;

typedef struct tikv_expr_bytes {
    const uint8_t *data;
    size_t len;
} tikv_expr_bytes;

typedef struct tikv_expr_context {
    uint32_t struct_size;
    uint32_t abi_version;
    uint64_t flags;
    uint64_t sql_mode;
    tikv_expr_bytes timezone_name; /* UTF-8, empty means use offset. */
    int64_t timezone_offset;      /* Seconds east of UTC. */
    uint32_t div_precision_increment;
    uint32_t max_warning_count;
} tikv_expr_context;

typedef struct tikv_expr_column {
    uint32_t struct_size;
    uint32_t type;
    size_t len;
    const uint8_t *nulls;         /* NULL = all valid; otherwise len bytes, 1=NULL. */
    const int64_t *ints;          /* Required only for INT64; unsigned uses bits. */
    const double *reals;          /* Required only for FLOAT64. */
    const tikv_expr_bytes *strings; /* Required only for BYTES, len row slices. */
} tikv_expr_column;

typedef struct tikv_expr_selection {
    const size_t *indices;
    size_t len;
} tikv_expr_selection;

typedef struct tikv_expr_warning {
    int32_t mysql_code;
    tikv_expr_bytes message;      /* UTF-8, not NUL terminated. */
} tikv_expr_warning;

typedef struct tikv_expr_result_view {
    tikv_expr_column column;      /* Dense selection order, repeated rows retained. */
    const tikv_expr_warning *warnings;
    size_t warnings_len;          /* Retained details, bounded by context cap. */
    size_t warning_count;         /* Total occurrences, including dropped details. */
} tikv_expr_result_view;

typedef struct tikv_expr_error_view {
    uint32_t status;
    int32_t mysql_code;           /* Native engine code; 0 for ABI/panic errors. */
    tikv_expr_bytes message;      /* UTF-8, not NUL terminated. */
} tikv_expr_error_view;

/* ABI v1 is native-endian/native-size_t, same architecture as the host.
 * Every descriptor and buffer must be live, accessible, correctly aligned and
 * immutable for the call. Numeric buffers and slice arrays contain len entries,
 * including NULL rows. Unused typed pointers MUST be NULL. nulls entries are 0/1.
 * Each non-NULL string row is a length-delimited binary slice (no terminator).
 * NULL + zero length is valid for buffers; a zero-length pointer is not read.
 * String payloads for NULL/unselected rows are not read. Non-NULL row slice
 * address/length shape is checked even when unselected. Live storage remains the
 * caller's obligation: size/alignment checks cannot prove pointer validity.
 * All sizes/multiplications/address additions must fit isize_t/size_t.
 * Invalid/dangling pointers, races, stale/wrong handles and double frees are UB,
 * NOT errors that catch_unwind can catch. Never pass C++/Rust objects by value.
 *
 * Output slots must be valid, writable, correctly aligned and disjoint from all
 * inputs and each other. compile/eval set *out=NULL on failure. An optional
 * out_error is initialized to NULL and receives an owned error on failure;
 * callers must free an old handle before reusing a slot. No TLS error state.
 * Handles belong to this loaded library instance; free with its matching API.
 * Program access is exclusive (including free); result/error views are immutable
 * and may be read concurrently, but may not race their free. Unload only after
 * all handles are freed and all calls have finished. NULL frees are no-ops.
 *
 * Only Rust unwinding is contained (requires panic=unwind); OOM, aborts and
 * invalid memory access are not recoverable. An eval panic poisons its program.
 * No evaluation error/panic permits transparent fallback to another evaluator.
 */
uint32_t tikv_expr_abi_version(void);
uint32_t tikv_expr_context_default(tikv_expr_context *out);

/* expr/schema elements are serialized tipb Expr/FieldType messages. Context
 * is required; obtain defaults above. struct_size/version must match exactly.
 * Metadata is owned after return. Only Int64/Float64/Bytes and NULL are admitted
 * at every expression node. The original copying facade validates SQL metadata.
 */
uint32_t tikv_expr_compile(tikv_expr_bytes expr,
                           const tikv_expr_bytes *schema, size_t schema_len,
                           const tikv_expr_context *context,
                           tikv_expr_program **out, tikv_expr_error **out_error);

/* NULL selection = all physical rows; non-NULL {NULL,0} = no rows.
 * All column.len values must equal row_count. Indices may repeat/reorder.
 * The ABI gathers selected rows into original OWNED facade Columns, then calls
 * unchanged PreparedExpression::eval with dense logical rows and no selection.
 * The facade copies again to/from engine vectors. No caller payload survives.
 * Zero-column constant expressions use row_count (or selection.len) to broadcast.
 */
uint32_t tikv_expr_eval(tikv_expr_program *program,
                        const tikv_expr_column *columns, size_t columns_len,
                        size_t row_count, const tikv_expr_selection *selection,
                        tikv_expr_result **out, tikv_expr_error **out_error);

/* Views borrow handle-owned memory, valid until its free; no per-value FFI calls.
 * Required out slots are zeroed on recoverable invalid-argument failure.
 */
uint32_t tikv_expr_result_get_view(const tikv_expr_result *result,
                                   tikv_expr_result_view *out);
uint32_t tikv_expr_error_get_view(const tikv_expr_error *error,
                                  tikv_expr_error_view *out);
void tikv_expr_program_free(tikv_expr_program *program);
void tikv_expr_result_free(tikv_expr_result *result);
void tikv_expr_error_free(tikv_expr_error *error);

/* Additive borrowed ABI: resolve/query this symbol BEFORE passing new structs.
 * The copying ABI above remains version 1 and unchanged. */
#define TIKV_EXPR_BORROWED_ABI_VERSION 1u
#define TIKV_EXPR_UINT8 4u  /* Output only: checked Int value 0 or 1. */
#define TIKV_EXPR_UINT64 5u /* Output only: checked nonnegative Int value. */
#define TIKV_EXPR_BROADCAST 1u
uint32_t tikv_expr_borrowed_abi_version(void);
/* Returns 1 for supported original borrowed kernels with numeric output; 0
 * otherwise (including NULL/poisoned program). Same exclusive handle contract. */
uint32_t tikv_expr_program_supports_borrowed(const tikv_expr_program *program);

typedef struct tikv_expr_borrowed_column {
    uint32_t struct_size;
    uint32_t type;
    uint32_t flags;
    uint32_t reserved;             /* Must be zero. */
    size_t len;                    /* Stored rows: row_count, or 1 if BROADCAST. */
    const uint8_t *nulls;           /* NULL=no nulls; ANY nonzero byte=NULL. */
    size_t nulls_len;               /* 0 if nulls=NULL, otherwise exactly len. */
    const int64_t *ints;            /* len native aligned entries for INT64. */
    const double *reals;            /* len native aligned entries for FLOAT64. */
    const uint8_t *chars;           /* Native ColumnString storage, no packing. */
    size_t chars_len;
    const uint64_t *offsets;        /* len END offsets, including terminal NUL. */
    size_t offsets_len;
} tikv_expr_borrowed_column;

typedef struct tikv_expr_borrowed_output {
    uint32_t struct_size;
    uint32_t type;
    size_t capacity;               /* Writable numeric ELEMENTS, not bytes. */
    uint8_t *nulls;                /* Required scratch map; receives only 0/1. */
    size_t nulls_capacity;          /* Writable bytes. */
    int64_t *ints;
    double *reals;
    uint8_t *uint8s;
    uint64_t *uint64s;
} tikv_expr_borrowed_output;

typedef struct tikv_expr_diagnostics tikv_expr_diagnostics;
typedef struct tikv_expr_diagnostics_view {
    const tikv_expr_warning *warnings;
    size_t warnings_len;
    size_t warning_count;
} tikv_expr_diagnostics_view;

/* Borrowed ABI v1: structs must have exactly the declared struct_size; unknown
 * flags/reserved bits are rejected. Input type is INT64/FLOAT64/BYTES. Unused
 * typed pointers and associated lengths MUST be zero. Native byte null maps
 * (not validity bitmaps) and single-row broadcasts require no conversion.
 * BROADCAST requires len=1 even for zero logical rows, and maps every selected
 * physical index to stored row 0. Selection indices still must be <row_count.
 *
 * BYTES: offsets_len=len; offsets are strictly increasing, each <=chars_len,
 * final offset equals chars_len (zero stored rows require chars_len=0), and
 * chars[offset-1] is NUL for EVERY stored row, including NULL/unselected rows.
 * The first row starts at 0; later rows start at the preceding END offset.
 * Exactly the final NUL is excluded from the borrowed value. Empty strings and
 * embedded NULs are supported. Descriptors, offsets and terminators are validated
 * for the entire layout; only selected non-NULL REAL values must be finite.
 *
 * Output type must match compiled numeric output, except INT64 may target the
 * checked UINT8/UINT64 sinks above. Byte output is rejected before kernels.
 * Both capacities must be >=logical output rows (selection.len, or row_count).
 * Only the corresponding numeric pointer is non-NULL. NULL+zero capacity is
 * permitted. All output numeric rows, including NULL rows, receive a value
 * (zero for NULL); output nulls are canonical 0/1. Even nonnullable callers must
 * supply the null map: this initial API retains O(rows) caller scratch storage.
 *
 * All counts, byte-size products and address additions must fit size_t/isize_t.
 * Typed arrays must be naturally aligned, live, accessible and initialized for
 * their declared lengths; writable output capacity must denote actual storage.
 * Zero-length buffers are not read. All input memory remains immutable for the
 * call; no foreign pointer is retained. Output ranges must be disjoint from each
 * other and all input/descriptor/selection ranges; payload overlaps are rejected.
 * Handles and output handle slots must additionally be valid and nonaliased
 * with every other object/buffer; that remains a caller obligation.
 *
 * Layouts, types, selection, capacities, overlaps and selected REAL domains are
 * validated before any kernel or payload write. out/out_error handle slots are
 * initialized separately and are not covered by the no-payload-write guarantee.
 * Original scalar kernels and Rust-owned intermediate vectors are reused, but
 * no owned facade input/output column is made. On later kernel/sink failure,
 * discard ALL partial output; NEVER replay in another evaluator. No C++ callback
 * or exception crosses this ABI. Rust panic poisons the shared program for BOTH
 * eval APIs. OOM/abort/invalid pointers are not catchable. On success *out owns
 * diagnostics only, independent of inputs/output/program; on failure *out=NULL.
 */
uint32_t tikv_expr_eval_borrowed(tikv_expr_program *program,
    const tikv_expr_borrowed_column *columns, size_t columns_len,
    size_t row_count, const tikv_expr_selection *selection,
    const tikv_expr_borrowed_output *output,
    tikv_expr_diagnostics **out, tikv_expr_error **out_error);
uint32_t tikv_expr_diagnostics_get_view(const tikv_expr_diagnostics *diagnostics,
    tikv_expr_diagnostics_view *out);
void tikv_expr_diagnostics_free(tikv_expr_diagnostics *diagnostics);

#ifdef __cplusplus
}
#endif
#endif /* TIKV_EXPR_H */
