//! Scoped C host callbacks.
//!
//! A registration creates one native owner that holds the foreign function
//! pointers plus the caller's `u64` user data. The core `HostRegistry` handler
//! owns the callback capture, and a dispatched `Dispatch` keeps its handler
//! alive while the callback runs. The C engine's callback map stores only
//! `RegisteredCallback` lookup metadata; it is not an owner of the foreign
//! user data. Engine shutdown retires registry handlers, and a running dispatch
//! keeps its owner until it returns. Release is never called under a registry
//! lock.

use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use denosanbox::policy;
use denosanbox::{
    Engine, EngineConfig, HostGrant, InvocationContext, Policy, RegisteredCallback, SandboxHandle,
    SandboxOptions,
};

use crate::{take_entry, with_registry, CError, DenoBuffer, DenoError, Entry, EntryKind};

pub(crate) const STATUS_OK: i32 = 0;
pub(crate) const STATUS_INVALID_ARGUMENT: i32 = 2;
pub(crate) const STATUS_CANCELLED: i32 = 5;
pub(crate) const STATUS_DEADLINE: i32 = 6;
pub(crate) const STATUS_INTERNAL: i32 = 10;

pub(crate) const MAX_CAPABILITY_LIST_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_GRANT_LIST_BYTES: usize = 1024 * 1024;
const MAX_CALLBACK_EDGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CONTEXT_JSON_BYTES: usize = 64 * 1024;

/// Foreign callback. Returns a `deno_status_t`-shaped value; non-OK values are
/// reduced to one sanitized typed error and will never be forwarded verbatim.
pub(crate) type HostCallbackFn = unsafe extern "C" fn(
    context_token: u64,
    input: *const u8,
    input_len: usize,
    out_result: *mut DenoBuffer,
    user_data: u64,
) -> i32;

/// Optional release of the user data. Called exactly once by native code after
/// the last owner (registry entry, grant, in-flight invocation) ends.
pub(crate) type HostReleaseFn = unsafe extern "C" fn(user_data: u64);

/// One native callback owner. `armed` flips on only after the registration was
/// accepted, so a failed registration never releases caller-owned user data.
pub(crate) struct CHostCallback {
    callback: HostCallbackFn,
    release: Option<HostReleaseFn>,
    user_data: u64,
    /// Stays false until the registration is accepted and handed to the engine
    /// map, so a rejected registration never releases caller-owned user data.
    armed: AtomicBool,
}

/// Rust-side count of foreign callback invocations that have not returned. The
/// core also tracks its own outstanding count; this is the C layer's truthful
/// diagnostic that an owner (and user data) is still alive after close.
static ACTIVE_CALLBACKS: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn pending_host_operations() -> usize {
    ACTIVE_CALLBACKS.load(Ordering::Acquire)
}

struct ActiveCallback;

impl ActiveCallback {
    fn enter() -> Self {
        ACTIVE_CALLBACKS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for ActiveCallback {
    fn drop(&mut self) {
        ACTIVE_CALLBACKS.fetch_sub(1, Ordering::AcqRel);
    }
}

impl CHostCallback {
    /// Creates an unarmed owner; `Drop` will not call release until `arm`.
    fn new_unarmed(
        callback: HostCallbackFn,
        release: Option<HostReleaseFn>,
        user_data: u64,
    ) -> Self {
        Self {
            callback,
            release,
            user_data,
            armed: AtomicBool::new(false),
        }
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    fn invoke(&self, context: &InvocationContext, input: &[u8]) -> policy::Result<Vec<u8>> {
        let context_token = crate::insert_context(context.clone());
        let result = Self::invoke_token(self.callback, context_token, input, self.user_data);
        // The borrowed context token is only valid for the foreign call; release
        // it before returning, including every error path.
        let _ = take_entry(context_token, EntryKind::Context);
        result
    }

    pub(crate) fn invoke_token(
        callback: HostCallbackFn,
        context_token: u64,
        input: &[u8],
        user_data: u64,
    ) -> policy::Result<Vec<u8>> {
        // Held until the foreign function really returns, so accounting and the
        // callback owner (user data) survive a cancellation that arrives while
        // a non-cooperative callback is still running.
        let _active = ActiveCallback::enter();
        let mut out = DenoBuffer::zero();
        let out_ptr: *mut DenoBuffer = &mut out;
        let input_ptr = input.as_ptr();
        let input_len = input.len();
        let status = catch_unwind(AssertUnwindSafe(|| unsafe {
            callback(context_token, input_ptr, input_len, out_ptr, user_data)
        }));
        let status = match status {
            Ok(status) => status,
            Err(_) => {
                // The foreign function unwound (only possible for Rust callers
                // linked into the same image); still consume any buffer it wrote.
                unsafe {
                    let _ = DenoBuffer::consume_foreign(&mut out);
                }
                return Err(policy::Error::denied(
                    "host callback failed (panic contained)",
                ));
            }
        };
        // Always consume the returned buffer exactly once, on success and error.
        let bytes = unsafe { DenoBuffer::consume_foreign(&mut out) };
        if status != STATUS_OK {
            // Never forward the foreign message: it may contain secrets.
            return Err(policy::Error::denied(format!(
                "host callback failed (native status {status}); original error withheld"
            )));
        }
        match bytes {
            Ok(Some(bytes)) => Ok(bytes),
            Ok(None) => Err(policy::Error::denied(
                "host callback returned no owned result buffer",
            )),
            Err(()) => Err(policy::Error::denied(
                "host callback returned an invalid result buffer",
            )),
        }
    }

    /// Ends the native owner: releases the caller's user data (at most once).
    /// This may run on a native thread. A release implementation that may only
    /// run on specific threads is responsible for deferring it there; the C ABI
    /// only promises that this is called after the last owner and exactly once.
    pub(crate) fn finish(callback: HostCallbackFn, release: Option<HostReleaseFn>, user_data: u64) {
        let _ = callback;
        if let Some(release) = release {
            let _ = catch_unwind(AssertUnwindSafe(|| unsafe { release(user_data) }));
        }
    }
}

impl Drop for CHostCallback {
    fn drop(&mut self) {
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        Self::finish(self.callback, self.release, self.user_data);
    }
}

impl DenoBuffer {
    /// Consumes a buffer struct written by a foreign callback by deregistering
    /// its token. A zero struct is an empty result. A stale or foreign token is
    /// an error; it is never freed on the foreign caller's behalf.
    unsafe fn consume_foreign(slot: &mut DenoBuffer) -> Result<Option<Vec<u8>>, ()> {
        let token = slot.token;
        slot.data = std::ptr::null();
        slot.len = 0;
        slot.token = 0;
        if token == 0 {
            return Ok(None);
        }
        match take_entry(token, EntryKind::Buffer) {
            Some(Entry::Buffer(bytes)) => Ok(Some(bytes)),
            _ => Err(()),
        }
    }
}

/// Local newtype so the core's blanket `Fn` impl can hold the owner `Arc`.
pub(crate) struct CTrampoline(pub(crate) Arc<CHostCallback>);

impl CTrampoline {
    pub(crate) fn call(
        &self,
        context: &InvocationContext,
        input: &[u8],
    ) -> policy::Result<Vec<u8>> {
        self.0.invoke(context, input)
    }
}

/// The C engine wrapper: the real engine plus callback lookup metadata.
/// Actual callback captures are owned by the core registry and in-flight
/// dispatches.
pub(crate) struct CEngine {
    pub(crate) inner: Engine,
    pub(crate) ceiling: Policy,
    pub(crate) callbacks: Mutex<BTreeMap<String, RegisteredCallback>>,
}

impl CEngine {
    /// Registration needs a ceiling that is already fixed on the engine; a
    /// deny-all engine simply has no grantable names.
    pub(crate) fn from_engine(inner: Engine, ceiling: Policy) -> Arc<Self> {
        Self::build(inner, ceiling)
    }

    fn build(inner: Engine, ceiling: Policy) -> Arc<Self> {
        Arc::new(Self {
            inner,
            ceiling,
            callbacks: Mutex::new(BTreeMap::new()),
        })
    }

    /// Drops one C lookup-metadata entry. The core registry still owns the
    /// callback handler until engine shutdown; an in-flight dispatch retains
    /// its handler until the callback returns.
    pub(crate) fn forget(&self, name: &str) -> Option<RegisteredCallback> {
        self.callbacks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name)
    }

    pub(crate) fn forget_all(&self) {
        // This clears lookup metadata only. Core shutdown has already retired
        // registration handlers, except for captures retained by in-flight
        // dispatches.
        self.callbacks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    pub(crate) fn callback(&self, name: &str) -> Result<RegisteredCallback, CError> {
        self.lookup(name)
            .ok_or_else(|| CError::invalid_argument("host callback is not registered"))
    }

    fn lookup(&self, name: &str) -> Option<RegisteredCallback> {
        self.callbacks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    fn insert(&self, name: String, callback: RegisteredCallback) -> Result<(), CError> {
        let mut map = self
            .callbacks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if map.contains_key(&name) {
            return Err(CError::invalid_argument(
                "callback name already registered on this engine",
            ));
        }
        map.insert(name, callback);
        Ok(())
    }
}

impl std::ops::Deref for CEngine {
    type Target = Engine;

    fn deref(&self) -> &Engine {
        &self.inner
    }
}

/// Looks up an engine created by either constructor. Callback registration and
/// grants are only available on wrapped engines; a plain engine reports a typed
/// unsupported error instead of silently ignoring the request.
pub(crate) fn get_engine_any(token: u64) -> Result<Arc<CEngine>, CError> {
    if token == 0 {
        return Err(CError::invalid_handle("engine token is invalid or stale"));
    }
    match with_registry(|registry| registry.entries.get(&token).map(|entry| entry.kind())) {
        Some(EntryKind::Engine) => crate::get_engine(token)
            .ok_or_else(|| CError::invalid_handle("engine token is invalid or stale")),
        _ => Err(CError::invalid_handle("engine token is invalid or stale")),
    }
}

/// Validates and narrows UTF-8 capability strings (callback names).
pub(crate) fn parse_capabilities(bytes: &[u8]) -> Result<Vec<String>, CError> {
    let names: Vec<String> = serde_json::from_slice(bytes).map_err(|error| {
        CError::invalid_argument(format!(
            "capabilities are not a JSON array of strings: {error}"
        ))
    })?;
    let mut policy = Policy::default();
    for name in names {
        policy = policy.allow_capability(&name).map_err(|error| {
            CError::invalid_argument(format!("invalid capability name: {}", error.message()))
        })?;
    }
    Ok(policy.capabilities().map(str::to_string).collect())
}

fn parse_grants(bytes: &[u8]) -> Result<Vec<(String, String)>, CError> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
        CError::invalid_argument(format!("host grants are not valid JSON: {error}"))
    })?;
    let Some(object) = value.as_object() else {
        return Err(CError::invalid_argument(
            "host grants must be a JSON object mapping callback name to scope",
        ));
    };
    let mut grants = Vec::with_capacity(object.len());
    for (name, scope) in object {
        let Some(scope) = scope.as_str() else {
            return Err(CError::invalid_argument(
                "host grant scope must be a string",
            ));
        };
        grants.push((name.clone(), scope.to_string()));
    }
    Ok(grants)
}

#[no_mangle]
/// # Safety
/// All non-null pointers must be valid for the documented byte length or
/// output type, and owned buffers must follow the header's single owner/free
/// rules. Invalid native addresses are outside panic containment.
pub unsafe extern "C" fn deno_engine_new_with_capabilities(
    capabilities_json: *const u8,
    capabilities_len: usize,
    out_engine: *mut u64,
    error_out: *mut DenoError,
) -> i32 {
    crate::ffi(error_out, || {
        if out_engine.is_null() {
            return Err(CError::invalid_argument("out_engine must not be NULL"));
        }
        std::ptr::write(out_engine, 0);
        let bytes = crate::copy_input(
            capabilities_json,
            capabilities_len,
            MAX_CAPABILITY_LIST_BYTES,
            "capability list",
        )?;
        let names = parse_capabilities(&bytes)?;
        let mut ceiling = Policy::default();
        for name in &names {
            ceiling = ceiling
                .allow_capability(name)
                .map_err(|error| CError::invalid_argument(error.message().to_string()))?;
        }
        let engine = Engine::with_policy(EngineConfig::default(), ceiling.clone())
            .map_err(CError::from_core)?;
        let token = crate::insert_entry(Entry::Engine(CEngine::from_engine(engine, ceiling)));
        std::ptr::write(out_engine, token);
        Ok(STATUS_OK)
    })
}

#[no_mangle]
/// # Safety
/// `callback` must be a valid, thread-safe function pointer that does not
/// longjmp/throw across the boundary. Ownership of `user_data` transfers only on
/// DENO_STATUS_OK; on failure the caller still owns it.
pub unsafe extern "C" fn deno_engine_register_host(
    engine_token: u64,
    name: *const u8,
    name_len: usize,
    input_limit: usize,
    output_limit: usize,
    callback: Option<HostCallbackFn>,
    release: Option<HostReleaseFn>,
    user_data: u64,
    error_out: *mut DenoError,
) -> i32 {
    crate::ffi(error_out, || {
        let Some(callback) = callback else {
            return Err(CError::invalid_argument(
                "callback function must not be NULL",
            ));
        };
        if input_limit == 0 || input_limit > MAX_CALLBACK_EDGE_BYTES {
            return Err(CError::new(
                crate::STATUS_LIMIT,
                "callback input limit is zero or exceeds 64MiB",
            ));
        }
        if output_limit == 0 || output_limit > MAX_CALLBACK_EDGE_BYTES {
            return Err(CError::new(
                crate::STATUS_LIMIT,
                "callback output limit is zero or exceeds 64MiB",
            ));
        }
        let name_bytes = crate::copy_input(name, name_len, 64, "callback name")?;
        let name = std::str::from_utf8(&name_bytes)
            .map_err(|_| CError::invalid_argument("callback name is not UTF-8"))?
            .to_string();
        let engine = get_engine_any(engine_token)?;
        let owner = CHostCallback::new_unarmed(callback, release, user_data);
        // Validate the name against the C map before the core registration so a
        // rejected call never touches the caller's user data.
        if engine.lookup(&name).is_some() {
            return Err(CError::invalid_argument(
                "callback name already registered on this engine",
            ));
        }
        let owner = Arc::new(owner);
        let trampoline = CTrampoline(owner.clone());
        let registration = engine
            .inner
            .register_host(
                &name,
                input_limit,
                output_limit,
                move |context: &InvocationContext, input: &[u8]| trampoline.call(context, input),
            )
            .map_err(CError::from_core)?;
        // The core accepted the registration. If the C map insert fails (only
        // possible for a concurrent duplicate, which is pre-checked above) the
        // owner stays unarmed: the caller keeps its user data and the idempotent
        // Go release ignores the late release.
        if let Err(error) = engine.insert(name.clone(), registration) {
            return Err(error);
        }
        owner.arm();
        Ok(STATUS_OK)
    })
}

#[no_mangle]
/// # Safety
/// Pointers follow the header contracts; grants check the cap before parsing.
pub unsafe extern "C" fn deno_engine_create_sandbox_with_grants(
    engine_token: u64,
    source: *const u8,
    source_len: usize,
    grants_json: *const u8,
    grants_len: usize,
    out_sandbox: *mut u64,
    error_out: *mut DenoError,
) -> i32 {
    crate::ffi(error_out, || {
        if out_sandbox.is_null() {
            return Err(CError::invalid_argument("out_sandbox must not be NULL"));
        }
        std::ptr::write(out_sandbox, 0);
        let source_bytes =
            crate::copy_input(source, source_len, crate::MAX_SOURCE_BYTES, "plugin source")?;
        let source_text = std::str::from_utf8(&source_bytes)
            .map_err(|error| {
                CError::invalid_argument(format!("plugin source is not UTF-8: {error}"))
            })?
            .to_string();
        let grant_bytes = crate::copy_input(
            grants_json,
            grants_len,
            MAX_GRANT_LIST_BYTES,
            "host grant list",
        )?;
        let grants = parse_grants(&grant_bytes)?;
        let engine = get_engine_any(engine_token)?;
        // Resolve every requested callback in the owning engine. The sandbox
        // policy is the engine's fixed ceiling (core narrows the request);
        // callability then requires BOTH the ceiling capability and an explicit
        // grant here, so the guest source can never contribute a grant.
        let mut host_grants = Vec::with_capacity(grants.len());
        for (name, scope) in grants {
            let callback = engine.callback(&name)?;
            host_grants.push(HostGrant { callback, scope });
        }
        // The engine's fixed ceiling also rejects any name outside it.
        let policy = engine.ceiling.clone();
        let runtime = crate::runtime()?;
        let prepared = engine
            .prepare(denosanbox::PluginSource::single("main.js", source_text))
            .map_err(CError::from_core)?;
        let options = SandboxOptions {
            policy,
            host_grants: host_grants.clone(),
            ..SandboxOptions::default()
        };
        let sandbox: SandboxHandle = runtime
            .block_on(engine.create(prepared, options))
            .map_err(CError::from_core)?;
        // Engine::create narrows the effective sandbox policy to the requested
        // subset before the isolate exists, but the host-capability check reads
        // that effective policy. A sandbox created through this grant API must
        // retain the engine's fixed ceiling (never anything wider) before the
        // first call.
        sandbox
            .narrow_policy(&engine.ceiling)
            .map_err(CError::from_core)?;
        let token = crate::insert_entry(Entry::Sandbox(sandbox));
        std::ptr::write(out_sandbox, token);
        Ok(STATUS_OK)
    })
}

#[no_mangle]
/// # Safety
/// All pointers follow the header contracts; the cap is applied before slicing.
pub unsafe extern "C" fn deno_buffer_copy(
    data: *const u8,
    len: usize,
    out_buffer: *mut DenoBuffer,
    error_out: *mut DenoError,
) -> i32 {
    crate::ffi(error_out, || {
        if out_buffer.is_null() {
            return Err(CError::invalid_argument("out_buffer must not be NULL"));
        }
        std::ptr::write(out_buffer, DenoBuffer::zero());
        if len > MAX_CALLBACK_EDGE_BYTES {
            return Err(CError::new(
                crate::STATUS_LIMIT,
                "buffer copy length exceeds the 64MiB limit",
            ));
        }
        let bytes = crate::copy_input(data, len, MAX_CALLBACK_EDGE_BYTES, "buffer")?;
        std::ptr::write(out_buffer, crate::insert_buffer(bytes));
        Ok(STATUS_OK)
    })
}

fn context_metadata(context: &InvocationContext) -> Result<String, CError> {
    let live = context.check_live();
    let cancelled =
        matches!(&live, Err(error) if matches!(error.kind(), policy::ErrorKind::Cancelled));
    let deadline_exceeded =
        matches!(&live, Err(error) if matches!(error.kind(), policy::ErrorKind::DeadlineExceeded));
    let remaining_ms: Option<u64> = context.deadline().remaining().map(|remaining| {
        if deadline_exceeded {
            0
        } else {
            // Round up so a sub-millisecond budget is not reported as zero.
            let millis =
                remaining.as_millis() + u128::from(remaining.subsec_nanos() % 1_000_000 != 0);
            millis.min(u128::from(u64::MAX)) as u64
        }
    });
    let value = serde_json::json!({
        "sandbox_id": context.sandbox_id().to_string(),
        "scope": context.scope(),
        "remaining_ms": remaining_ms,
        "cancelled": cancelled,
    });
    serde_json::to_string(&value)
        .map_err(|error| CError::internal(format!("failed to encode context: {error}")))
}

#[no_mangle]
/// # Safety
/// Pointers follow the header contracts. The context token is borrowed for the
/// duration of the current foreign callback only.
pub unsafe extern "C" fn deno_context_json(
    context_token: u64,
    out_buffer: *mut DenoBuffer,
    error_out: *mut DenoError,
) -> i32 {
    crate::ffi(error_out, || {
        if out_buffer.is_null() {
            return Err(CError::invalid_argument("out_buffer must not be NULL"));
        }
        std::ptr::write(out_buffer, DenoBuffer::zero());
        let Some(context) = crate::get_context(context_token) else {
            return Err(CError::invalid_handle(
                "context token is invalid, stale or not borrowed by this call",
            ));
        };
        let json = context_metadata(&context)?;
        if json.len() > MAX_CONTEXT_JSON_BYTES {
            return Err(CError::internal("context metadata unexpectedly large"));
        }
        std::ptr::write(out_buffer, crate::insert_buffer(json.into_bytes()));
        Ok(STATUS_OK)
    })
}

#[no_mangle]
pub extern "C" fn deno_context_check(context_token: u64) -> i32 {
    crate::ffi_no_error(|| {
        let Some(context) = crate::get_context(context_token) else {
            return crate::STATUS_INVALID_HANDLE;
        };
        match context.check_live() {
            Ok(()) => STATUS_OK,
            Err(error) => match error.kind() {
                policy::ErrorKind::Cancelled => STATUS_CANCELLED,
                policy::ErrorKind::DeadlineExceeded => STATUS_DEADLINE,
                _ => STATUS_INTERNAL,
            },
        }
    })
}

#[no_mangle]
pub extern "C" fn deno_context_free(context_token: u64) -> i32 {
    // Borrowed context tokens are never caller-freeable; releasing one could
    // invalidate a pointer another thread still uses for this invocation.
    crate::ffi_no_error(|| match crate::get_context(context_token) {
        Some(_) => STATUS_INVALID_ARGUMENT,
        None => crate::STATUS_INVALID_HANDLE,
    })
}
