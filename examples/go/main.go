// Two sandboxes with independent JavaScript state, using the polling-only Go
// adapter. Both sandboxes execute in this host process; no guest PID is claimed.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"time"

	"denosanbox/bindings/go"
)

func call(ctx context.Context, sandbox *denosanbox.Sandbox, export string) (json.RawMessage, error) {
	return sandbox.Call(ctx, export, json.RawMessage("null"), 5*time.Second)
}

func run() error {
	fmt.Printf("Go adapter: C ABI %d, package %s\n",
		denosanbox.ABIVersion(), denosanbox.PackageVersion())

	engine, err := denosanbox.New()
	if err != nil {
		return fmt.Errorf("engine: %w", err)
	}
	defer func() { _, _ = engine.Close() }()

	source := `
let state = 0;
export function inc() { state += 1; return state; }
export function get() { return state; }
`
	first, err := engine.Create(source)
	if err != nil {
		return fmt.Errorf("first sandbox: %w", err)
	}
	defer func() { _, _ = first.Close() }()
	second, err := engine.Create(source)
	if err != nil {
		return fmt.Errorf("second sandbox: %w", err)
	}
	defer func() { _, _ = second.Close() }()

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	for i := 0; i < 2; i++ {
		result, err := call(ctx, first, "inc")
		if err != nil {
			return fmt.Errorf("first inc: %w", err)
		}
		fmt.Printf("first.inc() -> %s\n", result)
	}
	firstValue, err := call(ctx, first, "get")
	if err != nil {
		return fmt.Errorf("first get: %w", err)
	}
	secondValue, err := call(ctx, second, "get")
	if err != nil {
		return fmt.Errorf("second get: %w", err)
	}
	fmt.Printf("first.get() -> %s; second.get() -> %s\n", firstValue, secondValue)
	if string(firstValue) != "2" || string(secondValue) != "0" {
		return fmt.Errorf("sandbox state was not independent: first=%s second=%s",
			firstValue, secondValue)
	}

	if _, err := call(ctx, first, "missing"); err == nil {
		return fmt.Errorf("missing export unexpectedly succeeded")
	} else {
		var typed *denosanbox.Error
		if !errors.As(err, &typed) {
			return fmt.Errorf("missing export error was not typed: %w", err)
		}
		fmt.Printf("missing export typed error: %s\n", typed.Status)
	}

	if err := first.Cancel(); err != nil {
		return fmt.Errorf("cancel: %w", err)
	}
	fmt.Println("first cancel: terminal")
	if _, err := call(ctx, first, "get"); err == nil {
		return fmt.Errorf("call after cancel unexpectedly succeeded")
	} else {
		var typed *denosanbox.Error
		if !errors.As(err, &typed) ||
			(typed.Status != denosanbox.StatusClosed && typed.Status != denosanbox.StatusCancelled) {
			return fmt.Errorf("call after cancel returned the wrong typed error: %w", err)
		}
		fmt.Printf("first.get() after cancel: %s\n", typed.Status)
	}

	secondReport, err := second.Close()
	if err != nil {
		return fmt.Errorf("second close: %w", err)
	}
	fmt.Printf("second close: clean=%v forced=%v completed=%d cancelled=%d\n",
		secondReport.ClosedCleanly, secondReport.ForcedTermination,
		secondReport.InvocationsCompleted, secondReport.InvocationsCancelled)

	firstReport, err := first.Close()
	if err != nil {
		return fmt.Errorf("first close: %w", err)
	}
	fmt.Printf("first close: clean=%v forced=%v completed=%d cancelled=%d\n",
		firstReport.ClosedCleanly, firstReport.ForcedTermination,
		firstReport.InvocationsCompleted, firstReport.InvocationsCancelled)

	shutdown, err := engine.Close()
	if err != nil {
		return fmt.Errorf("engine close: %w", err)
	}
	fmt.Printf("engine close: clean=%v closed=%d forced=%d\n",
		shutdown.Clean, shutdown.SandboxesClosed, shutdown.SandboxesForced)
	return nil
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "go example:", err)
		os.Exit(1)
	}
	fmt.Println("Go example: OK")
}
