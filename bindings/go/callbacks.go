package denosanbox

// Exported cgo trampolines for scoped host callbacks.
//
// The C ABI holds only an opaque uintptr_t (a cgo.Handle) as "user data". Each
// handle owns one *hostRecord. Release deletes that handle exactly once, after
// the native owner (engine registration, every sandbox grant and every
// in-flight invocation) is gone, and therefore only after the Go callback has
// really returned.
//
// Execution model: the Rust core calls the trampoline synchronously on its own
// managed host thread. The Go callback runs on that same thread; a watcher
// goroutine only cancels the HostContext when the native invocation is
// cancelled or its deadline passes. The watcher is joined before the
// trampoline returns, so it never outlives the borrowed native context token.
// A non-cooperative callback is not detached: it keeps the native outstanding
// accounting and user-data owner, the call reports cancellation, and close
// truthfully reports pending host work until the callback really returns.

/*
#cgo CFLAGS: -I${SRCDIR}/../c/include
#include <stddef.h>
#include <stdint.h>

#include "denosanbox.h"
*/
import "C"

import (
	"context"
	"encoding/json"
	"runtime"
	"runtime/cgo"
	"sync"
	"sync/atomic"
	"time"
	"unsafe"
)

// HostCallback handles one scoped host call. It runs on a core-managed native
// host thread and may block; observe ctx.Done for cooperative cancellation.
// Cancellation cannot roll back side effects that already happened. The
// returned JSON must fit the callback's output limit. A nil result/error is
// forwarded as a typed error; callback errors and panics never reach the guest.
type HostCallback func(ctx HostContext, input json.RawMessage) (json.RawMessage, error)

// HostContext is trusted native metadata plus a cancellable context. The
// sandbox id and scope never come from guest JSON. ctx.Done is closed when the
// native invocation is cancelled/closed or its deadline expires.
type HostContext struct {
	// Ctx is the invocation context; use it for Done(), Deadline() and Err().
	Ctx       context.Context
	SandboxID string
	Scope     string
}

// Done mirrors Ctx.Done for convenience; it is nil if no context is set.
func (h HostContext) Done() <-chan struct{} {
	if h.Ctx == nil {
		return nil
	}
	return h.Ctx.Done()
}

// Err mirrors Ctx.Err.
func (h HostContext) Err() error {
	if h.Ctx == nil {
		return nil
	}
	return h.Ctx.Err()
}

// Deadline mirrors Ctx.Deadline.
func (h HostContext) Deadline() (time.Time, bool) {
	if h.Ctx == nil {
		return time.Time{}, false
	}
	return h.Ctx.Deadline()
}

// Value mirrors Ctx.Value.
func (h HostContext) Value(key any) any {
	if h.Ctx == nil {
		return nil
	}
	return h.Ctx.Value(key)
}

// callbackCheckInterval is how often the in-callback watcher asks native code
// whether the invocation was cancelled or its deadline passed. The watcher
// runs until the Go callback returns.
const callbackCheckInterval = time.Millisecond

const (
	recordLive uint32 = iota
	recordDead
)

type hostRecord struct {
	fn    HostCallback
	state atomic.Uint32 // recordLive -> recordDead
	// released counts exactly-once release observations (test suite only).
	released *atomic.Int32
}

// countRelease is a test hook (no-op in production) run after the handle is
// deleted, i.e. when the user data was really released.
var countRelease = func(*hostRecord) {}

var (
	releasedMu      sync.Mutex
	releasedHandles = map[uintptr]struct{}{}
)

// goContextDepth counts callbacks currently running on Go-entered Rust threads.
var goContextDepth atomic.Int32

// releaseRecord deletes the handle exactly once. Native code fires the foreign
// release once per owner, but this stays idempotent: cgo.Handle.Value must not
// be called after Delete (Go panics), so repeated values are answered from
// releasedHandles without touching cgo state again.
func releaseRecord(h uintptr) {
	releasedMu.Lock()
	_, seen := releasedHandles[h]
	if !seen {
		releasedHandles[h] = struct{}{}
	}
	releasedMu.Unlock()
	if seen {
		return
	}
	rec, ok := cgo.Handle(h).Value().(*hostRecord)
	if !ok || rec == nil || !rec.state.CompareAndSwap(recordLive, recordDead) {
		return
	}
	cgo.Handle(h).Delete()
	if rec.released != nil {
		rec.released.Add(1)
	}
	countRelease(rec)
}

var (
	deferredMu     sync.Mutex
	deferredFails  = map[uintptr]struct{}{}
	deferredEvents atomic.Int64
	releaseDrains  atomic.Int64
)

// deferRelease records a handle whose release must run from Go. It never calls
// cgo.Handle.Value: the record is resolved when the deferred release runs.
func deferRelease(h uintptr) {
	deferredMu.Lock()
	defer deferredMu.Unlock()
	if _, ok := deferredFails[h]; ok {
		return
	}
	deferredFails[h] = struct{}{}
	deferredEvents.Add(1)
}

// runDeferred releases every pending handle from a Go goroutine. Each handle is
// released at most once (native release fires once per owner, and
// releaseRecord is idempotent for a repeated value).
func runDeferred() {
	pending := make([]uintptr, 0, 4)
	deferredMu.Lock()
	for h := range deferredFails {
		delete(deferredFails, h)
		pending = append(pending, h)
	}
	deferredMu.Unlock()
	for _, h := range pending {
		releaseRecord(h)
	}
}

// ReleaseDeferredReleases runs host-callback releases that became pending on a
// Rust thread that could not call back into Go (normally after engine close).
// It executes each deferred release at most once and must be called from a
// normal Go goroutine. A background reaper does the same automatically.
func ReleaseDeferredReleases() {
	releaseDrains.Add(1)
	runDeferred()
}

func init() {
	go func() {
		ticker := time.NewTicker(5 * time.Millisecond)
		defer ticker.Stop()
		var last int64
		for range ticker.C {
			if seen := deferredEvents.Load(); seen != last {
				last = seen
				runDeferred()
			}
		}
	}()
}

// GoDenoHostRelease is the C ABI release function; see callback_shim.c.
//
// Rust calls it once per owner, usually from a Rust-owned teardown thread that
// cgo did not enter from Go, where re-entering Go is not allowed. In that case
// the opaque handle is recorded and replayed from Go instead. Releasing user
// data therefore never happens before the owner really ended, and never on a
// foreign thread.
//
//export GoDenoHostRelease
func GoDenoHostRelease(h C.uint64_t) {
	if goContextDepth.Load() > 0 {
		releaseRecord(uintptr(h))
		return
	}
	deferRelease(uintptr(h))
}

type contextMetadata struct {
	SandboxID   string  `json:"sandbox_id"`
	Scope       string  `json:"scope"`
	RemainingMS *uint64 `json:"remaining_ms"`
	Cancelled   bool    `json:"cancelled"`
}

// nativeContext fetches trusted metadata from the borrowed native context.
// error_out is required by the C ABI and is always supplied and freed.
func nativeContext(token C.uint64_t) (contextMetadata, bool) {
	var meta contextMetadata
	var buffer C.deno_buffer
	var cerr C.deno_error
	status := C.deno_context_json(token, &buffer, &cerr)
	if status != C.DENO_STATUS_OK {
		C.deno_error_free(&cerr)
		return meta, false
	}
	raw := takeBuffer(&buffer)
	if json.Unmarshal(raw, &meta) != nil || meta.SandboxID == "" || meta.Scope == "" {
		return contextMetadata{}, false
	}
	return meta, true
}

// GoDenoHostCallback is the C ABI callback; see callback_shim.c. It never lets
// a Go panic cross into C.
//
//export GoDenoHostCallback
func GoDenoHostCallback(contextToken C.uint64_t, input *C.uint8_t, inputLen C.size_t,
	outResult *C.deno_buffer, h C.uint64_t) (status C.deno_status_t) {
	defer func() {
		if recover() != nil {
			// recover() stopped the unwinding before C resumed; report a generic
			// execution failure and leave out_result untouched.
			status = C.DENO_STATUS_EXECUTION
		}
	}()
	if outResult == nil {
		return C.DENO_STATUS_INVALID_ARGUMENT
	}
	releasedMu.Lock()
	_, released := releasedHandles[uintptr(h)]
	releasedMu.Unlock()
	if released {
		return C.DENO_STATUS_INVALID_ARGUMENT
	}
	rec, ok := cgo.Handle(uintptr(h)).Value().(*hostRecord)
	if !ok || rec == nil || rec.state.Load() != recordLive {
		return C.DENO_STATUS_INVALID_ARGUMENT
	}

	meta, ok := nativeContext(contextToken)
	if !ok {
		return C.DENO_STATUS_INTERNAL
	}
	if meta.Cancelled || C.deno_context_check(contextToken) != C.DENO_STATUS_OK {
		return C.DENO_STATUS_CANCELLED
	}

	payload := make([]byte, int(inputLen))
	if inputLen > 0 {
		if input == nil {
			return C.DENO_STATUS_INVALID_ARGUMENT
		}
		copy(payload, unsafe.Slice((*byte)(unsafe.Pointer(input)), int(inputLen)))
	}
	runtime.KeepAlive(input)

	// ctx is cancelled by the watcher (native cancel/close/deadline) or by the
	// native remaining deadline. Nothing is detached from native accounting.
	base := context.Background()
	if meta.RemainingMS != nil {
		// Honor the native deadline through the context itself so Deadline(),
		// Done() and Err() all agree.
		base = context.Background()
		var deadlineCancel context.CancelFunc
		base, deadlineCancel = context.WithTimeout(base, time.Duration(*meta.RemainingMS)*time.Millisecond)
		defer deadlineCancel()
	}
	ctx, cancel := context.WithCancel(base)
	defer cancel()
	hostCtx := HostContext{Ctx: ctx, SandboxID: meta.SandboxID, Scope: meta.Scope}

	// Watcher: signals cancellation into ctx only, until the callback returns.
	// It is stopped and joined before this function returns, so no native
	// context token outlives it.
	watcherDone := make(chan struct{})
	var watcher sync.WaitGroup
	watcher.Add(1)
	go func() {
		defer watcher.Done()
		ticker := time.NewTicker(callbackCheckInterval)
		defer ticker.Stop()
		for {
			select {
			case <-watcherDone:
				return
			case <-ticker.C:
				if C.deno_context_check(contextToken) != C.DENO_STATUS_OK {
					cancel()
					return
				}
			}
		}
	}()

	// The user callback runs synchronously on this Rust-owned host thread.
	goContextDepth.Add(1)
	var result struct {
		out json.RawMessage
		err error
		ok  bool
	}
	func() {
		defer func() {
			if recover() != nil {
				result.err = nil
				result.ok = false
			}
		}()
		out, err := rec.fn(hostCtx, json.RawMessage(payload))
		result.out, result.err, result.ok = out, err, true
	}()
	close(watcherDone)
	watcher.Wait()
	goContextDepth.Add(-1)

	// Releases that were deferred while this thread was inside cgo are replayed
	// here, on the Go-entered thread.
	runDeferred()

	if !result.ok || result.err != nil || result.out == nil {
		return C.DENO_STATUS_EXECUTION
	}
	out := []byte(result.out)
	var copied C.deno_buffer
	var cerr C.deno_error
	copyStatus := C.deno_buffer_copy(bytesPtr(out), C.size_t(len(out)), &copied, &cerr)
	runtime.KeepAlive(out)
	if copyStatus != C.DENO_STATUS_OK {
		C.deno_error_free(&cerr)
		return copyStatus
	}
	*outResult = copied
	return C.DENO_STATUS_OK
}

// releaseSandboxGrants replays releases for owners that ended with a sandbox
// close. It is called from Close, i.e. from a normal Go goroutine.
func releaseSandboxGrants() {
	ReleaseDeferredReleases()
}
