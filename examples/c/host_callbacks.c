/* Host callback product: engine ceiling, registration, grants, trusted context,
 * same-PID callbacks, cooperative cancellation and exactly-once userdata
 * release after close.
 *
 * Callbacks run synchronously on a core-managed host thread of this process.
 * The main thread keeps polling the job while a callback is blocked; the
 * callback samples its own PID into userspace and the main thread compares it
 * with getpid(), proving a same-process (same-PID) callback. */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

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

enum { USERDATA_ECHO = 0xE001u, USERDATA_SLOW = 0xE002u };

static _Atomic int echo_released = 0;
static _Atomic int slow_released = 0;
static _Atomic int slow_cancel_check = -1;
static _Atomic int slow_seen = 0;
static _Atomic int callback_pid = 0;

static void note_release(_Atomic int *counter, uint64_t user_data) {
    if (atomic_fetch_add(counter, 1) != 0) {
        fprintf(stderr, "FAIL: release called more than once for %#llx\n",
                (unsigned long long)user_data);
        failures++;
    }
}

static void release_echo(uint64_t user_data) {
    if (user_data != USERDATA_ECHO) {
        failures++;
    }
    note_release(&echo_released, user_data);
}

static void release_slow(uint64_t user_data) {
    if (user_data != USERDATA_SLOW) {
        failures++;
    }
    note_release(&slow_released, user_data);
}

static void copy_string(const char *text, deno_buffer *out) {
    if (deno_buffer_copy((const uint8_t *)text, strlen(text), out, NULL) != DENO_STATUS_OK) {
        *out = (deno_buffer){0};
    }
}

/* echo: returns trusted context metadata plus the received input. */
static deno_status_t echo_callback(deno_context_token context,
                                   const uint8_t *input,
                                   size_t input_len,
                                   deno_buffer *out_result,
                                   uint64_t user_data) {
    if (user_data != USERDATA_ECHO) {
        return DENO_STATUS_INVALID_ARGUMENT;
    }
    deno_buffer meta = {0};
    deno_error error = {0};
    if (deno_context_json(context, &meta, &error) != DENO_STATUS_OK) {
        deno_error_free(&error);
        return DENO_STATUS_INTERNAL;
    }
    char body[512];
    size_t used = (size_t)snprintf(body, sizeof body, "{\"context\":");
    if (used + meta.len + 16 >= sizeof body) {
        deno_buffer_free(&meta);
        return DENO_STATUS_LIMIT;
    }
    memcpy(body + used, meta.data, meta.len);
    used += meta.len;
    deno_buffer_free(&meta);
    int written = snprintf(body + used, sizeof body - used, ",\"input_len\":%zu}", input_len);
    if (written < 0) {
        return DENO_STATUS_INTERNAL;
    }
    (void)input; /* input is borrowed; do not retain it */
    copy_string(body, out_result);
    return DENO_STATUS_OK;
}

/* slow: blocks until cancelled, checking only the native context token. */
static deno_status_t slow_callback(deno_context_token context,
                                   const uint8_t *input,
                                   size_t input_len,
                                   deno_buffer *out_result,
                                   uint64_t user_data) {
    (void)input;
    (void)input_len;
    if (user_data != USERDATA_SLOW) {
        return DENO_STATUS_INVALID_ARGUMENT;
    }
    atomic_store(&callback_pid, (int)getpid());
    atomic_store(&slow_seen, 1);
    for (int i = 0; i < 400; i++) {
        deno_status_t live = deno_context_check(context);
        if (live != DENO_STATUS_OK) {
            atomic_store(&slow_cancel_check, (int)live);
            return DENO_STATUS_CANCELLED;
        }
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 5 * 1000 * 1000};
        nanosleep(&pause, NULL);
    }
    copy_string("{\"finished\":true}", out_result);
    return DENO_STATUS_OK;
}

static const char ECHO_GUEST[] =
    "export async function run(input) {\n"
    "  const host = await sandbox.host.call('echo', input);\n"
    "  return { host };\n"
    "}\n";

static const char SLOW_GUEST[] =
    "export function run() {\n"
    "  return sandbox.host.call('slow', null);\n"
    "}\n";

static const char UNGRANTED_GUEST[] =
    "export function run() {\n"
    "  return sandbox.host.call('echo', {scope: 'forged'});\n"
    "}\n";

static deno_status_t run_call(deno_sandbox_token sandbox,
                              const char *input,
                              deno_job_token *job,
                              deno_buffer *out,
                              deno_error *error) {
    deno_status_t status = deno_sandbox_call_submit(sandbox, (const uint8_t *)"run", 3,
                                                    (const uint8_t *)input, strlen(input), 10000,
                                                    job, error);
    if (status != DENO_STATUS_OK) {
        return status;
    }
    return deno_job_wait(*job, 15000, out, error);
}

int main(void) {
    deno_error error = {0};
    deno_engine_token engine = 0;
    deno_sandbox_token alpha = 0;
    deno_sandbox_token beta = 0;

    /* deno_engine_new stays deny-all: registration is accepted but a grant can
     * never pass the immutable ceiling. */
    deno_engine_token plain = 0;
    CHECK(deno_engine_new(&plain, &error) == DENO_STATUS_OK, "plain engine constructs");
    deno_error_free(&error);
    CHECK(deno_engine_register_host(plain, (const uint8_t *)"echo", 4, 1024, 1024,
                                    echo_callback, release_echo, USERDATA_ECHO,
                                    &error) == DENO_STATUS_OK,
          "plain engine registration accepted (ceiling still gates grants)");
    deno_error_free(&error);
    {
        const char *plain_grants = "{\"echo\":\"tenant-plain\"}";
        deno_sandbox_token denied = 0;
        CHECK(deno_engine_create_sandbox_with_grants(
                  plain, (const uint8_t *)ECHO_GUEST, strlen(ECHO_GUEST),
                  (const uint8_t *)plain_grants, strlen(plain_grants), &denied,
                  &error) != DENO_STATUS_OK,
              "deny-all engine rejects every grant");
        deno_error_free(&error);
        CHECK(denied == 0, "denied sandbox token stays zero");
    }
    {
        deno_engine_shutdown_report report;
        CHECK(deno_engine_close(plain, NULL, &error) == DENO_STATUS_INVALID_ARGUMENT,
              "NULL close report rejected");
        deno_error_free(&error);
        CHECK(deno_engine_close(plain, &report, &error) == DENO_STATUS_OK,
              "plain engine closes");
        deno_error_free(&error);
        CHECK(atomic_load(&echo_released) == 0, "plain close keeps userdata");
        CHECK(deno_engine_free(plain, &error) == DENO_STATUS_OK, "plain engine frees");
        deno_error_free(&error);
        CHECK(atomic_load(&echo_released) == 1,
              "plain engine userdata released exactly once after free");
        atomic_store(&echo_released, 0);
    }

    /* Oversized capability list is rejected before parsing. */
    CHECK(deno_engine_new_with_capabilities((const uint8_t *)"[", DENO_MAX_CAPABILITIES_BYTES + 1,
                                           &engine, &error) == DENO_STATUS_LIMIT,
          "capability list cap enforced");
    CHECK(engine == 0, "engine token stays zero after cap failure");
    deno_error_free(&error);

    const char *caps = "[\"echo\",\"slow\"]";
    CHECK(deno_engine_new_with_capabilities((const uint8_t *)caps, strlen(caps), &engine,
                                           &error) == DENO_STATUS_OK,
          "engine with capability ceiling constructs");
    deno_error_free(&error);

    /* Registration limits and required function pointer. */
    CHECK(deno_engine_register_host(engine, (const uint8_t *)"echo", 4, 0, 1024, echo_callback,
                                    NULL, 0, &error) == DENO_STATUS_LIMIT,
          "zero input limit rejected");
    deno_error_free(&error);

    /* A failed registration does not release caller-owned userdata. */
    CHECK(deno_engine_register_host(engine, (const uint8_t *)"ECHO", 4, 1024, 1024,
                                    echo_callback, release_echo, USERDATA_ECHO,
                                    &error) == DENO_STATUS_INVALID_ARGUMENT,
          "invalid callback name rejected");
    deno_error_free(&error);
    CHECK(atomic_load(&echo_released) == 0, "failed registration keeps userdata");

    CHECK(deno_engine_register_host(engine, (const uint8_t *)"echo", 4, 4096, 4096, echo_callback,
                                    release_echo, USERDATA_ECHO, &error) == DENO_STATUS_OK,
          "echo callback registers");
    deno_error_free(&error);
    CHECK(deno_engine_register_host(engine, (const uint8_t *)"echo", 4, 4096, 4096, echo_callback,
                                    release_echo, USERDATA_ECHO, &error) != DENO_STATUS_OK,
          "duplicate echo registration rejected");
    deno_error_free(&error);
    CHECK(atomic_load(&echo_released) == 0, "duplicate rejection keeps userdata");
    CHECK(deno_engine_register_host(engine, (const uint8_t *)"slow", 4, 4096, 4096, slow_callback,
                                    release_slow, USERDATA_SLOW, &error) == DENO_STATUS_OK,
          "slow callback registers");
    deno_error_free(&error);

    /* "missing" is inside the ceiling but unregistered: registration alone
     * grants nothing to any sandbox. */
    const char *unknown = "{\"missing\":\"tenant-x\"}";
    deno_sandbox_token rejected = 0;
    CHECK(deno_engine_create_sandbox_with_grants(
              engine, (const uint8_t *)ECHO_GUEST, strlen(ECHO_GUEST), (const uint8_t *)unknown,
              strlen(unknown), &rejected, &error) != DENO_STATUS_OK,
          "grant of unregistered callback rejected");
    deno_error_free(&error);

    const char *alpha_grants = "{\"echo\":\"tenant-alpha\",\"slow\":\"tenant-alpha\"}";
    CHECK(deno_engine_create_sandbox_with_grants(engine, (const uint8_t *)ECHO_GUEST,
                                                 strlen(ECHO_GUEST),
                                                 (const uint8_t *)alpha_grants,
                                                 strlen(alpha_grants), &alpha,
                                                 &error) == DENO_STATUS_OK,
          "alpha sandbox created with grants");
    deno_error_free(&error);
    const char *beta_grants = "{\"echo\":\"tenant-beta\"}";
    CHECK(deno_engine_create_sandbox_with_grants(
              engine, (const uint8_t *)ECHO_GUEST, strlen(ECHO_GUEST), (const uint8_t *)beta_grants,
              strlen(beta_grants), &beta, &error) == DENO_STATUS_OK,
          "beta sandbox created with grants");
    deno_error_free(&error);

    /* Trusted scope: the guest tries to forge "tenant-forged" in its JSON. */
    deno_job_token job = 0;
    deno_buffer result = {0};
    deno_status_t alpha_status = run_call(alpha, "{\"scope\":\"tenant-forged\"}", &job, &result,
                                          &error);
    if (alpha_status != DENO_STATUS_OK) {
        char message[512];
        size_t n = error.message.len < sizeof message - 1 ? error.message.len : sizeof message - 1;
        memcpy(message, error.message.data, n);
        message[n] = 0;
        fprintf(stderr, "alpha status=%d: %s\n", alpha_status, message);
    }
    CHECK(alpha_status == DENO_STATUS_OK, "alpha host call completes");
    CHECK(result.data != NULL &&
              memmem(result.data, result.len, "\"tenant-alpha\"", 14) != NULL,
          "alpha callback sees trusted scope");
    CHECK(result.data != NULL &&
              memmem(result.data, result.len, "tenant-forged", 13) == NULL,
          "forged guest scope is not trusted");
    CHECK(result.data != NULL && memmem(result.data, result.len, "sandbox-", 8) != NULL,
          "alpha sees a trusted sandbox id");
    deno_buffer_free(&result);
    deno_job_free(job);

    CHECK(run_call(beta, "null", &job, &result, &error) == DENO_STATUS_OK,
          "beta host call completes independently");
    CHECK(result.data != NULL &&
              memmem(result.data, result.len, "\"tenant-beta\"", 13) != NULL,
          "beta sees its own scope");
    deno_buffer_free(&result);
    deno_job_free(job);

    /* Ungranted capability: a fresh sandbox created without grants. */
    deno_sandbox_token bare = 0;
    CHECK(deno_engine_create_sandbox(engine, (const uint8_t *)UNGRANTED_GUEST,
                                     strlen(UNGRANTED_GUEST), &bare,
                                     &error) == DENO_STATUS_OK,
          "ungranted sandbox created");
    deno_error_free(&error);
    CHECK(run_call(bare, "null", &job, &result, &error) != DENO_STATUS_OK,
          "ungranted host call is rejected");
    CHECK(result.token == 0, "rejected call yields no result buffer");
    deno_error_free(&error);
    deno_job_free(job);
    {
        deno_sandbox_close_report report;
        CHECK(deno_sandbox_close(bare, &report, &error) == DENO_STATUS_OK,
              "ungranted sandbox closes");
        deno_error_free(&error);
        CHECK(deno_sandbox_free(bare, &error) == DENO_STATUS_OK, "ungranted sandbox frees");
        deno_error_free(&error);
    }

    /* Cooperative cancellation: the callback blocks and is observed on the
     * main thread through shared atomics while polling the job. */
    deno_sandbox_token slow = 0;
    CHECK(deno_engine_create_sandbox_with_grants(
              engine, (const uint8_t *)SLOW_GUEST, strlen(SLOW_GUEST),
              (const uint8_t *)alpha_grants, strlen(alpha_grants), &slow,
              &error) == DENO_STATUS_OK,
          "slow sandbox created");
    deno_error_free(&error);
    deno_job_token slow_job = 0;
    CHECK(deno_sandbox_call_submit(slow, (const uint8_t *)"run", 3, (const uint8_t *)"null", 4,
                                   20000, &slow_job, &error) == DENO_STATUS_OK,
          "slow call submitted");
    deno_error_free(&error);
    for (int i = 0; i < 400 && !atomic_load(&slow_seen); i++) {
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 5 * 1000 * 1000};
        nanosleep(&pause, NULL);
    }
    CHECK(atomic_load(&slow_seen) == 1, "callback runs while main thread is blocked in the call");
    CHECK(atomic_load(&callback_pid) == (int)getpid(), "callback PID equals host PID (same process)");
    CHECK(deno_job_cancel(slow_job) == DENO_STATUS_OK, "job cancel returns immediately");
    deno_status_t cancel_status = deno_job_wait(slow_job, 5000, &result, &error);
    CHECK(cancel_status == DENO_STATUS_CANCELLED, "cancelled job reports cancelled");
    deno_error_free(&error);
    CHECK(result.token == 0, "cancelled job yields no buffer");
    CHECK(atomic_load(&slow_cancel_check) == (int)DENO_STATUS_CANCELLED,
          "callback observed native context cancellation");
    /* The context token is dead once the callback returned. */
    CHECK(deno_context_check(1) == DENO_STATUS_INVALID_HANDLE, "stale context rejected");
    CHECK(deno_context_free(1) == DENO_STATUS_INVALID_HANDLE, "foreign context cannot be freed");
    deno_job_free(slow_job);

    deno_sandbox_close_report report;
    CHECK(deno_sandbox_close(slow, &report, &error) == DENO_STATUS_OK, "slow sandbox closes");
    deno_error_free(&error);
    CHECK(deno_sandbox_free(slow, &error) == DENO_STATUS_OK, "slow sandbox frees");
    deno_error_free(&error);
    CHECK(atomic_load(&echo_released) == 0, "alpha/beta grants keep echo userdata alive");
    CHECK(atomic_load(&slow_released) == 0, "slow grant keeps userdata alive");

    CHECK(deno_sandbox_close(alpha, &report, &error) == DENO_STATUS_OK, "alpha closes");
    deno_error_free(&error);
    CHECK(deno_sandbox_free(alpha, &error) == DENO_STATUS_OK, "alpha frees");
    deno_error_free(&error);
    CHECK(atomic_load(&echo_released) == 0, "beta still holds echo userdata");
    CHECK(atomic_load(&slow_released) == 1, "slow userdata released after its sandbox closes");
    CHECK(deno_sandbox_close(beta, &report, &error) == DENO_STATUS_OK, "beta closes");
    deno_error_free(&error);
    CHECK(deno_sandbox_free(beta, &error) == DENO_STATUS_OK, "beta frees");
    deno_error_free(&error);
    CHECK(atomic_load(&echo_released) == 0, "engine registration still holds echo userdata");

    deno_engine_shutdown_report shutdown;
    CHECK(deno_engine_close(engine, &shutdown, &error) == DENO_STATUS_OK, "engine closes");
    deno_error_free(&error);
    CHECK(atomic_load(&echo_released) == 0, "close keeps callback owner until free");
    CHECK(deno_engine_free(engine, &error) == DENO_STATUS_OK, "engine frees");
    deno_error_free(&error);
    CHECK(atomic_load(&echo_released) == 1, "echo userdata released exactly once after free");
    CHECK(atomic_load(&slow_released) == 1, "slow userdata still released exactly once");

    if (failures != 0) {
        fprintf(stderr, "%d host callback check(s) failed\n", failures);
        return 1;
    }
    printf("all host callback checks passed\n");
    return 0;
}
