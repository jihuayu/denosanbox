/* Two native sandboxes with independent state, typed errors, cancellation and
 * clean close. This example uses only the polling subset of the C ABI. */
#include <stdio.h>
#include <string.h>

#include "denosanbox.h"

static int call_json(deno_sandbox_token sandbox,
                     const char *export_name,
                     const char *input_json,
                     const char *label) {
    deno_job_token job = 0;
    deno_error error = {0};
    deno_status_t status = deno_sandbox_call_submit(
        sandbox,
        (const uint8_t *)export_name,
        strlen(export_name),
        (const uint8_t *)input_json,
        strlen(input_json),
        5000,
        &job,
        &error);
    if (status != DENO_STATUS_OK) {
        fprintf(stderr, "%s submit failed: %s\n", label, deno_status_name(status));
        if (error.message.data != NULL && error.message.len > 0) {
            fprintf(stderr, "  %.*s\n", (int)error.message.len,
                    (const char *)error.message.data);
        }
        deno_error_free(&error);
        return 1;
    }

    deno_buffer result = {0};
    status = deno_job_wait(job, 10000, &result, &error);
    if (status == DENO_STATUS_OK) {
        printf("%s: %.*s\n", label, (int)result.len,
               result.data ? (const char *)result.data : "");
        deno_buffer_free(&result);
        deno_job_free(job);
        return 0;
    }

    fprintf(stderr, "%s failed: %s\n", label, deno_status_name(status));
    if (error.message.data != NULL && error.message.len > 0) {
        fprintf(stderr, "  %.*s\n", (int)error.message.len,
                (const char *)error.message.data);
    }
    deno_error_free(&error);
    deno_job_free(job);
    return 1;
}

/* After terminal cancel, a later call must fail with a typed lifecycle error. */
static int expect_terminal_after_cancel(deno_sandbox_token sandbox,
                                        const char *label) {
    deno_job_token job = 0;
    deno_error error = {0};
    deno_status_t status = deno_sandbox_call_submit(
        sandbox, (const uint8_t *)"get", 3, (const uint8_t *)"null", 4,
        5000, &job, &error);
    if (status != DENO_STATUS_OK) {
        int expected = status == DENO_STATUS_CLOSED || status == DENO_STATUS_CANCELLED;
        printf("%s submit: %s\n", label, deno_status_name(status));
        deno_error_free(&error);
        return expected ? 0 : 1;
    }

    deno_buffer result = {0};
    status = deno_job_wait(job, 10000, &result, &error);
    if (status == DENO_STATUS_OK) {
        fprintf(stderr, "%s unexpectedly succeeded after cancel\n", label);
        deno_buffer_free(&result);
        deno_job_free(job);
        return 1;
    }
    int expected = status == DENO_STATUS_CLOSED || status == DENO_STATUS_CANCELLED;
    printf("%s: %s\n", label, deno_status_name(status));
    deno_error_free(&error);
    deno_job_free(job);
    return expected ? 0 : 1;
}

static int close_sandbox(deno_sandbox_token sandbox, const char *label) {
    deno_sandbox_close_report report = {0};
    deno_error error = {0};
    deno_status_t status = deno_sandbox_close(sandbox, &report, &error);
    if (status != DENO_STATUS_OK) {
        fprintf(stderr, "%s close failed: %s\n", label, deno_status_name(status));
        deno_error_free(&error);
        deno_sandbox_free(sandbox, &error);
        deno_error_free(&error);
        return 1;
    }
    printf("%s close: clean=%d forced=%d completed=%llu cancelled=%llu\n",
           label,
           report.closed_cleanly,
           report.forced_termination,
           (unsigned long long)report.invocations_completed,
           (unsigned long long)report.invocations_cancelled);
    status = deno_sandbox_free(sandbox, &error);
    if (status != DENO_STATUS_OK) {
        fprintf(stderr, "%s free failed: %s\n", label, deno_status_name(status));
        deno_error_free(&error);
        return 1;
    }
    return 0;
}

int main(void) {
    int failures = 0;
    printf("C ABI %u, package %s\n", deno_abi_version(), deno_package_version());

    deno_engine_token engine = 0;
    deno_error error = {0};
    if (deno_engine_new(&engine, &error) != DENO_STATUS_OK) {
        fprintf(stderr, "engine_new failed: %s\n", deno_status_name(error.code));
        deno_error_free(&error);
        return 1;
    }

    static const char source[] =
        "let state = 0;\n"
        "export function inc() { state += 1; return state; }\n"
        "export function get() { return state; }\n";

    deno_sandbox_token first = 0;
    deno_sandbox_token second = 0;
    if (deno_engine_create_sandbox(engine, (const uint8_t *)source, strlen(source),
                                   &first, &error) != DENO_STATUS_OK) {
        fprintf(stderr, "first sandbox creation failed: %s\n", deno_status_name(error.code));
        deno_error_free(&error);
        deno_engine_free(engine, &error);
        return 1;
    }
    if (deno_engine_create_sandbox(engine, (const uint8_t *)source, strlen(source),
                                   &second, &error) != DENO_STATUS_OK) {
        fprintf(stderr, "second sandbox creation failed: %s\n", deno_status_name(error.code));
        deno_error_free(&error);
        deno_sandbox_free(first, &error);
        deno_engine_free(engine, &error);
        return 1;
    }

    failures += call_json(first, "inc", "null", "first.inc()");
    failures += call_json(first, "inc", "null", "first.inc()");
    failures += call_json(first, "get", "null", "first.get()");
    failures += call_json(second, "get", "null", "second.get()");

    deno_job_token missing_job = 0;
    deno_status_t missing_status = deno_sandbox_call_submit(
        first, (const uint8_t *)"missing", 7, (const uint8_t *)"null", 4,
        5000, &missing_job, &error);
    if (missing_status == DENO_STATUS_OK) {
        deno_buffer missing_result = {0};
        missing_status = deno_job_wait(missing_job, 10000, &missing_result, &error);
        if (missing_status == DENO_STATUS_OK) {
            fprintf(stderr, "missing export unexpectedly succeeded\n");
            deno_buffer_free(&missing_result);
            failures++;
        } else {
            printf("missing export typed error: %s\n", deno_status_name(missing_status));
            deno_error_free(&error);
        }
        deno_job_free(missing_job);
    } else {
        printf("missing export typed error: %s\n", deno_status_name(missing_status));
        deno_error_free(&error);
    }

    if (deno_sandbox_cancel(first) != DENO_STATUS_OK) {
        fprintf(stderr, "sandbox cancel failed\n");
        failures++;
    } else {
        printf("first cancel: terminal\n");
    }
    failures += expect_terminal_after_cancel(first, "first.get() after cancel");

    if (deno_sandbox_free(999999, &error) != DENO_STATUS_INVALID_HANDLE) {
        fprintf(stderr, "stale sandbox token was not rejected\n");
        failures++;
    } else {
        printf("stale sandbox token: typed invalid_handle\n");
    }
    deno_error_free(&error);

    failures += close_sandbox(second, "second");
    failures += close_sandbox(first, "first");

    deno_engine_shutdown_report shutdown = {0};
    if (deno_engine_close(engine, &shutdown, &error) != DENO_STATUS_OK) {
        fprintf(stderr, "engine close failed: %s\n", deno_status_name(error.code));
        deno_error_free(&error);
        deno_engine_free(engine, &error);
        return 1;
    }
    printf("engine close: clean=%d closed=%llu forced=%llu\n",
           shutdown.clean,
           (unsigned long long)shutdown.sandboxes_closed,
           (unsigned long long)shutdown.sandboxes_forced);
    if (deno_engine_free(engine, &error) != DENO_STATUS_OK) {
        fprintf(stderr, "engine free failed: %s\n", deno_status_name(error.code));
        deno_error_free(&error);
        failures++;
    }

    printf(failures == 0 ? "C example: OK\n" : "C example: FAILED\n");
    return failures == 0 ? 0 : 1;
}
