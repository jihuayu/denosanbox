/* C definitions for the //export trampolines declared in callbacks.go.
 *
 * cgo does not accept non-static definitions in a //export preamble, so the
 * forward-declared functions are defined here with their normal definitions and
 * passed to the C ABI as plain extern function pointers. No Go pointer is ever
 * stored in C: the only argument is the opaque uintptr_t cgo.Handle value. */
#include <stddef.h>
#include <stdint.h>

#include "denosanbox.h"

/* Implemented in callbacks.go. */
extern deno_status_t GoDenoHostCallback(uint64_t context, const uint8_t *input,
                                        size_t input_len, deno_buffer *out_result,
                                        uint64_t handle);
extern void GoDenoHostRelease(uint64_t handle);

deno_status_t denosanbox_go_callback(uint64_t context, const uint8_t *input,
                                     size_t input_len, deno_buffer *out_result,
                                     uint64_t handle) {
    return GoDenoHostCallback(context, input, input_len, out_result, handle);
}

void denosanbox_go_release(uint64_t handle) { GoDenoHostRelease(handle); }

deno_host_callback_fn denosanbox_callback_fn(void) { return denosanbox_go_callback; }

deno_host_release_fn denosanbox_release_fn(void) { return denosanbox_go_release; }
