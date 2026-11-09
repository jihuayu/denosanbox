//! Experimental C ABI for the native `denosanbox` core.
//!
//! This layer owns integer-token handles, caller-owned buffers, a small async
//! job bookkeeping layer and scoped foreign host callbacks (`host.rs`). It
//! deliberately does not implement JavaScript, a policy engine, a second
//! runtime or subprocesses. Every actual sandbox operation is delegated to the
//! `denosanbox` crate; callbacks run on the core's managed host threads.

use std::collections::HashMap;
use std::ffi::c_char;
use std::mem;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use denosanbox::{
    Engine, EngineConfig, Error, InvocationContext, InvocationOptions, PluginSource, Policy,
    SandboxHandle, SandboxOptions,
};
use futures_util::FutureExt;

mod host;
use host::CEngine;
use serde_json::Value;
use tokio::runtime::Runtime;

pub(crate) const STATUS_OK: i32 = 0;
const STATUS_PENDING: i32 = 1;
pub(crate) const STATUS_INVALID_ARGUMENT: i32 = 2;
pub(crate) const STATUS_INVALID_HANDLE: i32 = 3;
const STATUS_CLOSED: i32 = 4;
const STATUS_CANCELLED: i32 = 5;
const STATUS_DEADLINE: i32 = 6;
pub(crate) const STATUS_LIMIT: i32 = 7;
const STATUS_EXECUTION: i32 = 8;
const STATUS_UNSUPPORTED: i32 = 9;
pub(crate) const STATUS_INTERNAL: i32 = 10;

pub(crate) const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const MAX_EXPORT_BYTES: usize = 128;
const MAX_PENDING_JOBS: usize = 132;
static PENDING_JOBS: AtomicUsize = AtomicUsize::new(0);

struct SubmissionPermit;
impl SubmissionPermit {
    fn acquire() -> Result<Self, CError> {
        let mut used = PENDING_JOBS.load(Ordering::Acquire);
        loop {
            if used >= MAX_PENDING_JOBS {
                return Err(CError::new(STATUS_LIMIT, "native submission queue is full"));
            }
            match PENDING_JOBS.compare_exchange_weak(
                used,
                used + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Self),
                Err(actual) => used = actual,
            }
        }
    }
}
impl Drop for SubmissionPermit {
    fn drop(&mut self) {
        PENDING_JOBS.fetch_sub(1, Ordering::AcqRel);
    }
}

type DenoStatus = i32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DenoBuffer {
    data: *const u8,
    len: usize,
    token: u64,
}

#[repr(C)]
pub struct DenoError {
    code: DenoStatus,
    message: DenoBuffer,
}

#[repr(C)]
pub struct DenoSandboxCloseReport {
    invocations_completed: u64,
    invocations_cancelled: u64,
    forced_termination: i32,
    closed_cleanly: i32,
}

#[repr(C)]
pub struct DenoEngineShutdownReport {
    sandboxes_closed: u64,
    sandboxes_forced: u64,
    clean: i32,
}

impl DenoBuffer {
    fn zero() -> Self {
        Self {
            data: ptr::null(),
            len: 0,
            token: 0,
        }
    }
}

pub(crate) struct CError {
    pub(crate) status: DenoStatus,
    pub(crate) message: String,
}

impl CError {
    pub(crate) fn new(status: DenoStatus, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub(crate) fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(STATUS_INVALID_ARGUMENT, message)
    }

    pub(crate) fn invalid_handle(message: impl Into<String>) -> Self {
        Self::new(STATUS_INVALID_HANDLE, message)
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(STATUS_INTERNAL, message)
    }

    pub(crate) fn from_core(error: Error) -> Self {
        Self::new(status_for_kind(error.kind()), error.to_string())
    }
}

fn status_for_kind(kind: &str) -> DenoStatus {
    match kind {
        "closed" => STATUS_CLOSED,
        "cancelled" => STATUS_CANCELLED,
        "deadline_exceeded" => STATUS_DEADLINE,
        "limit_exceeded" | "queue_full" => STATUS_LIMIT,
        "execution" => STATUS_EXECUTION,
        "unsupported_feature" | "unsupported_language" | "runtime_unavailable" => {
            STATUS_UNSUPPORTED
        }
        "backend" | "internal" => STATUS_INTERNAL,
        "invalid_plugin"
        | "module_not_found"
        | "module_denied"
        | "invalid_configuration"
        | "policy" => STATUS_INVALID_ARGUMENT,
        _ => STATUS_INTERNAL,
    }
}

static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();

pub(crate) fn runtime() -> Result<&'static Runtime, CError> {
    match RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("denosanbox-c")
            .build()
            .map_err(|error| error.to_string())
    }) {
        Ok(runtime) => Ok(runtime),
        Err(message) => Err(CError::internal(format!(
            "failed to initialize the native Tokio runtime: {message}"
        ))),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Engine,
    Sandbox,
    Job,
    Buffer,
    /// A context borrowed by exactly one in-flight foreign callback.
    Context,
}

enum Entry {
    /// Engines are wrapped so the C layer can own callback registrations next to
    /// the real core engine. `Deref` keeps every existing engine call site.
    Engine(Arc<CEngine>),
    Sandbox(SandboxHandle),
    Job(Arc<JobState>),
    Buffer(Vec<u8>),
    Context(InvocationContext),
}

impl Entry {
    fn kind(&self) -> EntryKind {
        match self {
            Self::Engine(_) => EntryKind::Engine,
            Self::Sandbox(_) => EntryKind::Sandbox,
            Self::Job(_) => EntryKind::Job,
            Self::Buffer(_) => EntryKind::Buffer,
            Self::Context(_) => EntryKind::Context,
        }
    }
}

struct Registry {
    next: u64,
    entries: HashMap<u64, Entry>,
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| {
        Mutex::new(Registry {
            next: 1,
            entries: HashMap::new(),
        })
    })
}

fn with_registry<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let mut guard = registry().lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut guard)
}

fn insert_entry(entry: Entry) -> u64 {
    with_registry(|registry| {
        let token = registry.next;
        // Token 0 is reserved for "empty". Exhausting a u64 token space is not
        // a practical runtime condition; fail rather than reuse an old token.
        registry.next = token
            .checked_add(1)
            .expect("denosanbox C ABI token space exhausted");
        registry.entries.insert(token, entry);
        token
    })
}

fn get_engine(token: u64) -> Option<Arc<CEngine>> {
    if token == 0 {
        return None;
    }
    with_registry(|registry| match registry.entries.get(&token) {
        Some(Entry::Engine(engine)) => Some(engine.clone()),
        _ => None,
    })
}

/// Removes the C lookup metadata for one callback name. The core registry owns
/// the handler until engine shutdown; an in-flight dispatch keeps its handler
/// until the callback returns.
#[no_mangle]
/// # Safety
/// All non-null pointers follow the header's byte/output contracts.
pub unsafe extern "C" fn deno_engine_forget_host(
    engine_token: u64,
    name: *const u8,
    name_len: usize,
    error_out: *mut DenoError,
) -> DenoStatus {
    ffi(error_out, || {
        let name_bytes = copy_input(name, name_len, 64, "callback name")?;
        let name = std::str::from_utf8(&name_bytes)
            .map_err(|_| CError::invalid_argument("callback name is not UTF-8"))?;
        let engine = get_engine(engine_token)
            .ok_or_else(|| CError::invalid_handle("engine token is invalid or stale"))?;
        match engine.forget(name) {
            Some(registration) => {
                drop(registration);
                Ok(STATUS_OK)
            }
            None => Err(CError::invalid_argument("host callback is not registered")),
        }
    })
}

pub(crate) fn get_context(token: u64) -> Option<InvocationContext> {
    if token == 0 {
        return None;
    }
    with_registry(|registry| match registry.entries.get(&token) {
        Some(Entry::Context(context)) => Some(context.clone()),
        _ => None,
    })
}

pub(crate) fn insert_context(context: InvocationContext) -> u64 {
    insert_entry(Entry::Context(context))
}

fn get_sandbox(token: u64) -> Option<SandboxHandle> {
    if token == 0 {
        return None;
    }
    with_registry(|registry| match registry.entries.get(&token) {
        Some(Entry::Sandbox(sandbox)) => Some(sandbox.clone()),
        _ => None,
    })
}

fn get_job(token: u64) -> Option<Arc<JobState>> {
    if token == 0 {
        return None;
    }
    with_registry(|registry| match registry.entries.get(&token) {
        Some(Entry::Job(job)) => Some(job.clone()),
        _ => None,
    })
}

pub(crate) fn take_entry(token: u64, kind: EntryKind) -> Option<Entry> {
    if token == 0 {
        return None;
    }
    with_registry(|registry| {
        let matches_kind = matches!(
            registry.entries.get(&token),
            Some(entry) if entry.kind() == kind
        );
        if matches_kind {
            registry.entries.remove(&token)
        } else {
            None
        }
    })
}

pub(crate) fn insert_buffer(bytes: Vec<u8>) -> DenoBuffer {
    let len = bytes.len();
    // A Vec's heap allocation does not move when the Vec header is stored in
    // the registry, so this pointer remains valid until buffer_free removes it.
    let data = if len == 0 {
        ptr::null()
    } else {
        bytes.as_ptr()
    };
    let token = insert_entry(Entry::Buffer(bytes));
    DenoBuffer { data, len, token }
}

unsafe fn clear_error(error_out: *mut DenoError) {
    if !error_out.is_null() {
        ptr::write_bytes(error_out, 0, 1);
    }
}

pub(crate) unsafe fn set_error(error_out: *mut DenoError, error: CError) {
    if error_out.is_null() {
        return;
    }
    let message = insert_buffer(error.message.into_bytes());
    ptr::write(
        error_out,
        DenoError {
            code: error.status,
            message,
        },
    );
}

/// Runs an extern implementation with panic containment. A panic never unwinds
/// across the C ABI boundary; it becomes a typed internal error. OOM/abort and
/// native faults remain outside this guarantee.
pub(crate) fn ffi(
    err_out: *mut DenoError,
    f: impl FnOnce() -> Result<DenoStatus, CError>,
) -> DenoStatus {
    // Every fallible extern function requires a writable error output. Do not
    // perform side effects when the caller gave us nowhere to report failures.
    if err_out.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(status)) => {
            // Callers are documented to free an existing message before reuse.
            unsafe { clear_error(err_out) };
            status
        }
        Ok(Err(error)) => {
            let status = error.status;
            unsafe { set_error(err_out, error) };
            status
        }
        Err(_) => {
            let error = CError::internal("recoverable panic contained at the C ABI boundary");
            unsafe { set_error(err_out, error) };
            STATUS_INTERNAL
        }
    }
}

pub(crate) fn ffi_no_error(f: impl FnOnce() -> DenoStatus) -> DenoStatus {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(STATUS_INTERNAL)
}

pub(crate) unsafe fn copy_input(
    data: *const u8,
    len: usize,
    cap: usize,
    label: &str,
) -> Result<Vec<u8>, CError> {
    if len > cap {
        return Err(CError::new(
            STATUS_LIMIT,
            format!("{label} length {len} exceeds the {cap} byte limit"),
        ));
    }
    if len == 0 {
        // Never call from_raw_parts(NULL, 0): that is UB even though it looks
        // harmless. This ABI defines NULL + 0 as the empty slice.
        return Ok(Vec::new());
    }
    if data.is_null() {
        return Err(CError::invalid_argument(format!(
            "{label} pointer is NULL but len is {len}"
        )));
    }
    Ok(std::slice::from_raw_parts(data, len).to_vec())
}

struct JobError {
    status: DenoStatus,
    message: String,
}

impl JobError {
    fn from_core(error: Error) -> Self {
        Self {
            status: status_for_kind(error.kind()),
            message: error.to_string(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: STATUS_INTERNAL,
            message: message.into(),
        }
    }
}

enum JobOutcome {
    Pending,
    Ready(Vec<u8>),
    Failed(JobError),
}

struct JobInner {
    outcome: JobOutcome,
    consumed: bool,
}

struct JobState {
    sandbox: SandboxHandle,
    cancel_requested: AtomicBool,
    inner: Mutex<JobInner>,
    completed: Condvar,
}

enum WaitOutcome {
    Pending,
    Ready(Vec<u8>),
    Failed(JobError),
    Consumed,
}

impl JobState {
    fn new(sandbox: SandboxHandle) -> Self {
        Self {
            sandbox,
            cancel_requested: AtomicBool::new(false),
            inner: Mutex::new(JobInner {
                outcome: JobOutcome::Pending,
                consumed: false,
            }),
            completed: Condvar::new(),
        }
    }

    fn complete(&self, result: Result<Vec<u8>, JobError>) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.consumed {
            return;
        }
        if self.cancel_requested.load(Ordering::Acquire) {
            inner.outcome = JobOutcome::Failed(JobError {
                status: STATUS_CANCELLED,
                message: "job cancelled; cancellation is terminal for the sandbox".to_string(),
            });
        } else {
            inner.outcome = match result {
                Ok(bytes) => JobOutcome::Ready(bytes),
                Err(error) => JobOutcome::Failed(error),
            };
        }
        self.completed.notify_all();
    }

    fn request_cancel(&self) {
        self.cancel_requested.store(true, Ordering::Release);
        self.sandbox.cancel();
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(inner.outcome, JobOutcome::Pending) {
            inner.outcome = JobOutcome::Failed(JobError {
                status: STATUS_CANCELLED,
                message: "job cancelled; cancellation is terminal for the sandbox".to_string(),
            });
        }
        self.completed.notify_all();
    }

    fn wait(&self, wait_ms: u64) -> WaitOutcome {
        let deadline = Instant::now().checked_add(Duration::from_millis(wait_ms));
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);

        loop {
            if inner.consumed {
                return WaitOutcome::Consumed;
            }
            match &inner.outcome {
                JobOutcome::Pending => {}
                JobOutcome::Ready(_) | JobOutcome::Failed(_) => {
                    let outcome = mem::replace(&mut inner.outcome, JobOutcome::Pending);
                    inner.consumed = true;
                    return match outcome {
                        JobOutcome::Ready(bytes) => WaitOutcome::Ready(bytes),
                        JobOutcome::Failed(error) => WaitOutcome::Failed(error),
                        JobOutcome::Pending => WaitOutcome::Pending,
                    };
                }
            }

            if wait_ms == 0 {
                return WaitOutcome::Pending;
            }
            let Some(deadline) = deadline else {
                return WaitOutcome::Pending;
            };
            let now = Instant::now();
            if now >= deadline {
                return WaitOutcome::Pending;
            }
            let remaining = deadline.saturating_duration_since(now);
            let (guard, _) = self
                .completed
                .wait_timeout(inner, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            inner = guard;
        }
    }
}

fn submit_job(
    sandbox: SandboxHandle,
    export: String,
    input: Value,
    timeout: Option<Duration>,
) -> Result<u64, CError> {
    let permit = SubmissionPermit::acquire()?;
    let runtime = runtime()?;
    // Pin the deadline before handing the job to Tokio's scheduler so queueing
    // or host executor delay cannot extend the caller's submitted timeout.
    let deadline = timeout.and_then(|duration| SystemTime::now().checked_add(duration));
    let job = Arc::new(JobState::new(sandbox));
    let token = insert_entry(Entry::Job(job.clone()));

    let task_job = job.clone();
    let call_job = job.clone();
    let future = async move {
        // Cancellation/free may release the public token first. Keep this
        // admission reservation until native execution and accounting finish.
        let _permit = permit;
        let caught = AssertUnwindSafe(async move {
            let result = call_job
                .sandbox
                .call(&export, input, InvocationOptions { timeout, deadline })
                .await;
            match result {
                Ok(value) => serde_json::to_vec(&value).map_err(|error| {
                    JobError::internal(format!("failed to serialize guest result: {error}"))
                }),
                Err(error) => Err(JobError::from_core(error)),
            }
        })
        .catch_unwind()
        .await;

        let outcome = match caught {
            Ok(result) => result,
            Err(_) => Err(JobError::internal(
                "recoverable panic contained in the native job task",
            )),
        };
        task_job.complete(outcome);
    };

    let spawn_result = catch_unwind(AssertUnwindSafe(|| {
        runtime.handle().spawn(future);
    }));
    if spawn_result.is_err() {
        let _ = take_entry(token, EntryKind::Job);
        return Err(CError::internal(
            "failed to spawn the native invocation task",
        ));
    }
    Ok(token)
}

#[no_mangle]
pub extern "C" fn deno_abi_version() -> u32 {
    1
}

#[no_mangle]
pub extern "C" fn deno_package_version() -> *const c_char {
    c"0.1.0".as_ptr()
}

#[no_mangle]
pub extern "C" fn deno_status_name(status: i32) -> *const c_char {
    let name: &'static [u8] = match status {
        STATUS_OK => b"ok\0",
        STATUS_PENDING => b"pending\0",
        STATUS_INVALID_ARGUMENT => b"invalid_argument\0",
        STATUS_INVALID_HANDLE => b"invalid_handle\0",
        STATUS_CLOSED => b"closed\0",
        STATUS_CANCELLED => b"cancelled\0",
        STATUS_DEADLINE => b"deadline\0",
        STATUS_LIMIT => b"limit\0",
        STATUS_EXECUTION => b"execution\0",
        STATUS_UNSUPPORTED => b"unsupported\0",
        STATUS_INTERNAL => b"internal\0",
        _ => b"unknown\0",
    };
    name.as_ptr() as *const c_char
}

#[no_mangle]
/// # Safety
/// All non-null pointers must be valid for the documented byte length or
/// output type, and owned buffers must follow the versioned header's single
/// owner/free rules. Invalid native addresses are outside panic containment.
pub unsafe extern "C" fn deno_engine_new(
    out_engine: *mut u64,
    error_out: *mut DenoError,
) -> DenoStatus {
    ffi(error_out, || {
        if out_engine.is_null() {
            return Err(CError::invalid_argument("out_engine must not be NULL"));
        }
        ptr::write(out_engine, 0);
        // Wrapped with the same deny-all ceiling: registration is allowed but
        // grants fail the ceiling check until an engine with capabilities is used.
        let engine = Engine::with_policy(EngineConfig::default(), Policy::default())
            .map_err(CError::from_core)?;
        let token = insert_entry(Entry::Engine(CEngine::from_engine(
            engine,
            Policy::default(),
        )));
        ptr::write(out_engine, token);
        Ok(STATUS_OK)
    })
}

#[no_mangle]
/// # Safety
/// All non-null pointers must be valid for the documented byte length or
/// output type, and owned buffers must follow the versioned header's single
/// owner/free rules. Invalid native addresses are outside panic containment.
pub unsafe extern "C" fn deno_engine_create_sandbox(
    engine_token: u64,
    source: *const u8,
    source_len: usize,
    out_sandbox: *mut u64,
    error_out: *mut DenoError,
) -> DenoStatus {
    ffi(error_out, || {
        if out_sandbox.is_null() {
            return Err(CError::invalid_argument("out_sandbox must not be NULL"));
        }
        ptr::write(out_sandbox, 0);
        let source_bytes = copy_input(source, source_len, MAX_SOURCE_BYTES, "plugin source")?;
        let source_text = std::str::from_utf8(&source_bytes)
            .map_err(|error| {
                CError::invalid_argument(format!("plugin source is not UTF-8: {error}"))
            })?
            .to_string();
        let engine = get_engine(engine_token)
            .ok_or_else(|| CError::invalid_handle("engine token is invalid or stale"))?;
        let runtime = runtime()?;
        let prepared = engine
            .prepare(PluginSource::single("main.js", source_text))
            .map_err(CError::from_core)?;
        let sandbox = runtime
            .block_on(engine.create(prepared, SandboxOptions::default()))
            .map_err(CError::from_core)?;
        let token = insert_entry(Entry::Sandbox(sandbox));
        ptr::write(out_sandbox, token);
        Ok(STATUS_OK)
    })
}

#[no_mangle]
/// # Safety
/// All non-null pointers must be valid for the documented byte length or
/// output type, and owned buffers must follow the versioned header's single
/// owner/free rules. Invalid native addresses are outside panic containment.
pub unsafe extern "C" fn deno_sandbox_call_submit(
    sandbox_token: u64,
    export_name: *const u8,
    export_name_len: usize,
    input_json: *const u8,
    input_json_len: usize,
    timeout_ms: u64,
    out_job: *mut u64,
    error_out: *mut DenoError,
) -> DenoStatus {
    ffi(error_out, || {
        if out_job.is_null() {
            return Err(CError::invalid_argument("out_job must not be NULL"));
        }
        ptr::write(out_job, 0);
        let export_bytes = copy_input(
            export_name,
            export_name_len,
            MAX_EXPORT_BYTES,
            "export name",
        )?;
        let export = std::str::from_utf8(&export_bytes)
            .map_err(|error| {
                CError::invalid_argument(format!("export name is not UTF-8: {error}"))
            })?
            .to_string();
        if input_json_len == 0 {
            return Err(CError::invalid_argument(
                "input_json must contain a complete JSON value",
            ));
        }
        let input_bytes = copy_input(input_json, input_json_len, MAX_INPUT_BYTES, "input JSON")?;
        let input: Value = serde_json::from_slice(&input_bytes).map_err(|error| {
            CError::invalid_argument(format!("input_json is not valid JSON: {error}"))
        })?;
        let sandbox = get_sandbox(sandbox_token)
            .ok_or_else(|| CError::invalid_handle("sandbox token is invalid or stale"))?;
        let timeout = if timeout_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(timeout_ms))
        };
        let job_token = submit_job(sandbox, export, input, timeout)?;
        ptr::write(out_job, job_token);
        Ok(STATUS_OK)
    })
}

#[no_mangle]
/// # Safety
/// All non-null pointers must be valid for the documented byte length or
/// output type, and owned buffers must follow the versioned header's single
/// owner/free rules. Invalid native addresses are outside panic containment.
pub unsafe extern "C" fn deno_job_wait(
    job_token: u64,
    wait_ms: u64,
    out_result: *mut DenoBuffer,
    error_out: *mut DenoError,
) -> DenoStatus {
    ffi(error_out, || {
        if out_result.is_null() {
            return Err(CError::invalid_argument("out_result must not be NULL"));
        }
        ptr::write(out_result, DenoBuffer::zero());
        let job = get_job(job_token)
            .ok_or_else(|| CError::invalid_handle("job token is invalid or stale"))?;
        match job.wait(wait_ms) {
            WaitOutcome::Pending => Ok(STATUS_PENDING),
            WaitOutcome::Ready(bytes) => {
                ptr::write(out_result, insert_buffer(bytes));
                Ok(STATUS_OK)
            }
            WaitOutcome::Failed(error) => Err(CError::new(error.status, error.message)),
            WaitOutcome::Consumed => Err(CError::invalid_handle(
                "job result was already consumed; free the job token",
            )),
        }
    })
}

#[no_mangle]
pub extern "C" fn deno_job_cancel(job_token: u64) -> DenoStatus {
    ffi_no_error(|| {
        let job = match get_job(job_token) {
            Some(job) => job,
            None => return STATUS_INVALID_HANDLE,
        };
        job.request_cancel();
        STATUS_OK
    })
}

#[no_mangle]
pub extern "C" fn deno_job_free(job_token: u64) -> DenoStatus {
    ffi_no_error(|| match take_entry(job_token, EntryKind::Job) {
        Some(entry) => {
            drop(entry);
            STATUS_OK
        }
        None => STATUS_INVALID_HANDLE,
    })
}

#[no_mangle]
pub extern "C" fn deno_sandbox_cancel(sandbox_token: u64) -> DenoStatus {
    ffi_no_error(|| match get_sandbox(sandbox_token) {
        Some(sandbox) => {
            sandbox.cancel();
            STATUS_OK
        }
        None => STATUS_INVALID_HANDLE,
    })
}

#[no_mangle]
/// # Safety
/// All non-null pointers must be valid for the documented byte length or
/// output type, and owned buffers must follow the versioned header's single
/// owner/free rules. Invalid native addresses are outside panic containment.
pub unsafe extern "C" fn deno_sandbox_close(
    sandbox_token: u64,
    out_report: *mut DenoSandboxCloseReport,
    error_out: *mut DenoError,
) -> DenoStatus {
    ffi(error_out, || {
        if out_report.is_null() {
            return Err(CError::invalid_argument("out_report must not be NULL"));
        }
        ptr::write_bytes(out_report, 0, 1);
        let sandbox = get_sandbox(sandbox_token)
            .ok_or_else(|| CError::invalid_handle("sandbox token is invalid or stale"))?;
        let runtime = runtime()?;
        let report = runtime
            .block_on(sandbox.close())
            .map_err(CError::from_core)?;
        ptr::write(
            out_report,
            DenoSandboxCloseReport {
                invocations_completed: report.invocations_completed,
                invocations_cancelled: report.invocations_cancelled,
                forced_termination: i32::from(report.forced_termination),
                closed_cleanly: i32::from(report.closed_cleanly),
            },
        );
        Ok(STATUS_OK)
    })
}

#[no_mangle]
/// Host callback operations still holding native accounting after close. This is
/// the C layer's count of foreign callbacks that have not returned. No error_out
/// (status only); a NULL output returns DENO_STATUS_INVALID_ARGUMENT.
/// Call it after `deno_sandbox_close`: a nonzero value means a foreign callback
/// has not returned yet and user data will be released only after it does.
/// Returns DENO_STATUS_INVALID_HANDLE for a stale/wrong-kind token.
pub extern "C" fn deno_sandbox_pending_host_operations(
    sandbox_token: u64,
    out_pending: *mut u64,
) -> DenoStatus {
    ffi_no_error(|| {
        if out_pending.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        unsafe { ptr::write(out_pending, 0) };
        let _ = sandbox_token;
        unsafe { ptr::write(out_pending, host::pending_host_operations() as u64) };
        STATUS_OK
    })
}

#[no_mangle]
/// # Safety
/// All non-null pointers must be valid for the documented byte length or
/// output type, and owned buffers must follow the versioned header's single
/// owner/free rules. Invalid native addresses are outside panic containment.
pub unsafe extern "C" fn deno_sandbox_free(
    sandbox_token: u64,
    error_out: *mut DenoError,
) -> DenoStatus {
    ffi(error_out, || {
        match take_entry(sandbox_token, EntryKind::Sandbox) {
            Some(entry) => {
                drop(entry);
                Ok(STATUS_OK)
            }
            None => Err(CError::invalid_handle(
                "sandbox token is invalid, stale or of another kind",
            )),
        }
    })
}

#[no_mangle]
/// # Safety
/// All non-null pointers must be valid for the documented byte length or
/// output type, and owned buffers must follow the versioned header's single
/// owner/free rules. Invalid native addresses are outside panic containment.
pub unsafe extern "C" fn deno_engine_close(
    engine_token: u64,
    out_report: *mut DenoEngineShutdownReport,
    error_out: *mut DenoError,
) -> DenoStatus {
    ffi(error_out, || {
        if out_report.is_null() {
            return Err(CError::invalid_argument("out_report must not be NULL"));
        }
        ptr::write_bytes(out_report, 0, 1);
        let engine = get_engine(engine_token)
            .ok_or_else(|| CError::invalid_handle("engine token is invalid or stale"))?;
        let runtime = runtime()?;
        let report = runtime
            .block_on(engine.shutdown())
            .map_err(CError::from_core)?;
        ptr::write(
            out_report,
            DenoEngineShutdownReport {
                sandboxes_closed: report.sandboxes_closed as u64,
                sandboxes_forced: report.sandboxes_forced as u64,
                clean: i32::from(report.clean),
            },
        );
        Ok(STATUS_OK)
    })
}

#[no_mangle]
/// # Safety
/// All non-null pointers must be valid for the documented byte length or
/// output type, and owned buffers must follow the versioned header's single
/// owner/free rules. Invalid native addresses are outside panic containment.
pub unsafe extern "C" fn deno_engine_free(
    engine_token: u64,
    error_out: *mut DenoError,
) -> DenoStatus {
    ffi(error_out, || {
        match take_entry(engine_token, EntryKind::Engine) {
            Some(Entry::Engine(engine)) => {
                // Engine shutdown retires core registration handlers; any
                // cancelled-but-running dispatch keeps its capture until it
                // returns. This map contains lookup metadata only.
                engine.forget_all();
                drop(engine);
                Ok(STATUS_OK)
            }
            None | Some(_) => Err(CError::invalid_handle(
                "engine token is invalid, stale or of another kind",
            )),
        }
    })
}

#[no_mangle]
/// # Safety
/// All non-null pointers must be valid for the documented byte length or
/// output type, and owned buffers must follow the versioned header's single
/// owner/free rules. Invalid native addresses are outside panic containment.
pub unsafe extern "C" fn deno_buffer_free(buffer: *mut DenoBuffer) -> DenoStatus {
    ffi_no_error(|| {
        if buffer.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        let token = (*buffer).token;
        ptr::write_bytes(buffer, 0, 1);
        if token == 0 {
            return STATUS_OK;
        }
        match take_entry(token, EntryKind::Buffer) {
            Some(Entry::Buffer(bytes)) => {
                drop(bytes);
                STATUS_OK
            }
            _ => STATUS_INVALID_HANDLE,
        }
    })
}

#[no_mangle]
/// # Safety
/// All non-null pointers must be valid for the documented byte length or
/// output type, and owned buffers must follow the versioned header's single
/// owner/free rules. Invalid native addresses are outside panic containment.
pub unsafe extern "C" fn deno_error_free(error: *mut DenoError) -> DenoStatus {
    ffi_no_error(|| {
        if error.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        let token = (*error).message.token;
        ptr::write_bytes(error, 0, 1);
        if token == 0 {
            return STATUS_OK;
        }
        match take_entry(token, EntryKind::Buffer) {
            Some(Entry::Buffer(bytes)) => {
                drop(bytes);
                STATUS_OK
            }
            _ => STATUS_INVALID_HANDLE,
        }
    })
}
