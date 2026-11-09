package denosanbox

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"strconv"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

const hostEchoSource = `
export async function run(input) {
  const host = await sandbox.host.call('echo', input);
  return { host };
}
`

const hostSlowSource = `
export async function run() {
  return sandbox.host.call('slow', null);
}
`

const hostForgedSource = `
export function run() {
  return sandbox.host.call('echo', {scope: 'tenant-forged'});
}
`

func released(counter *atomic.Int32) int32 { return counter.Load() }

func waitFor(t *testing.T, limit time.Duration, condition func() bool) {
	t.Helper()
	deadline := time.Now().Add(limit)
	for time.Now().Before(deadline) {
		if condition() {
			return
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatalf("condition not met within %s", limit)
}

// TestHostCallbacksOwnershipScopeAndSamePID proves trusted metadata, same-PID
// execution, ungranted rejection and exactly-once release after engine close.
func TestHostCallbacksOwnershipScopeAndSamePID(t *testing.T) {
	engine, err := NewWithCapabilities([]string{"echo"})
	if err != nil {
		t.Fatalf("NewWithCapabilities: %v", err)
	}
	self := os.Getpid()
	var echoed atomic.Int32
	var callbackPID atomic.Int32
	echo := func(ctx HostContext, input json.RawMessage) (json.RawMessage, error) {
		if ctx.SandboxID == "" || ctx.Scope == "" {
			return nil, errors.New("missing trusted metadata")
		}
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		default:
		}
		echoed.Add(1)
		callbackPID.Store(int32(os.Getpid()))
		return json.Marshal(map[string]any{
			"sandbox_id": ctx.SandboxID,
			"scope":      ctx.Scope,
			"input":      json.RawMessage(input),
		})
	}
	echoReleased := watchRelease(echo)
	if err := engine.RegisterHost("echo", echo, 4096, 4096); err != nil {
		t.Fatalf("RegisterHost: %v", err)
	}
	if err := engine.RegisterHost("echo", echo, 4096, 4096); err == nil {
		t.Fatalf("duplicate RegisterHost succeeded")
	}
	if err := engine.RegisterHost("bad name", echo, 4096, 4096); err == nil {
		t.Fatalf("invalid RegisterHost name succeeded")
	}
	if released(echoReleased) != 0 {
		t.Fatalf("failed registrations released user data")
	}

	// A deny-all engine never grants, even with a registration present.
	plain, err := New()
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	if _, err := plain.CreateWithGrants(hostEchoSource, map[string]string{"echo": "tenant-x"}); err == nil {
		t.Fatalf("deny-all engine granted a capability")
	}
	if _, err := plain.Close(); err != nil {
		t.Fatalf("plain close: %v", err)
	}

	alpha, err := engine.CreateWithGrants(hostEchoSource, map[string]string{"echo": "tenant-alpha"})
	if err != nil {
		t.Fatalf("alpha CreateWithGrants: %v", err)
	}
	beta, err := engine.CreateWithGrants(hostEchoSource, map[string]string{"echo": "tenant-beta"})
	if err != nil {
		t.Fatalf("beta CreateWithGrants: %v", err)
	}
	bare, err := engine.Create(hostForgedSource)
	if err != nil {
		t.Fatalf("bare create: %v", err)
	}
	if _, err := engine.CreateWithGrants(hostEchoSource, map[string]string{"missing": "x"}); err == nil {
		t.Fatalf("grant of unregistered callback succeeded")
	}

	forged := json.RawMessage(`{"scope":"tenant-forged","pid":` + strconv.Itoa(self) + `}`)
	out, err := alpha.Call(context.Background(), "run", forged, 10*time.Second)
	if err != nil {
		t.Fatalf("alpha call: %v", err)
	}
	var parsed struct {
		Host struct {
			SandboxID string          `json:"sandbox_id"`
			Scope     string          `json:"scope"`
			Input     json.RawMessage `json:"input"`
		} `json:"host"`
	}
	if err := json.Unmarshal(out, &parsed); err != nil {
		t.Fatalf("alpha result: %v (%s)", err, out)
	}
	if parsed.Host.Scope != "tenant-alpha" {
		t.Fatalf("trusted scope = %q, want tenant-alpha", parsed.Host.Scope)
	}
	if !strings.Contains(string(parsed.Host.Input), "tenant-forged") {
		t.Fatalf("guest input should pass through unchanged: %s", parsed.Host.Input)
	}
	if parsed.Host.SandboxID == "" {
		t.Fatalf("missing trusted sandbox id")
	}
	if got := int(callbackPID.Load()); got != self {
		t.Fatalf("callback ran in PID %d, want %d", got, self)
	}

	out, err = beta.Call(context.Background(), "run", json.RawMessage("null"), 10*time.Second)
	if err != nil {
		t.Fatalf("beta call: %v", err)
	}
	if err := json.Unmarshal(out, &parsed); err != nil || parsed.Host.Scope != "tenant-beta" {
		t.Fatalf("beta scope: %v (%s)", err, out)
	}

	if _, err := bare.Call(context.Background(), "run", json.RawMessage("null"), 10*time.Second); err == nil {
		t.Fatalf("ungranted host call succeeded")
	} else if nerr, ok := err.(*Error); ok && nerr.Status != StatusExecution && nerr.Status != StatusInvalidArgument {
		t.Fatalf("unexpected ungranted status %v", nerr)
	}

	for name, sb := range map[string]*Sandbox{"alpha": alpha, "beta": beta, "bare": bare} {
		if _, err := sb.Close(); err != nil {
			t.Fatalf("%s close: %v", name, err)
		}
	}
	if got := released(echoReleased); got != 0 {
		t.Fatalf("release count after sandbox close = %d, want 0", got)
	}

	if _, err := engine.Close(); err != nil {
		t.Fatalf("engine close: %v", err)
	}
	waitFor(t, 2*time.Second, func() bool { return released(echoReleased) == 1 })
	if _, err := engine.Close(); err != nil {
		t.Fatalf("second close: %v", err)
	}
	time.Sleep(20 * time.Millisecond)
	if got := released(echoReleased); got != 1 {
		t.Fatalf("release count after repeated close = %d, want 1", got)
	}
	if echoed.Load() != 2 {
		t.Fatalf("echo callback invocations = %d", echoed.Load())
	}
}

// TestHostCallbackLongRunningCancelSignalsContext covers requirement 1: a
// callback longer than 5ms must still see cancellation that arrives later, and
// prompt cancellation is observed cooperatively.
func TestHostCallbackLongRunningCancelSignalsContext(t *testing.T) {
	engine, err := NewWithCapabilities([]string{"slow"})
	if err != nil {
		t.Fatalf("NewWithCapabilities: %v", err)
	}
	started := make(chan struct{})
	observed := make(chan time.Duration, 1)
	exited := make(chan struct{})
	slow := func(ctx HostContext, input json.RawMessage) (json.RawMessage, error) {
		begin := time.Now()
		close(started)
		select {
		case <-ctx.Done():
			observed <- time.Since(begin)
			close(exited)
			return nil, ctx.Err()
		case <-time.After(30 * time.Second):
			close(exited)
			return json.RawMessage("null"), nil
		}
	}
	slowReleased := watchRelease(slow)
	if err := engine.RegisterHost("slow", slow, 1024, 1024); err != nil {
		t.Fatalf("RegisterHost: %v", err)
	}
	sandbox, err := engine.CreateWithGrants(hostSlowSource, map[string]string{"slow": "tenant-slow"})
	if err != nil {
		t.Fatalf("CreateWithGrants: %v", err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		_, err := sandbox.Call(ctx, "run", json.RawMessage("null"), 20*time.Second)
		done <- err
	}()
	select {
	case <-started:
	case <-time.After(10 * time.Second):
		t.Fatalf("callback never entered")
	}
	// Wait well past the old 5ms monitor bound before cancelling.
	time.Sleep(50 * time.Millisecond)
	select {
	case <-observed:
		t.Fatalf("callback observed cancellation before it was requested")
	default:
	}
	cancel()
	select {
	case err := <-done:
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("call error = %v, want context.Canceled", err)
		}
	case <-time.After(10 * time.Second):
		t.Fatalf("call did not return after cancel")
	}
	select {
	case waited := <-observed:
		if waited > 2*time.Second {
			t.Fatalf("callback observed cancellation only after %s", waited)
		}
	case <-time.After(10 * time.Second):
		t.Fatalf("callback never observed cancellation after cancel")
	}
	<-exited
	if _, err := sandbox.Close(); err != nil {
		t.Fatalf("sandbox close: %v", err)
	}
	if _, err := engine.Close(); err != nil {
		t.Fatalf("engine close: %v", err)
	}
	waitFor(t, 2*time.Second, func() bool { return released(slowReleased) == 1 })
}

// TestHostCallbackNativeDeadlineHonored checks the native remaining deadline is
// turned into a Go context deadline even for a callback blocking on ctx.
func TestHostCallbackNativeDeadlineHonored(t *testing.T) {
	engine, err := NewWithCapabilities([]string{"slow"})
	if err != nil {
		t.Fatalf("NewWithCapabilities: %v", err)
	}
	defer engine.Close()
	done := make(chan error, 1)
	slow := func(ctx HostContext, input json.RawMessage) (json.RawMessage, error) {
		dl, ok := ctx.Deadline()
		if !ok || time.Until(dl) > 5*time.Second {
			done <- errors.New("missing native-derived deadline")
			return nil, errors.New("missing deadline")
		}
		<-ctx.Done()
		done <- ctx.Err()
		return nil, ctx.Err()
	}
	if err := engine.RegisterHost("slow", slow, 1024, 1024); err != nil {
		t.Fatalf("RegisterHost: %v", err)
	}
	sandbox, err := engine.CreateWithGrants(hostSlowSource, map[string]string{"slow": "tenant-slow"})
	if err != nil {
		t.Fatalf("CreateWithGrants: %v", err)
	}
	_, err = sandbox.Call(context.Background(), "run", json.RawMessage("null"), 300*time.Millisecond)
	if err == nil {
		t.Fatalf("deadline call succeeded")
	}
	select {
	case err := <-done:
		// The invocation may end through the native deadline or through the
		// job's cancellation; both must unblock the contextual callback.
		if !errors.Is(err, context.DeadlineExceeded) && !errors.Is(err, context.Canceled) {
			t.Fatalf("callback context err = %v, want deadline/canceled", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatalf("callback never observed the deadline")
	}
	if _, err := sandbox.Close(); err != nil {
		t.Fatalf("sandbox close: %v", err)
	}
}

// TestHostCallbackNonCooperativeStaysAccounted covers requirement 2: a gated
// (non-cooperative) callback stays counted by native accounting after the call
// returns on its native deadline, close reports clean=0, and the native user
// data is released only after the callback really returns.
func TestHostCallbackNonCooperativeStaysAccounted(t *testing.T) {
	engine, err := NewWithCapabilities([]string{"slow"})
	if err != nil {
		t.Fatalf("NewWithCapabilities: %v", err)
	}
	baseLive := liveRegistrationCount()
	started := make(chan struct{})
	gate := make(chan struct{})
	slow := func(ctx HostContext, input json.RawMessage) (json.RawMessage, error) {
		close(started)
		<-gate
		return json.RawMessage("null"), nil
	}
	slowReleased := watchRelease(slow)
	if err := engine.RegisterHost("slow", slow, 1024, 1024); err != nil {
		t.Fatalf("RegisterHost: %v", err)
	}
	sandbox, err := engine.CreateWithGrants(hostSlowSource, map[string]string{"slow": "tenant-slow"})
	if err != nil {
		t.Fatalf("CreateWithGrants: %v", err)
	}
	callDone := make(chan error, 1)
	go func() {
		// The native deadline ends the call while the callback is still gated.
		_, err := sandbox.Call(context.Background(), "run", json.RawMessage("null"), 200*time.Millisecond)
		callDone <- err
	}()
	select {
	case <-started:
	case <-time.After(10 * time.Second):
		t.Fatalf("callback never entered")
	}
	// Wait well past the old 5ms bound before the call ends.
	time.Sleep(50 * time.Millisecond)
	// Close from a separate goroutine (as a host would); the Call goroutine is
	// still polling its job.
	closed := make(chan struct{})
	var report SandboxCloseReport
	var closeErr error
	go func() {
		report, closeErr = sandbox.Close()
		close(closed)
	}()
	select {
	case <-closed:
	case <-time.After(30 * time.Second):
		t.Fatalf("sandbox close did not return")
	}
	if closeErr != nil {
		t.Fatalf("sandbox close: %v", closeErr)
	}
	if err := <-callDone; err == nil {
		t.Fatalf("call succeeded while the callback was still gated")
	}
	if report.ClosedCleanly {
		t.Fatalf("close reported clean while a non-cooperative callback still runs")
	}
	if report.PendingHostOperations == 0 {
		t.Fatalf("close reported no pending host work while the callback still runs")
	}
	if got := released(slowReleased); got != 0 {
		t.Fatalf("user data released while callback still runs: %d", got)
	}
	if got := liveRegistrationCount(); got != baseLive+1 {
		t.Fatalf("live registrations while callback runs = %d, want %d", got, baseLive+1)
	}

	close(gate)
	// Registration still owns the callback after its invocation returns.
	// Closing the engine releases that final owner after native work drains.
	if _, err := engine.Close(); err != nil {
		t.Fatalf("engine close: %v", err)
	}
	waitFor(t, 2*time.Second, func() bool { return released(slowReleased) == 1 })
	if got := liveRegistrationCount(); got != baseLive {
		t.Fatalf("live registrations after teardown = %d, want %d", got, baseLive)
	}
}

// TestHostCallbackPanicContained ensures a panicking callback becomes a typed
// guest error and cannot unwind into C.
func TestHostCallbackPanicContained(t *testing.T) {
	engine, err := NewWithCapabilities([]string{"echo"})
	if err != nil {
		t.Fatalf("NewWithCapabilities: %v", err)
	}
	var echoReleased *atomic.Int32
	defer func() {
		if _, err := engine.Close(); err != nil {
			t.Fatalf("engine close: %v", err)
		}
		if echoReleased != nil {
			waitFor(t, 2*time.Second, func() bool { return released(echoReleased) == 1 })
		}
		_ = echoReleased
	}()
	panicky := func(ctx HostContext, input json.RawMessage) (json.RawMessage, error) {
		panic("secret panic text")
	}
	echoReleased = watchRelease(panicky)
	if err := engine.RegisterHost("echo", panicky, 1024, 1024); err != nil {
		t.Fatalf("RegisterHost: %v", err)
	}
	sandbox, err := engine.CreateWithGrants(hostEchoSource, map[string]string{"echo": "tenant-panic"})
	if err != nil {
		t.Fatalf("CreateWithGrants: %v", err)
	}
	_, err = sandbox.Call(context.Background(), "run", json.RawMessage("null"), 5*time.Second)
	if err == nil {
		t.Fatalf("panicking callback succeeded")
	}
	if strings.Contains(err.Error(), "secret panic text") {
		t.Fatalf("panic text leaked: %v", err)
	}
	if _, err := sandbox.Close(); err != nil {
		t.Fatalf("sandbox close: %v", err)
	}
}

// TestHostRegistrationRegistryBounded covers requirement 2: repeated
// register+close cycles return the live-registration count to zero, release the
// native user data exactly once per registration, and retain no closed handles.
func TestHostRegistrationRegistryBounded(t *testing.T) {
	const cycles = 32
	echo := func(ctx HostContext, input json.RawMessage) (json.RawMessage, error) {
		return json.RawMessage("null"), nil
	}
	base := liveRegistrationCount()
	for i := 0; i < cycles; i++ {
		counter := watchRelease(echo)
		engine, err := NewWithCapabilities([]string{"echo"})
		if err != nil {
			t.Fatalf("cycle %d NewWithCapabilities: %v", i, err)
		}
		if err := engine.RegisterHost("echo", echo, 1024, 1024); err != nil {
			t.Fatalf("cycle %d RegisterHost: %v", i, err)
		}
		if got := liveRegistrationCount(); got != base+1 {
			t.Fatalf("cycle %d live registrations = %d, want %d", i, got, base+1)
		}
		sandbox, err := engine.CreateWithGrants(hostEchoSource, map[string]string{"echo": "tenant-cycle"})
		if err != nil {
			t.Fatalf("cycle %d CreateWithGrants: %v", i, err)
		}
		if _, err := sandbox.Call(context.Background(), "run", json.RawMessage("null"), 10*time.Second); err != nil {
			// hostEchoSource returns the callback result; echo returns null and
			// the guest wraps it, so a successful call is expected.
			t.Fatalf("cycle %d call: %v", i, err)
		}
		if _, err := sandbox.Close(); err != nil {
			t.Fatalf("cycle %d sandbox close: %v", i, err)
		}
		if _, err := engine.Close(); err != nil {
			t.Fatalf("cycle %d engine close: %v", i, err)
		}
		waitFor(t, 2*time.Second, func() bool { return released(counter) == 1 })
		if got := released(counter); got != 1 {
			t.Fatalf("cycle %d release count = %d, want exactly 1", i, got)
		}
		if got := liveRegistrationCount(); got != base {
			t.Fatalf("cycle %d live registrations after close = %d, want %d", i, got, base)
		}
	}
}
