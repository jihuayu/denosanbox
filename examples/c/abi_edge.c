/* Exercise edge cases of the experimental C ABI: caps before parsing, NULL+0
 * input slices, NULL outputs, stale/cross-kind tokens and buffer ownership. */
#include <stdio.h>
#include <string.h>

#include "denosanbox.h"

static int failures = 0;

#define CHECK(condition, label)                                                \
    do {                                                                       \
        if (condition) {                                                       \
            printf("ok: %s\n", label);                                         \
        } else {                                                               \
            fprintf(stderr, "FAIL: %s\n", label);                              \
            failures++;                                                        \
        }                                                                      \
    } while (0)

static int submit_and_wait(deno_sandbox_token sandbox,
                           const char *export_name,
                           const char *input,
                           deno_job_token *out_job,
                           deno_buffer *out_buffer,
                           deno_error *error) {
    deno_status_t status = deno_sandbox_call_submit(
        sandbox,
        (const uint8_t *)export_name,
        strlen(export_name),
        (const uint8_t *)input,
        strlen(input),
        5000,
        out_job,
        error);
    if (status != DENO_STATUS_OK) {
        return status;
    }
    return deno_job_wait(*out_job, 10000, out_buffer, error);
}

int main(void) {
    CHECK(deno_abi_version() == 1u, "ABI version is 1");
    CHECK(strcmp(deno_package_version(), "0.1.0") == 0, "package version is 0.1.0");
    CHECK(strcmp(deno_status_name(DENO_STATUS_CANCELLED), "cancelled") == 0,
          "status names are stable");

    deno_error error = {0};
    deno_engine_token engine = 0;
    CHECK(deno_engine_new(NULL, &error) == DENO_STATUS_INVALID_ARGUMENT,
          "NULL engine output is invalid");
    deno_error_free(&error);
    CHECK(deno_engine_new(&engine, NULL) == DENO_STATUS_INVALID_ARGUMENT,
          "NULL error output is invalid");
    CHECK(engine == 0, "engine token remains zero after NULL error output");
    CHECK(deno_engine_new(&engine, &error) == DENO_STATUS_OK,
          "engine construction succeeds");
    deno_error_free(&error);

    /* Oversize inputs must be rejected before any slice or JSON parse. The
     * pointer is NULL to prove the cap check happens first. */
    deno_sandbox_token oversize_sandbox = 123;
    CHECK(deno_engine_create_sandbox(engine, NULL, DENO_MAX_SOURCE_BYTES + 1,
                                     &oversize_sandbox, &error) == DENO_STATUS_LIMIT,
          "oversize source rejected before slice");
    CHECK(oversize_sandbox == 0, "sandbox output zeroed on cap failure");
    deno_error_free(&error);

    /* NULL + 0 is the documented empty input slice. Empty source is handed to
     * the core as an empty module (the binding itself must not form a slice
     * from NULL, which would be UB). */
    deno_sandbox_token empty_source_sandbox = 0;
    deno_status_t empty_status = deno_engine_create_sandbox(
        engine, NULL, 0, &empty_source_sandbox, &error);
    CHECK(empty_status == DENO_STATUS_OK || empty_status == DENO_STATUS_INVALID_ARGUMENT,
          "NULL+0 source is handled without UB");
    deno_error_free(&error);
    if (empty_source_sandbox != 0) {
        deno_sandbox_close_report report = {0};
        deno_sandbox_close(empty_source_sandbox, &report, &error);
        deno_error_free(&error);
        deno_sandbox_free(empty_source_sandbox, &error);
        deno_error_free(&error);
    }

    static const char source[] = "export function get() { return 7; }\n";
    deno_sandbox_token sandbox = 0;
    CHECK(deno_engine_create_sandbox(engine, (const uint8_t *)source, strlen(source),
                                     &sandbox, &error) == DENO_STATUS_OK,
          "sandbox creation succeeds");
    deno_error_free(&error);
    CHECK(sandbox != 0, "sandbox token is nonzero");

    /* Cross-kind token rejection. */
    deno_sandbox_token wrong_kind_sandbox = 0;
    CHECK(deno_engine_create_sandbox(sandbox, (const uint8_t *)source, strlen(source),
                                     &wrong_kind_sandbox, &error) == DENO_STATUS_INVALID_HANDLE,
          "sandbox token rejected as engine");
    deno_error_free(&error);
    CHECK(deno_engine_free(sandbox, &error) == DENO_STATUS_INVALID_HANDLE,
          "sandbox token rejected as engine free");
    deno_error_free(&error);

    /* Empty / NULL / oversize input and export validation. */
    deno_job_token job = 123;
    CHECK(deno_sandbox_call_submit(sandbox, (const uint8_t *)"get", 3,
                                   NULL, 0, 0, &job, &error) == DENO_STATUS_INVALID_ARGUMENT,
          "empty JSON is invalid");
    CHECK(job == 0, "job output zeroed on invalid JSON");
    deno_error_free(&error);

    CHECK(deno_sandbox_call_submit(sandbox, (const uint8_t *)"get", 3,
                                   NULL, 1, 0, &job, &error) == DENO_STATUS_INVALID_ARGUMENT,
          "NULL input with nonzero length is invalid");
    deno_error_free(&error);

    static uint8_t too_long_export[DENO_MAX_EXPORT_BYTES + 1];
    memset(too_long_export, 'a', sizeof(too_long_export));
    CHECK(deno_sandbox_call_submit(sandbox, too_long_export, sizeof(too_long_export),
                                   (const uint8_t *)"null", 4, 0, &job, &error) == DENO_STATUS_LIMIT,
          "oversize export rejected before slice");
    deno_error_free(&error);

    CHECK(deno_sandbox_call_submit(sandbox, (const uint8_t *)"get", 3,
                                   NULL, DENO_MAX_INPUT_BYTES + 1, 0, &job, &error) == DENO_STATUS_LIMIT,
          "oversize input rejected before slice");
    deno_error_free(&error);

    /* Result buffer ownership: a copied buffer token is stale after the first
     * free and cannot double-free the live allocation. */
    deno_buffer buffer = {0};
    CHECK(submit_and_wait(sandbox, "get", "null", &job, &buffer, &error) == DENO_STATUS_OK,
          "successful call returns JSON");
    CHECK(buffer.token != 0, "result buffer has an owned token");
    deno_buffer copied = buffer;
    CHECK(deno_buffer_free(&buffer) == DENO_STATUS_OK, "first buffer free succeeds");
    CHECK(buffer.token == 0 && buffer.data == NULL, "freed buffer is zeroed");
    CHECK(deno_buffer_free(&buffer) == DENO_STATUS_OK, "zeroed buffer free is a no-op");
    CHECK(deno_buffer_free(&copied) == DENO_STATUS_INVALID_HANDLE,
          "copied stale buffer token is invalid");
    CHECK(copied.token == 0, "copied stale buffer is zeroed");
    CHECK(deno_job_free(job) == DENO_STATUS_OK, "completed job frees");
    CHECK(deno_job_free(job) == DENO_STATUS_INVALID_HANDLE, "job double free is invalid");

    /* Typed execution error and owned error message cleanup. */
    deno_job_token missing_job = 0;
    deno_buffer missing_buffer = {0};
    deno_status_t missing = submit_and_wait(sandbox, "missing", "null",
                                            &missing_job, &missing_buffer, &error);
    CHECK(missing != DENO_STATUS_OK, "missing export fails");
    CHECK(error.code == missing, "error_out code matches returned status");
    CHECK(error.message.token != 0, "error message is owned");
    deno_buffer copied_message = error.message;
    CHECK(deno_error_free(&error) == DENO_STATUS_OK, "error message frees");
    CHECK(error.message.token == 0, "error struct is zeroed");
    CHECK(deno_buffer_free(&copied_message) == DENO_STATUS_INVALID_HANDLE,
          "copied error message token is stale");
    deno_job_free(missing_job);

    /* Early job free releases only the owner token; the native task retains
     * its own ownership and the sandbox can still be closed. */
    deno_job_token early_job = 0;
    CHECK(deno_sandbox_call_submit(sandbox, (const uint8_t *)"get", 3,
                                   (const uint8_t *)"null", 4, 1000,
                                   &early_job, &error) == DENO_STATUS_OK,
          "job submission succeeds");
    deno_error_free(&error);
    CHECK(deno_job_free(early_job) == DENO_STATUS_OK, "early job free succeeds");
    CHECK(deno_job_wait(early_job, 10, &buffer, &error) == DENO_STATUS_INVALID_HANDLE,
          "freed job token is invalid");
    deno_error_free(&error);

    CHECK(deno_sandbox_close(sandbox, NULL, &error) == DENO_STATUS_INVALID_ARGUMENT,
          "NULL close report is invalid");
    deno_error_free(&error);

    deno_sandbox_close_report report = {0};
    CHECK(deno_sandbox_close(sandbox, &report, &error) == DENO_STATUS_OK,
          "sandbox closes");
    printf("sandbox close: clean=%d forced=%d\n", report.closed_cleanly,
           report.forced_termination);
    deno_error_free(&error);
    CHECK(deno_sandbox_free(sandbox, &error) == DENO_STATUS_OK, "sandbox frees");
    CHECK(deno_sandbox_free(sandbox, &error) == DENO_STATUS_INVALID_HANDLE,
          "sandbox double free is invalid");
    deno_error_free(&error);
    CHECK(deno_sandbox_close(sandbox, &report, &error) == DENO_STATUS_INVALID_HANDLE,
          "stale sandbox close is invalid");
    deno_error_free(&error);

    deno_engine_shutdown_report shutdown = {0};
    CHECK(deno_engine_close(engine, &shutdown, &error) == DENO_STATUS_OK,
          "engine closes");
    printf("engine close: clean=%d closed=%llu\n", shutdown.clean,
           (unsigned long long)shutdown.sandboxes_closed);
    deno_error_free(&error);
    CHECK(deno_engine_free(engine, &error) == DENO_STATUS_OK, "engine frees");
    CHECK(deno_engine_free(engine, &error) == DENO_STATUS_INVALID_HANDLE,
          "engine double free is invalid");
    deno_error_free(&error);

    printf(failures == 0 ? "C ABI edge tests: OK\n" : "C ABI edge tests: FAILED (%d)\n",
           failures);
    return failures == 0 ? 0 : 1;
}
