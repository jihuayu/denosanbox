package denosanbox

import (
	"reflect"
	"sync"
	"sync/atomic"
)

// Per-registration release observation for the test suite (test builds only).
// watched[fn.SessionRegistration] holds the counters of that callback's next
// registrations; each counter is incremented by that registration's own release
// hook, which runs only after the native user-data release (cgo.Handle.Delete).
// Tests therefore observe real native teardown, not a timing assumption.

var (
	watchedMu sync.Mutex
	watched   = map[uintptr][]*atomic.Int32{}
)

func watchRelease(fn HostCallback) *atomic.Int32 {
	counter := &atomic.Int32{}
	key := reflect.ValueOf(fn).Pointer()
	watchedMu.Lock()
	watched[key] = append(watched[key], counter)
	watchedMu.Unlock()
	return counter
}

func takeWatch(fn HostCallback) *atomic.Int32 {
	if fn == nil {
		return nil
	}
	key := reflect.ValueOf(fn).Pointer()
	watchedMu.Lock()
	defer watchedMu.Unlock()
	queue := watched[key]
	if len(queue) == 0 {
		return nil
	}
	counter := queue[0]
	watched[key] = queue[1:]
	return counter
}

func init() {
	hookInsertRecord = func(rec *hostRecord) {
		if counter := takeWatch(rec.fn); counter != nil {
			rec.onRelease = func() { counter.Add(1) }
		}
	}
}
