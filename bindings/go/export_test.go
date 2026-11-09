package denosanbox

import (
	"reflect"
	"sync"
	"sync/atomic"
)

// Per-registration release observation for the test suite (test builds only).
// releasedCountFor associates a HostCallback identity with a counter that is
// incremented when that callback's user data is really released, so tests never
// depend on release timing relative to other tests.

var (
	recordedMu sync.Mutex
	recorded   = map[uintptr]*atomic.Int32{}
)

func releasedCountFor(fn HostCallback) *atomic.Int32 {
	key := reflect.ValueOf(fn).Pointer()
	counter := &atomic.Int32{}
	recordedMu.Lock()
	recorded[key] = counter
	recordedMu.Unlock()
	return counter
}

func init() {
	countRelease = func(rec *hostRecord) {
		key := reflect.ValueOf(rec.fn).Pointer()
		recordedMu.Lock()
		counter := recorded[key]
		recordedMu.Unlock()
		traceMu.Lock()
		traceLog = append(traceLog, key)
		traceMu.Unlock()
		if counter != nil {
			counter.Add(1)
		}
	}
}

var (
	traceMu  sync.Mutex
	traceLog []uintptr
)

func traceReleased() []uintptr {
	traceMu.Lock()
	defer traceMu.Unlock()
	return append([]uintptr(nil), traceLog...)
}
