// Package denosanbox is a thin cgo adapter over the experimental denosanbox C
// ABI. It owns no JavaScript runtime, policy or sandbox implementation: every
// operation is delegated to the native C library built from bindings/c.
//
// Async completion is polling-only. Scoped host callbacks are exposed through
// RegisterHost/CreateWithGrants; they run on core-managed native host threads
// and never receive a Go pointer through Rust (only an opaque cgo.Handle).
package denosanbox

/*
#cgo CFLAGS: -I${SRCDIR}/../c/include
#cgo LDFLAGS: -L${SRCDIR}/../c/target/debug -ldenosanbox_c -Wl,-rpath,${SRCDIR}/../c/target/debug
#include <stdlib.h>
#include "denosanbox.h"

// Defined in callback_shim.c, implemented in callbacks.go.
deno_host_callback_fn denosanbox_callback_fn(void);
deno_host_release_fn denosanbox_release_fn(void);
*/
import "C"

import (
	"context"
	"encoding/json"
	"fmt"
	"runtime"
	"runtime/cgo"
	"sync"
	"sync/atomic"
	"time"
	"unsafe"
)

// Status is a typed C ABI result.
type Status int32

const (
	StatusOK              Status = Status(C.DENO_STATUS_OK)
	StatusPending         Status = Status(C.DENO_STATUS_PENDING)
	StatusInvalidArgument Status = Status(C.DENO_STATUS_INVALID_ARGUMENT)
	StatusInvalidHandle   Status = Status(C.DENO_STATUS_INVALID_HANDLE)
	StatusClosed          Status = Status(C.DENO_STATUS_CLOSED)
	StatusCancelled       Status = Status(C.DENO_STATUS_CANCELLED)
	StatusDeadline        Status = Status(C.DENO_STATUS_DEADLINE)
	StatusLimit           Status = Status(C.DENO_STATUS_LIMIT)
	StatusExecution       Status = Status(C.DENO_STATUS_EXECUTION)
	StatusUnsupported     Status = Status(C.DENO_STATUS_UNSUPPORTED)
	StatusInternal        Status = Status(C.DENO_STATUS_INTERNAL)
)

func (s Status) String() string {
	switch s {
	case StatusOK:
		return "ok"
	case StatusPending:
		return "pending"
	case StatusInvalidArgument:
		return "invalid_argument"
	case StatusInvalidHandle:
		return "invalid_handle"
	case StatusClosed:
		return "closed"
	case StatusCancelled:
		return "cancelled"
	case StatusDeadline:
		return "deadline"
	case StatusLimit:
		return "limit"
	case StatusExecution:
		return "execution"
	case StatusUnsupported:
		return "unsupported"
	case StatusInternal:
		return "internal"
	default:
		return fmt.Sprintf("unknown(%d)", int32(s))
	}
}

// Error is a typed native core failure. Message is owned by Go after extraction.
type Error struct {
	Status  Status
	Message string
}

func (e *Error) Error() string {
	if e == nil {
		return "<nil>"
	}
	if e.Message == "" {
		return "denosanbox: " + e.Status.String()
	}
	return "denosanbox " + e.Status.String() + ": " + e.Message
}

// ABIVersion reports the C ABI version (currently 1).
func ABIVersion() uint32 {
	return uint32(C.deno_abi_version())
}

// PackageVersion reports the independently versioned binding package version.
func PackageVersion() string {
	return C.GoString(C.deno_package_version())
}

// Engine owns one native engine token.
type Engine struct {
	token       atomic.Uint64
	closeOnce   sync.Once
	closeReport EngineShutdownReport
	closeErr    error
}

// New creates an engine with core defaults (the real Deno backend).
func New() (*Engine, error) {
	var token C.uint64_t
	var cerr C.deno_error
	status := C.deno_engine_new(&token, &cerr)
	if status != C.DENO_STATUS_OK {
		return nil, takeError(&cerr)
	}
	if token == 0 {
		return nil, &Error{Status: StatusInternal, Message: "engine constructor returned token 0"}
	}
	engine := &Engine{}
	engine.token.Store(uint64(token))
	return engine, nil
}

// NewWithCapabilities creates an engine with a fixed capability-name ceiling.
// Registration alone still grants nothing: each sandbox needs CreateWithGrants.
func NewWithCapabilities(capabilities []string) (*Engine, error) {
	if capabilities == nil {
		capabilities = []string{}
	}
	encoded, err := json.Marshal(capabilities)
	if err != nil {
		return nil, &Error{Status: StatusInvalidArgument, Message: "capabilities: " + err.Error()}
	}
	if len(encoded) > int(C.DENO_MAX_CAPABILITIES_BYTES) {
		return nil, &Error{Status: StatusLimit, Message: "capability list exceeds 1MiB"}
	}
	var token C.uint64_t
	var cerr C.deno_error
	status := C.deno_engine_new_with_capabilities(bytesPtr(encoded), C.size_t(len(encoded)), &token, &cerr)
	runtime.KeepAlive(encoded)
	if status != C.DENO_STATUS_OK {
		return nil, takeError(&cerr)
	}
	if token == 0 {
		return nil, &Error{Status: StatusInternal, Message: "engine constructor returned token 0"}
	}
	engine := &Engine{}
	engine.token.Store(uint64(token))
	return engine, nil
}

// RegisterHost registers a scoped host callback on an engine created with
// NewWithCapabilities. inputLimit/outputLimit bound the JSON input and output
// (1..=64 MiB). Ownership of the Go callback transfers only on success; on
// failure the cgo.Handle is deleted here. Release happens exactly once after the
// engine registration, every grant and every in-flight invocation are gone.
func (e *Engine) RegisterHost(name string, fn HostCallback, inputLimit, outputLimit int) error {
	if e == nil || e.token.Load() == 0 {
		return &Error{Status: StatusInvalidHandle, Message: "engine is nil or freed"}
	}
	if fn == nil {
		return &Error{Status: StatusInvalidArgument, Message: "host callback is nil"}
	}
	if len(name) == 0 || len(name) > int(C.DENO_MAX_CALLBACK_NAME_BYTES) {
		return &Error{Status: StatusInvalidArgument, Message: "callback name must be 1..64 bytes"}
	}
	if inputLimit <= 0 || outputLimit <= 0 ||
		uint64(inputLimit) > uint64(C.DENO_MAX_CALLBACK_EDGE_BYTES) ||
		uint64(outputLimit) > uint64(C.DENO_MAX_CALLBACK_EDGE_BYTES) {
		return &Error{Status: StatusLimit, Message: "callback limits must be 1..=64MiB"}
	}
	// The handle is registered in the live registry before native code sees it,
	// so a native release can never observe an unknown live handle. Only live
	// registrations are retained; release removes the entry and deletes the
	// handle exactly once (from the Rust-owned thread that calls Go back).
	handle := cgo.NewHandle(&hostRecord{fn: fn})
	insertRecord(uintptr(handle), fn)
	registered := false
	defer func() {
		if !registered {
			dropRecord(uintptr(handle))
			handle.Delete()
		}
	}()
	nameBytes := []byte(name)
	var cerr C.deno_error
	status := C.deno_engine_register_host(
		C.uint64_t(e.token.Load()),
		bytesPtr(nameBytes),
		C.size_t(len(nameBytes)),
		C.size_t(inputLimit),
		C.size_t(outputLimit),
		C.denosanbox_callback_fn(),
		C.denosanbox_release_fn(),
		C.uint64_t(handle),
		&cerr,
	)
	runtime.KeepAlive(nameBytes)
	if status != C.DENO_STATUS_OK {
		err := takeError(&cerr)
		C.deno_error_free(&cerr)
		return err
	}
	registered = true
	runtime.KeepAlive(fn)
	return nil
}

// Create prepares and creates a sandbox from one JavaScript module named
// "main.js". The source is copied before this call returns.
func (e *Engine) Create(source string) (*Sandbox, error) {
	return e.CreateBytes([]byte(source))
}

// CreateBytes is the byte-slice form of Create.
func (e *Engine) CreateBytes(source []byte) (*Sandbox, error) {
	if e == nil || e.token.Load() == 0 {
		return nil, &Error{Status: StatusInvalidHandle, Message: "engine is nil or freed"}
	}
	var token C.uint64_t
	var cerr C.deno_error
	status := C.deno_engine_create_sandbox(
		C.uint64_t(e.token.Load()),
		bytesPtr(source),
		C.size_t(len(source)),
		&token,
		&cerr,
	)
	runtime.KeepAlive(source)
	if status != C.DENO_STATUS_OK {
		return nil, takeError(&cerr)
	}
	if token == 0 {
		return nil, &Error{Status: StatusInternal, Message: "sandbox constructor returned token 0"}
	}
	sandbox := &Sandbox{}
	sandbox.token.Store(uint64(token))
	return sandbox, nil
}

// CreateWithGrants creates a sandbox whose only permitted capabilities are the
// granted host callbacks, mapped to trusted scopes. Names resolve against the
// engine's registrations and fixed ceiling; the guest source never contributes a
// grant. A nil/empty map behaves like Create.
func (e *Engine) CreateWithGrants(source string, grants map[string]string) (*Sandbox, error) {
	if e == nil || e.token.Load() == 0 {
		return nil, &Error{Status: StatusInvalidHandle, Message: "engine is nil or freed"}
	}
	if grants == nil {
		grants = map[string]string{}
	}
	encoded, err := json.Marshal(grants)
	if err != nil {
		return nil, &Error{Status: StatusInvalidArgument, Message: "grants: " + err.Error()}
	}
	if len(encoded) > int(C.DENO_MAX_GRANTS_BYTES) {
		return nil, &Error{Status: StatusLimit, Message: "grant list exceeds 1MiB"}
	}
	sourceBytes := []byte(source)
	var token C.uint64_t
	var cerr C.deno_error
	status := C.deno_engine_create_sandbox_with_grants(
		C.uint64_t(e.token.Load()),
		bytesPtr(sourceBytes),
		C.size_t(len(sourceBytes)),
		bytesPtr(encoded),
		C.size_t(len(encoded)),
		&token,
		&cerr,
	)
	runtime.KeepAlive(sourceBytes)
	runtime.KeepAlive(encoded)
	if status != C.DENO_STATUS_OK {
		return nil, takeError(&cerr)
	}
	if token == 0 {
		return nil, &Error{Status: StatusInternal, Message: "sandbox constructor returned token 0"}
	}
	sandbox := &Sandbox{}
	sandbox.token.Store(uint64(token))
	return sandbox, nil
}

// EngineShutdownReport mirrors the core shutdown report.
type EngineShutdownReport struct {
	SandboxesClosed uint64
	SandboxesForced uint64
	Clean           bool
}

// Close awaits the native shutdown path, then releases the engine token.
// A non-clean report is returned without adding a synthetic error; callers can
// inspect both fields. The engine is unusable after Close.
func (e *Engine) Close() (EngineShutdownReport, error) {
	if e == nil {
		return EngineShutdownReport{}, &Error{Status: StatusInvalidHandle, Message: "nil engine"}
	}
	e.closeOnce.Do(func() { e.closeReport, e.closeErr = e.closeNative() })
	return e.closeReport, e.closeErr
}

func (e *Engine) closeNative() (EngineShutdownReport, error) {
	var report EngineShutdownReport
	if e == nil || e.token.Load() == 0 {
		return report, &Error{Status: StatusInvalidHandle, Message: "engine is nil or already freed"}
	}
	token := C.uint64_t(e.token.Swap(0))

	var raw C.deno_engine_shutdown_report
	var cerr C.deno_error
	closeStatus := C.deno_engine_close(token, &raw, &cerr)
	closeErr := takeError(&cerr)

	freeStatus := C.deno_engine_free(token, &cerr)
	freeErr := takeError(&cerr)

	report = EngineShutdownReport{
		SandboxesClosed: uint64(raw.sandboxes_closed),
		SandboxesForced: uint64(raw.sandboxes_forced),
		Clean:           raw.clean != 0,
	}
	if closeErr != nil {
		return report, closeErr
	}
	if closeStatus != C.DENO_STATUS_OK {
		return report, &Error{Status: Status(closeStatus), Message: "engine close failed"}
	}
	if freeErr != nil {
		return report, freeErr
	}
	if freeStatus != C.DENO_STATUS_OK {
		return report, &Error{Status: Status(freeStatus), Message: "engine free failed"}
	}
	return report, nil
}

// Sandbox is one native sandbox token.
type Sandbox struct {
	token       atomic.Uint64
	closeOnce   sync.Once
	closeReport SandboxCloseReport
	closeErr    error
}

// Call invokes an ESM export with a JSON value. It submits a native job and
// polls with a bounded wait. Context cancellation issues the core's terminal
// sandbox cancellation; no callback API is invented.
func (s *Sandbox) Call(
	ctx context.Context,
	export string,
	input json.RawMessage,
	timeout time.Duration,
) (json.RawMessage, error) {
	if s == nil || s.token.Load() == 0 {
		return nil, &Error{Status: StatusInvalidHandle, Message: "sandbox is nil or freed"}
	}
	if ctx == nil {
		ctx = context.Background()
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if input == nil {
		input = json.RawMessage("null")
	}

	if timeout < 0 {
		return nil, &Error{Status: StatusInvalidArgument, Message: "timeout must be nonnegative"}
	}
	if deadline, ok := ctx.Deadline(); ok {
		remaining := time.Until(deadline)
		if remaining <= 0 {
			return nil, context.DeadlineExceeded
		}
		if timeout == 0 || remaining < timeout {
			timeout = remaining
		}
	}
	var timeoutMS uint64
	if timeout > 0 {
		timeoutMS = uint64(timeout / time.Millisecond)
		if timeout%time.Millisecond != 0 {
			timeoutMS++
		}
	}

	exportBytes := []byte(export)
	var job C.uint64_t
	var cerr C.deno_error
	status := C.deno_sandbox_call_submit(
		C.uint64_t(s.token.Load()),
		bytesPtr(exportBytes),
		C.size_t(len(exportBytes)),
		bytesPtr(input),
		C.size_t(len(input)),
		C.uint64_t(timeoutMS),
		&job,
		&cerr,
	)
	runtime.KeepAlive(exportBytes)
	runtime.KeepAlive(input)
	if status != C.DENO_STATUS_OK {
		return nil, takeError(&cerr)
	}
	if job == 0 {
		return nil, &Error{Status: StatusInternal, Message: "call submit returned job token 0"}
	}
	defer func() {
		C.deno_job_free(job)
	}()

	for {
		if err := ctx.Err(); err != nil {
			C.deno_job_cancel(job)
			return nil, err
		}
		var result C.deno_buffer
		status = C.deno_job_wait(job, C.uint64_t(50), &result, &cerr)
		if err := ctx.Err(); err != nil {
			if result.token != 0 {
				C.deno_buffer_free(&result)
			}
			C.deno_error_free(&cerr)
			C.deno_job_cancel(job)
			return nil, err
		}
		switch status {
		case C.DENO_STATUS_OK:
			bytes := takeBuffer(&result)
			return json.RawMessage(bytes), nil
		case C.DENO_STATUS_PENDING:
			if err := ctx.Err(); err != nil {
				C.deno_job_cancel(job)
				return nil, err
			}
			continue
		default:
			return nil, takeError(&cerr)
		}
	}
}

// Cancel terminates the sandbox using core cancellation semantics. It is
// terminal; queued and later calls fail with StatusClosed.
func (s *Sandbox) Cancel() error {
	if s == nil || s.token.Load() == 0 {
		return &Error{Status: StatusInvalidHandle, Message: "sandbox is nil or freed"}
	}
	status := C.deno_sandbox_cancel(C.uint64_t(s.token.Load()))
	if status != C.DENO_STATUS_OK {
		return &Error{Status: Status(status), Message: "sandbox cancel failed"}
	}
	return nil
}

// SandboxCloseReport mirrors the core close report.
type SandboxCloseReport struct {
	InvocationsCompleted uint64
	InvocationsCancelled uint64
	ForcedTermination    bool
	ClosedCleanly        bool
	// PendingHostOperations counts native host callbacks still running after
	// close. Nonzero means a callback has not returned; its user data is
	// released only after it really ends.
	PendingHostOperations uint64
}

// Close awaits the native core close path, then frees the sandbox token.
// A non-clean report is returned without a synthetic error.
func (s *Sandbox) Close() (SandboxCloseReport, error) {
	if s == nil {
		return SandboxCloseReport{}, &Error{Status: StatusInvalidHandle, Message: "nil sandbox"}
	}
	s.closeOnce.Do(func() { s.closeReport, s.closeErr = s.closeNative() })
	return s.closeReport, s.closeErr
}

func (s *Sandbox) closeNative() (SandboxCloseReport, error) {
	var report SandboxCloseReport
	if s == nil || s.token.Load() == 0 {
		return report, &Error{Status: StatusInvalidHandle, Message: "sandbox is nil or already freed"}
	}
	token := C.uint64_t(s.token.Swap(0))

	var raw C.deno_sandbox_close_report
	var cerr C.deno_error
	closeStatus := C.deno_sandbox_close(token, &raw, &cerr)
	closeErr := takeError(&cerr)

	freeStatus := C.deno_sandbox_free(token, &cerr)
	freeErr := takeError(&cerr)

	var pending C.uint64_t
	if C.deno_sandbox_pending_host_operations(token, &pending) != C.DENO_STATUS_OK {
		pending = 0
	}

	// A successful close ends this sandbox's grant owners natively; a
	// non-cooperative callback keeps its own owner, so its user data (and the
	// Go registration record) is released only after it really returns.

	report = SandboxCloseReport{
		PendingHostOperations: uint64(pending),
		InvocationsCompleted:  uint64(raw.invocations_completed),
		InvocationsCancelled:  uint64(raw.invocations_cancelled),
		ForcedTermination:     raw.forced_termination != 0,
		ClosedCleanly:         raw.closed_cleanly != 0,
	}
	if closeErr != nil {
		return report, closeErr
	}
	if closeStatus != C.DENO_STATUS_OK {
		return report, &Error{Status: Status(closeStatus), Message: "sandbox close failed"}
	}
	if freeErr != nil {
		return report, freeErr
	}
	if freeStatus != C.DENO_STATUS_OK {
		return report, &Error{Status: Status(freeStatus), Message: "sandbox free failed"}
	}
	return report, nil
}

func bytesPtr(bytes []byte) *C.uint8_t {
	if len(bytes) == 0 {
		return nil
	}
	return (*C.uint8_t)(unsafe.Pointer(&bytes[0]))
}

func takeBuffer(buffer *C.deno_buffer) []byte {
	if buffer == nil || buffer.token == 0 {
		return nil
	}
	var out []byte
	if buffer.len > 0 && buffer.data != nil {
		out = C.GoBytes(unsafe.Pointer(buffer.data), C.int(buffer.len))
	}
	C.deno_buffer_free(buffer)
	return out
}

func takeError(cerr *C.deno_error) error {
	if cerr == nil {
		return &Error{Status: StatusInternal, Message: "missing native error output"}
	}
	code := Status(cerr.code)
	if code == StatusOK {
		C.deno_error_free(cerr)
		return nil
	}
	message := string(takeBuffer(&cerr.message))
	C.deno_error_free(cerr)
	if message == "" {
		message = code.String()
	}
	return &Error{Status: code, Message: message}
}
