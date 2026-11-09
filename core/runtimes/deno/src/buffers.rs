//! Budget-accounted V8 `ArrayBuffer` backing-store allocator.
//!
//! Each successful native allocation reserves `len + OVERHEAD` budget bytes and
//! records its native `Layout` with the RAII `Lease`. `free` returns the block
//! using the recorded layout and then drops the lease. The allocator `State` is
//! owned via a raw `Arc` pointer and reclaimed exactly once by V8's drop hook.

use std::alloc::{alloc, alloc_zeroed, dealloc, Layout};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use deno_core::v8;
use denosanbox_core::policy::{Budget, Lease};

const OVERHEAD: usize = 64;
const ALIGN: usize = 8;

struct Allocation {
    layout: Layout,
    lease: Lease,
}

struct State {
    budget: Arc<Budget>,
    allocations: Mutex<HashMap<usize, Allocation>>,
}

/// Build a budget-accounted V8 allocator.
///
/// The single `Arc<State>` strong reference is transferred to V8 and reclaimed
/// exactly once by `drop_state`.
pub(crate) fn allocator(budget: Arc<Budget>) -> v8::UniqueRef<v8::Allocator> {
    let state = Arc::new(State {
        budget,
        allocations: Mutex::new(HashMap::new()),
    });
    // `into_raw` transfers this one strong reference to V8; `drop_state` is its
    // only matching `from_raw`.
    let handle = Arc::into_raw(state);
    // SAFETY: `handle` points to a live `State` kept alive until V8 calls
    // `drop_state`; `VTABLE` is `'static` and its callbacks have type `State`.
    unsafe { v8::new_rust_allocator(handle, &VTABLE) }
}

static VTABLE: v8::RustAllocatorVtable<State> = v8::RustAllocatorVtable {
    allocate,
    allocate_uninitialized,
    free,
    drop: drop_state,
};

/// Lock the map, recovering from poison.
///
/// Nothing that can panic (no user code or formatting) runs under this lock, so
/// poisoned map contents remain structurally valid and `into_inner` is safe.
fn lock_allocations(state: &State) -> std::sync::MutexGuard<'_, HashMap<usize, Allocation>> {
    match state.allocations.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn allocate_impl(state: &State, len: usize, zeroed: bool) -> *mut c_void {
    let charge = match len.checked_add(OVERHEAD) {
        Some(charge) => charge,
        None => return std::ptr::null_mut(),
    };
    // At least one byte keeps the native layout valid; overhead is still charged.
    let layout = match Layout::from_size_align(len.max(1), ALIGN) {
        Ok(layout) => layout,
        Err(_) => return std::ptr::null_mut(),
    };
    // `lease` is dropped, releasing the reservation, on every failure below.
    let lease = match state.budget.reserve(charge as u64) {
        Ok(lease) => lease,
        Err(_) => return std::ptr::null_mut(),
    };

    // SAFETY: `layout` has non-zero size and valid alignment, so both allocator
    // calls are legal; the result is null or a block owned according to `layout`.
    let ptr = unsafe {
        if zeroed {
            alloc_zeroed(layout)
        } else {
            alloc(layout)
        }
    };
    if ptr.is_null() {
        return std::ptr::null_mut(); // `lease` drops here.
    }

    // The mutex is acquired only after the native allocation and never held
    // across a native allocator call.
    let mut allocations = lock_allocations(state);
    allocations.insert(ptr as usize, Allocation { layout, lease });
    ptr.cast::<c_void>()
}

unsafe extern "C" fn allocate(state: &State, len: usize) -> *mut c_void {
    allocate_impl(state, len, true)
}

unsafe extern "C" fn allocate_uninitialized(state: &State, len: usize) -> *mut c_void {
    allocate_impl(state, len, false)
}

unsafe extern "C" fn free(state: &State, ptr: *mut c_void, _len: usize) {
    if ptr.is_null() {
        return;
    }
    // Do not hold the lock while returning native memory.
    let removed = {
        let mut allocations = lock_allocations(state);
        allocations.remove(&(ptr as usize))
    };
    if let Some(allocation) = removed {
        // SAFETY: this pointer/layout pair came from our successful
        // `alloc`/`alloc_zeroed`; removing it from the map makes this the unique
        // deallocation. V8's `len` is untrusted; the recorded layout is used.
        unsafe { dealloc(ptr.cast::<u8>(), allocation.layout) };
        // Release quota only after the native block has been returned.
        drop(allocation.lease);
    }
    // Unknown/already-freed pointers are ignored: no double free or crash.
}

unsafe extern "C" fn drop_state(ptr: *const State) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: `ptr` came from `Arc::into_raw` in `allocator`; this is the single
    // matching `from_raw`, and V8 invokes this callback exactly once.
    let state = unsafe { Arc::from_raw(ptr) };

    // Defensive cleanup: V8 should free all blocks first, but return any
    // remaining native memory before the leases are dropped with `State`.
    let remaining = {
        let mut allocations = lock_allocations(&state);
        std::mem::take(&mut *allocations)
    };
    for (ptr, allocation) in remaining {
        // SAFETY: each entry came from a successful native allocation in
        // `allocate_impl` and is now uniquely owned by this loop.
        unsafe { dealloc(ptr as *mut u8, allocation.layout) };
        drop(allocation.lease);
    }
}
