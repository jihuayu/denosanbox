/*
 * denosanbox experimental C ABI, version 1.
 *
 * This is a deliberately small, direct-native subset of the denosanbox core.
 * It is NOT the final stable language-binding surface.
 *
 * ABI 1 is additive: every declaration below existed in ABI 1.0 and keeps its
 * signature and structure layout. The host-callback product (engine capability
 * ceiling, callback registration, grants, callback context) was added without a
 * version bump; it does not claim issue-16 completeness beyond what is listed.
 * Polling is still the only async completion path.
 *
 * Threading: all token APIs are safe to call concurrently. Runtime calls never
 * hold the global registry lock. A sandbox invocation may run concurrently with
 * another invocation on the same sandbox according to core queue semantics.
 * Host callbacks execute synchronously on a managed native host thread owned by
 * the core (never on the isolate thread and never with a registry lock held),
 * so a callback may block and may poll deno_context_check.
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
 * DENO_STATUS_INTERNAL. Aborts, OOM and native faults are not contained, and a
 * foreign callback that throws/longjmps/panics across the boundary, or faults,
 * is a caller bug that is not contained.
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
#define DENO_MAX_CAPABILITIES_BYTES (1024u * 1024u)
#define DENO_MAX_GRANTS_BYTES (1024u * 1024u)
#define DENO_MAX_CALLBACK_NAME_BYTES 64u
/* Edge limit for a foreign callback's input/output and for deno_buffer_copy. */
#define DENO_MAX_CALLBACK_EDGE_BYTES (64u * 1024u * 1024u)

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

/*
 * Borrowed callback context token, valid only inside a host callback invocation
 * (0 is "no context"). It cannot be freed or forged into another sandbox.
 */
typedef uint64_t deno_context_token;

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

/*
 * Native host callback invocations (across all sandboxes of this library) that
 * have not returned yet. Call after deno_sandbox_close: nonzero means a foreign
 * callback is still running, close reported closed_cleanly == 0, and that
 * callback's user data is released only after it really ends. No error_out;
 * a NULL output is invalid and token 0 is accepted for a global query.
 */
deno_status_t deno_sandbox_pending_host_operations(deno_sandbox_token sandbox,
                                                   uint64_t *out_pending);

/* Frees a buffer struct by token and zeroes it. Null is invalid; the zeroed
 * struct is a harmless no-op so repeated zero frees are safe. */
deno_status_t deno_buffer_free(deno_buffer *buffer);

/* Frees an error message by token and zeroes the whole error struct. */
deno_status_t deno_error_free(deno_error *error);

/*
 * --------------------------------------------------------------------------
 * Scoped host callbacks (additive, ABI 1)
 * --------------------------------------------------------------------------
 *
 * Callback returns a deno_status_t-shaped i32. Any non-OK value is converted by
 * native code into one generic typed policy error; the foreign error text is
 * never forwarded. input/context are borrowed for the duration of the call only
 * and must not be retained. The result must be an owned deno_buffer produced by
 * deno_buffer_copy on success; native code consumes it exactly once (also when
 * the callback reports an error). release (optional) is called exactly once by
 * native code with the transferred user_data after the last registration,
 * sandbox grant and in-flight invocation no longer references the callback.
 * Callback and release must be thread safe and must not throw across the
 * boundary.
 */
typedef deno_status_t (*deno_host_callback_fn)(deno_context_token context,
                                               const uint8_t *input,
                                               size_t input_len,
                                               deno_buffer *out_result,
                                               uint64_t user_data);
typedef void (*deno_host_release_fn)(uint64_t user_data);

/*
 * Engine with a fixed capability-name ceiling: a JSON array of strings, e.g.
 * ["echo","lookup"]. Names are lower-case [a-z0-9_.:-], 1..=64 bytes, and the
 * list is capped by DENO_MAX_CAPABILITIES_BYTES before parsing. The ceiling is
 * immutable after construction. deno_engine_new remains deny-all.
 */
deno_status_t deno_engine_new_with_capabilities(const uint8_t *capabilities_json,
                                                size_t capabilities_len,
                                                deno_engine_token *out_engine,
                                                deno_error *error_out);

/*
 * Register a host callback. Registration alone grants nothing to any sandbox;
 * the callback also needs a grant via deno_engine_create_sandbox_with_grants and
 * its name must be inside the engine ceiling. Limits are 1..=64 MiB.
 * Ownership of user_data transfers ONLY on DENO_STATUS_OK; on failure the caller
 * still owns it and release is not called. Requires an engine created with
 * deno_engine_new_with_capabilities.
 */
deno_status_t deno_engine_register_host(deno_engine_token engine,
                                        const uint8_t *name,
                                        size_t name_len,
                                        size_t input_limit,
                                        size_t output_limit,
                                        deno_host_callback_fn callback,
                                        deno_host_release_fn release,
                                        uint64_t user_data,
                                        deno_error *error_out);

/*
 * Create a sandbox whose only permitted capabilities are the requested host
 * grants: a JSON object {"name": "scope"}. Names are resolved against the
 * owning engine's registrations and the fixed ceiling; the guest source can
 * never contribute a grant. Scope is trusted host metadata (max 64 bytes).
 * deno_engine_create_sandbox is equivalent to passing {}.
 */
deno_status_t deno_engine_create_sandbox_with_grants(deno_engine_token engine,
                                                     const uint8_t *source,
                                                     size_t source_len,
                                                     const uint8_t *grants_json,
                                                     size_t grants_len,
                                                     deno_sandbox_token *out_sandbox,
                                                     deno_error *error_out);

/* Copies up to DENO_MAX_CALLBACK_EDGE_BYTES into a new owned deno_buffer. */
deno_status_t deno_buffer_copy(const uint8_t *data,
                               size_t len,
                               deno_buffer *out_buffer,
                               deno_error *error_out);

/* Trusted context metadata JSON: {"sandbox_id","scope","remaining_ms",
 * "cancelled"}. Metadata comes from the native invocation, never from guest
 * JSON. remaining_ms is null once the deadline is gone. */
deno_status_t deno_context_json(deno_context_token context,
                                deno_buffer *out_buffer,
                                deno_error *error_out);

/* OK / DENO_STATUS_CANCELLED / DENO_STATUS_DEADLINE /
 * DENO_STATUS_INVALID_HANDLE (stale or foreign token). */
deno_status_t deno_context_check(deno_context_token context);

/* Context tokens are borrowed: only the native owner releases them. Always
 * returns DENO_STATUS_INVALID_ARGUMENT for a live borrowed token. */
deno_status_t deno_context_free(deno_context_token context);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* DENOSANBOX_H */
