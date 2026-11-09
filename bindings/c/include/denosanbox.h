/*
 * denosanbox experimental C ABI, version 1.
 *
 * This is a deliberately small, direct-native subset of the denosanbox core.
 * It is NOT the final stable language-binding surface and it does not claim
 * issue-16 completeness: guest callbacks, callback registration parity and the
 * release gate are still missing. Polling is the only async completion path.
 *
 * Threading: all token APIs are safe to call concurrently. Runtime calls never
 * hold the global registry lock. A sandbox invocation may run concurrently with
 * another invocation on the same sandbox according to core queue semantics.
 *
 * Ownership and pointer rules:
 *   - Tokens are opaque, owned u64 values. Never dereference or guess one.
 *     They are kind-checked and are never reused; a stale token or a token of
 *     the wrong kind returns DENO_STATUS_INVALID_HANDLE.
 *   - The caller must ensure every input pointer is readable for the stated
 *     length and every output pointer is writable. Passing an invalid mapped
 *     address is outside this ABI's containment guarantees.
 *   - All out_* and error_out pointers are required. NULL returns
 *     DENO_STATUS_INVALID_ARGUMENT without performing the requested operation.
 *   - Input slices with len == 0 may have ptr == NULL.
 *   - JSON and source bytes need not be NUL terminated.
 *   - Buffers returned by this library own registry storage. They stay valid
 *     until the matching deno_buffer_free call. Copy bytes out if you need
 *     them longer. A buffer struct is single-owner; after free its token is
 *     stale and freeing it again is invalid (the zeroed struct is a no-op).
 *   - deno_error values own their message buffer. Call deno_error_free when
 *     done and zero/reinitialize before reusing the struct.
 *
 * Panic containment: recoverable Rust panics at the extern boundary become
 * DENO_STATUS_INTERNAL. Aborts, OOM and native faults are not contained.
 *
 * Tested platform: Linux x86_64. Other platforms are neither claimed nor
 * tested by this experimental binding.
 */

#ifndef DENOSANBOX_H
#define DENOSANBOX_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define DENO_ABI_VERSION 1u
#define DENO_PACKAGE_VERSION "0.1.0"

/* Bounded before any Rust slice or JSON parser sees caller data. */
#define DENO_MAX_SOURCE_BYTES (8u * 1024u * 1024u)
#define DENO_MAX_INPUT_BYTES (1024u * 1024u)
#define DENO_MAX_EXPORT_BYTES 128u
#define DENO_MAX_PENDING_JOBS 132u

typedef uint64_t deno_engine_token;
typedef uint64_t deno_sandbox_token;
typedef uint64_t deno_job_token;

typedef enum deno_status {
    DENO_STATUS_OK = 0,
    DENO_STATUS_PENDING = 1,
    DENO_STATUS_INVALID_ARGUMENT = 2,
    DENO_STATUS_INVALID_HANDLE = 3,
    DENO_STATUS_CLOSED = 4,
    DENO_STATUS_CANCELLED = 5,
    DENO_STATUS_DEADLINE = 6,
    DENO_STATUS_LIMIT = 7,
    DENO_STATUS_EXECUTION = 8,
    DENO_STATUS_UNSUPPORTED = 9,
    DENO_STATUS_INTERNAL = 10
} deno_status_t;

typedef struct deno_buffer {
    const uint8_t *data;
    size_t len;
    uint64_t token;
} deno_buffer;

typedef struct deno_error {
    deno_status_t code;
    deno_buffer message;
} deno_error;

typedef struct deno_sandbox_close_report {
    uint64_t invocations_completed;
    uint64_t invocations_cancelled;
    int32_t forced_termination;
    int32_t closed_cleanly;
} deno_sandbox_close_report;

typedef struct deno_engine_shutdown_report {
    uint64_t sandboxes_closed;
    uint64_t sandboxes_forced;
    int32_t clean;
} deno_engine_shutdown_report;

/* ABI and package versions are independent. */
uint32_t deno_abi_version(void);
const char *deno_package_version(void);
const char *deno_status_name(deno_status_t status);

/*
 * Engine lifecycle. Each successful constructor writes a nonzero owned token.
 * Output tokens are set to zero on failure.
 */
deno_status_t deno_engine_new(deno_engine_token *out_engine, deno_error *error_out);
deno_status_t deno_engine_close(deno_engine_token engine,
                                deno_engine_shutdown_report *out_report,
                                deno_error *error_out);
deno_status_t deno_engine_free(deno_engine_token engine, deno_error *error_out);

/*
 * Create a sandbox from one UTF-8 ES module named "main.js".
 * source_len must be <= DENO_MAX_SOURCE_BYTES.
 * Call deno_sandbox_close before deno_sandbox_free.
 */
deno_status_t deno_engine_create_sandbox(deno_engine_token engine,
                                         const uint8_t *source,
                                         size_t source_len,
                                         deno_sandbox_token *out_sandbox,
                                         deno_error *error_out);

/*
 * Submit an export call. export_len must be <= DENO_MAX_EXPORT_BYTES and
 * input_json_len must be <= DENO_MAX_INPUT_BYTES. input_json must be a complete
 * JSON value (an empty slice is invalid JSON). timeout_ms == 0 uses core
 * defaults; otherwise the timeout is relative to submission.
 *
 * At most DENO_MAX_PENDING_JOBS calls may be in flight across this ABI;
 * saturation returns DENO_STATUS_LIMIT before creating another task.
 * The returned job owns its sandbox reference. job_cancel is terminal for the
 * sandbox: queued and later calls fail with DENO_STATUS_CLOSED. job_free only
 * releases the job token; a still-running task keeps its own ownership.
 */
deno_status_t deno_sandbox_call_submit(deno_sandbox_token sandbox,
                                       const uint8_t *export_name,
                                       size_t export_name_len,
                                       const uint8_t *input_json,
                                       size_t input_json_len,
                                       uint64_t timeout_ms,
                                       deno_job_token *out_job,
                                       deno_error *error_out);

/*
 * Wait for at most wait_ms milliseconds. Returns DENO_STATUS_OK and installs an
 * owned result buffer on success, DENO_STATUS_PENDING if the bounded wait
 * elapsed, or a typed failure status. The result is delivered at most once;
 * polling again after a terminal result is DENO_STATUS_INVALID_HANDLE.
 */
deno_status_t deno_job_wait(deno_job_token job,
                            uint64_t wait_ms,
                            deno_buffer *out_result,
                            deno_error *error_out);

/* Terminal cancellation. The token remains valid until job_free. */
deno_status_t deno_job_cancel(deno_job_token job);
deno_status_t deno_job_free(deno_job_token job);

/* Direct terminal cancellation of a sandbox (same semantics as core cancel). */
deno_status_t deno_sandbox_cancel(deno_sandbox_token sandbox);

/*
 * Close awaits the native core close path and reports forced termination
 * truthfully. A failed close returns its typed error; the token still has to be
 * released with deno_sandbox_free. close does not free the token.
 */
deno_status_t deno_sandbox_close(deno_sandbox_token sandbox,
                                 deno_sandbox_close_report *out_report,
                                 deno_error *error_out);
deno_status_t deno_sandbox_free(deno_sandbox_token sandbox, deno_error *error_out);

/* Frees a buffer struct by token and zeroes it. Null is invalid; the zeroed
 * struct is a harmless no-op so repeated zero frees are safe. */
deno_status_t deno_buffer_free(deno_buffer *buffer);

/* Frees an error message by token and zeroes the whole error struct. */
deno_status_t deno_error_free(deno_error *error);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* DENOSANBOX_H */
