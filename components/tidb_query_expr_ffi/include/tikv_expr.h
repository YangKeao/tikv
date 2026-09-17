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

#ifdef __cplusplus
}
#endif
#endif /* TIKV_EXPR_H */
