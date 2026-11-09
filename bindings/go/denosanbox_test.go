package denosanbox

import (
	"context"
	"encoding/json"
	"errors"
	"testing"
	"time"
)

func TestVersions(t *testing.T) {
	if got := ABIVersion(); got != 1 {
		t.Fatalf("ABIVersion() = %d, want 1", got)
	}
	if got := PackageVersion(); got != "0.1.0" {
		t.Fatalf("PackageVersion() = %q, want 0.1.0", got)
	}
}

func TestTwoSandboxesTypedErrorsAndClose(t *testing.T) {
	engine, err := New()
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	defer func() { _, _ = engine.Close() }()

	const source = `
let state = 0;
export function inc() { state += 1; return state; }
export function get() { return state; }
`
	first, err := engine.Create(source)
	if err != nil {
		t.Fatalf("first Create: %v", err)
	}
	defer func() { _, _ = first.Close() }()
	second, err := engine.Create(source)
	if err != nil {
		t.Fatalf("second Create: %v", err)
	}
	defer func() { _, _ = second.Close() }()

	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	call := func(sandbox *Sandbox, export string) json.RawMessage {
		t.Helper()
		result, err := sandbox.Call(ctx, export, json.RawMessage("null"), 5*time.Second)
		if err != nil {
			t.Fatalf("%s: %v", export, err)
		}
		return result
	}

	if got := string(call(first, "inc")); got != "1" {
		t.Fatalf("first inc #1 = %s, want 1", got)
	}
	if got := string(call(first, "inc")); got != "2" {
		t.Fatalf("first inc #2 = %s, want 2", got)
	}
	if got := string(call(first, "get")); got != "2" {
		t.Fatalf("first get = %s, want 2", got)
	}
	if got := string(call(second, "get")); got != "0" {
		t.Fatalf("second get = %s, want 0", got)
	}

	_, err = first.Call(ctx, "missing", json.RawMessage("null"), 5*time.Second)
	if err == nil {
		t.Fatal("missing export unexpectedly succeeded")
	}
	var typed *Error
	if !errors.As(err, &typed) {
		t.Fatalf("missing export error is not *Error: %v", err)
	}
	if typed.Status == StatusOK || typed.Status == StatusPending {
		t.Fatalf("missing export status = %s", typed.Status)
	}

	if err := first.Cancel(); err != nil {
		t.Fatalf("Cancel: %v", err)
	}
	_, err = first.Call(ctx, "get", json.RawMessage("null"), 5*time.Second)
	if err == nil {
		t.Fatal("call after cancel unexpectedly succeeded")
	}
	if !errors.As(err, &typed) ||
		(typed.Status != StatusClosed && typed.Status != StatusCancelled) {
		t.Fatalf("call after cancel status = %v", err)
	}

	secondReport, err := second.Close()
	if err != nil {
		t.Fatalf("second Close: %v", err)
	}
	if !secondReport.ClosedCleanly {
		t.Logf("second close was not clean: %+v", secondReport)
	}
	firstReport, err := first.Close()
	if err != nil {
		t.Fatalf("first Close: %v", err)
	}
	t.Logf("first close report: %+v", firstReport)

	shutdown, err := engine.Close()
	if err != nil {
		t.Fatalf("engine Close: %v", err)
	}
	t.Logf("shutdown report: %+v", shutdown)
}

func TestContextCancellationAndConcurrentClose(t *testing.T) {
	engine, err := New()
	if err != nil {
		t.Fatal(err)
	}
	sandbox, err := engine.Create(`export function spin() { while(true){} }`)
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	result := make(chan error, 1)
	go func() { _, err := sandbox.Call(ctx, "spin", json.RawMessage("null"), time.Second); result <- err }()
	time.Sleep(20 * time.Millisecond)
	cancel()
	select {
	case err := <-result:
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("expected context.Canceled, got %v", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("cancelled call did not return")
	}
	closed := make(chan error, 2)
	for i := 0; i < 2; i++ {
		go func() { _, err := sandbox.Close(); closed <- err }()
	}
	for i := 0; i < 2; i++ {
		if err := <-closed; err != nil {
			t.Fatal(err)
		}
	}
	for i := 0; i < 2; i++ {
		go func() { _, err := engine.Close(); closed <- err }()
	}
	for i := 0; i < 2; i++ {
		if err := <-closed; err != nil {
			t.Fatal(err)
		}
	}
}

func TestContextDeadlinePreservesGoErrorIdentity(t *testing.T) {
	engine, err := New()
	if err != nil {
		t.Fatal(err)
	}
	defer engine.Close()
	sandbox, err := engine.Create(`export function spin() { while(true){} }`)
	if err != nil {
		t.Fatal(err)
	}
	defer sandbox.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Millisecond)
	defer cancel()
	_, err = sandbox.Call(ctx, "spin", json.RawMessage("null"), time.Second)
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("expected context.DeadlineExceeded, got %v", err)
	}
}
