/* Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0. */
/* Compile as both C11 and C++17; no C++ types cross the shared-library ABI. */
#include "tikv_expr.h"
#include <assert.h>
#include <stdio.h>

#if defined(__cplusplus)
#define ABI_ASSERT static_assert
#else
#define ABI_ASSERT _Static_assert
#endif

#if SIZE_MAX == UINT64_MAX
ABI_ASSERT(sizeof(tikv_expr_bytes) == 16, "bytes layout");
ABI_ASSERT(sizeof(tikv_expr_context) == 56, "context layout");
ABI_ASSERT(sizeof(tikv_expr_column) == 48, "column layout");
ABI_ASSERT(sizeof(tikv_expr_selection) == 16, "selection layout");
ABI_ASSERT(sizeof(tikv_expr_warning) == 24, "warning layout");
ABI_ASSERT(sizeof(tikv_expr_result_view) == 72, "result view layout");
ABI_ASSERT(sizeof(tikv_expr_error_view) == 24, "error view layout");
ABI_ASSERT(sizeof(tikv_expr_borrowed_column) == 88, "borrowed column layout");
ABI_ASSERT(sizeof(tikv_expr_borrowed_output) == 64, "borrowed output layout");
ABI_ASSERT(sizeof(tikv_expr_diagnostics_view) == 24, "diagnostics layout");
ABI_ASSERT(offsetof(tikv_expr_borrowed_column, offsets) == 72, "END offsets pointer");
ABI_ASSERT(offsetof(tikv_expr_borrowed_output, uint64s) == 56, "UInt64 output pointer");
ABI_ASSERT(offsetof(tikv_expr_context, timezone_name) == 24, "timezone offset");
ABI_ASSERT(offsetof(tikv_expr_column, strings) == 40, "strings offset");
#endif

int main(void)
{
    tikv_expr_context context;
    tikv_expr_program *program = NULL;
    tikv_expr_result *result = NULL;
    tikv_expr_error *error = NULL;
    tikv_expr_error_view error_view;
    tikv_expr_result_view result_view;
    const tikv_expr_bytes empty = {NULL, 0};
    assert(tikv_expr_abi_version() == TIKV_EXPR_ABI_VERSION);
    assert(tikv_expr_context_default(&context) == TIKV_EXPR_OK);
    assert(context.struct_size == sizeof(context));
    assert(context.flags == 0 && context.sql_mode == 0);
    assert(context.div_precision_increment == 4 && context.max_warning_count == 64);
    assert(tikv_expr_compile(empty, NULL, 0, &context, &program, &error)
           == TIKV_EXPR_COMPILE_ERROR);
    assert(program == NULL && error != NULL);
    assert(tikv_expr_error_get_view(error, &error_view) == TIKV_EXPR_OK);
    assert(error_view.status == TIKV_EXPR_COMPILE_ERROR && error_view.mysql_code == 1105);
    assert(error_view.message.data != NULL && error_view.message.len != 0);
    tikv_expr_error_free(error);
    error = NULL;
    assert(tikv_expr_eval(NULL, NULL, 0, 0, NULL, &result, &error)
           == TIKV_EXPR_INVALID_ARGUMENT);
    assert(result == NULL && error != NULL);
    tikv_expr_error_free(error);
    assert(tikv_expr_result_get_view(NULL, &result_view) == TIKV_EXPR_INVALID_ARGUMENT);
    assert(result_view.column.len == 0 && result_view.warning_count == 0);
    {
        tikv_expr_diagnostics *diagnostics = NULL;
        tikv_expr_diagnostics_view diagnostics_view;
        assert(tikv_expr_borrowed_abi_version() == TIKV_EXPR_BORROWED_ABI_VERSION);
        assert(tikv_expr_program_supports_borrowed(NULL) == 0);
        error = NULL;
        assert(tikv_expr_eval_borrowed(NULL, NULL, 0, 0, NULL, NULL,
                                     &diagnostics, &error) == TIKV_EXPR_INVALID_ARGUMENT);
        assert(diagnostics == NULL && error != NULL);
        tikv_expr_error_free(error);
        assert(tikv_expr_diagnostics_get_view(NULL, &diagnostics_view)
               == TIKV_EXPR_INVALID_ARGUMENT);
        assert(diagnostics_view.warnings == NULL && diagnostics_view.warning_count == 0);
        tikv_expr_diagnostics_free(NULL);
    }
    tikv_expr_program_free(NULL);
    tikv_expr_result_free(NULL);
    tikv_expr_error_free(NULL);
    puts("C/C++ header and shared-library ABI smoke passed");
    return 0;
}
