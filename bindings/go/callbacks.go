package denosanbox

// Exported cgo trampolines for scoped host callbacks.
//
// The C ABI holds only an opaque uintptr_t (a cgo.Handle) as "user data". Each
// handle is recorded in one bounded live-registry entry while its registration
// exists; release removes that entry and deletes the handle exactly once, after
// the native owner (engine registration, every sandbox grant and every
// in-flight invocation) is gone, and therefore only after the Go callback has
// really returned. Nothing about a closed registration is retained.
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

// The live registration registry. It holds exactly the host records that have
// been handed to native code and not yet released, so its size is bounded by
// the number of live registrations, never by the number of closed ones.
//
// Registry mutex > record lock is the only allowed lock order.
var registry = struct {
	sync.Mutex
	records map[uintptr]*hostRecord
}{records: make(map[uintptr]*hostRecord)}

// hookInsertRecord is a test hook (nil in production) run when a record is
// published, before native code sees the handle.
var hookInsertRecord func(*hostRecord)

type hostRecord struct {
	fn HostCallback
	// lock guards released. It is never held across a call into C or Go user
	// code: once released is set, no lookup can reach this record and native
	// code has ended its last owner (and with it any in-flight invocation).
	lock     sync.Mutex
	released bool
	// nativeArc pins the record for the duration of an in-flight C-ABI
	// callback, which only holds the handle integer across the boundary.
	nativeArc atomic.Int32
	// onRelease is a test hook fired once, after cgo.Handle.Delete.
	onRelease func()
}

// liveRegistrationCount reports the number of live registrations (test hook).
func liveRegistrationCount() int {
	registry.Lock()
	defer registry.Unlock()
	return len(registry.records)
}

// insertRecord publishes a new record under an opaque handle. Called before
// the registration reaches native code, so native release can never observe an
// unknown live handle.
func insertRecord(h uintptr, fn HostCallback) *hostRecord {
	rec := &hostRecord{fn: fn}
	if hookInsertRecord != nil {
		hookInsertRecord(rec)
	}
	registry.Lock()
	registry.records[h] = rec
	registry.Unlock()
	return rec
}

// dropRecord removes a record that never reached native code (a rejected
// registration); the caller still owns its user data, so no release happens.
func dropRecord(h uintptr) {
	registry.Lock()
	rec := registry.records[h]
	delete(registry.records, h)
	registry.Unlock()
	if rec != nil {
		rec.lock.Lock()
		rec.released = true
		rec.lock.Unlock()
	}
}

// lookupRecordPinned resolves a live record and pins it with nativeArc. The
// pin keeps the record (and its owner) resolvable for the whole C call; it is
// dropped with unpinRecord once the foreign call returned. A nil record means
// the handle is absent, i.e. already released or never registered.
func lookupRecordPinned(h uintptr) (*hostRecord, bool) {
	registry.Lock()
	defer registry.Unlock()
	rec := registry.records[h]
	if rec == nil {
		return nil, false
	}
	rec.lock.Lock()
	live := !rec.released
	rec.lock.Unlock()
	if !live {
		// Only reachable while a release holds the record lock; treat the
		// handle as already released rather than returning a dying record.
		return nil, false
	}
	rec.nativeArc.Add(1)
	return rec, true
}

func unpinRecord(rec *hostRecord) {
	if rec != nil {
		rec.nativeArc.Add(-1)
	}
}

// releaseRecord answers one native release. The registration is removed from
// the live registry atomically and the cgo handle is deleted exactly once; a
// handle that is no longer live was already released (or was never handed to
// native code) and is ignored. If a lookup is in flight the record lock makes
// this wait for it; native code only calls release after every owner, so no
// foreign call can be running by then.
func releaseRecord(h uintptr) {
	registry.Lock()
	rec := registry.records[h]
	delete(registry.records, h)
	registry.Unlock()
	if rec == nil {
		return
	}
	rec.lock.Lock()
	if rec.released {
		rec.lock.Unlock()
		return
	}
	rec.released = true
	rec.lock.Unlock()
	cgo.Handle(h).Delete()
	if rec.onRelease != nil {
		rec.onRelease()
	}
}

// GoDenoHostRelease is the C ABI release function; see callback_shim.c.
//
// Native code calls it exactly once per registration, usually from a
// Rust-owned teardown thread that did not enter Go. That is an ordinary cgo
// callback (runtime/cgocall.go): the Go runtime binds an M to the calling C
// thread for the duration of the call (runtime/proc.go needm/dropm), so the
// release runs directly here. A panic is contained at this boundary.
//
//export GoDenoHostRelease
func GoDenoHostRelease(h C.uint64_t) {
	defer func() {
		_ = recover()
	}()
	releaseRecord(uintptr(h))
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
	// The pin keeps the live registration (and its owner) resolvable until
	// this foreign call returns, even if native teardown runs concurrently.
	rec, pinned := lookupRecordPinned(uintptr(h))
	if rec == nil {
		return C.DENO_STATUS_INVALID_ARGUMENT
	}
	if pinned {
		defer unpinRecord(rec)
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
