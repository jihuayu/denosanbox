//! Bounded host callbacks. Prepare dispatch while locked, then execute without
//! holding the registry lock. Call `accept_result` after completion.
use crate::{CancellationToken, Deadline, Error, Generation, Result, SandboxId};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub trait Callback: Send + Sync {
    fn call(&self, ctx: &InvocationContext, input: &[u8]) -> Result<Vec<u8>>;
}
impl<F> Callback for F
where
    F: Fn(&InvocationContext, &[u8]) -> Result<Vec<u8>> + Send + Sync,
{
    fn call(&self, ctx: &InvocationContext, input: &[u8]) -> Result<Vec<u8>> {
        self(ctx, input)
    }
}
pub type CallbackFuture = Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'static>>;
pub trait AsyncCallback: Send + Sync {
    fn call(&self, ctx: InvocationContext, input: Vec<u8>) -> CallbackFuture;
}
impl<F, Fut> AsyncCallback for F
where
    F: Fn(InvocationContext, Vec<u8>) -> Fut + Send + Sync,
    Fut: Future<Output = Result<Vec<u8>>> + Send + 'static,
{
    fn call(&self, ctx: InvocationContext, input: Vec<u8>) -> CallbackFuture {
        Box::pin(self(ctx, input))
    }
}
#[derive(Clone)]
enum Handler {
    Sync(Arc<dyn Callback>),
    Async(Arc<dyn AsyncCallback>),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallbackId(u64);
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredCallback {
    id: CallbackId,
    name: String,
    input_limit: usize,
    output_limit: usize,
}
impl RegisteredCallback {
    pub fn id(&self) -> CallbackId {
        self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn input_limit(&self) -> usize {
        self.input_limit
    }
    pub fn output_limit(&self) -> usize {
        self.output_limit
    }
}
#[derive(Debug, Clone)]
pub struct InvocationContext {
    sandbox_id: SandboxId,
    deadline: Deadline,
    cancellation: CancellationToken,
    scope: String,
}
impl InvocationContext {
    pub fn new(
        sandbox_id: SandboxId,
        deadline: Deadline,
        cancellation: CancellationToken,
        scope: impl Into<String>,
    ) -> Self {
        Self {
            sandbox_id,
            deadline,
            cancellation,
            scope: scope.into(),
        }
    }
    pub fn sandbox_id(&self) -> SandboxId {
        self.sandbox_id
    }
    pub fn deadline(&self) -> Deadline {
        self.deadline
    }
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
    pub fn scope(&self) -> &str {
        &self.scope
    }
    pub fn check_live(&self) -> Result<()> {
        self.cancellation.check()?;
        self.deadline.check()
    }
}
#[derive(Debug, Clone)]
pub struct InvocationRequest {
    sandbox_id: SandboxId,
    callback_id: CallbackId,
    generation: Generation,
    scope: String,
    input: Vec<u8>,
    context: InvocationContext,
}
impl InvocationRequest {
    pub fn sandbox_id(&self) -> SandboxId {
        self.sandbox_id
    }
    pub fn callback_id(&self) -> CallbackId {
        self.callback_id
    }
    pub fn generation(&self) -> Generation {
        self.generation
    }
    pub fn scope(&self) -> &str {
        &self.scope
    }
    pub fn input(&self) -> &[u8] {
        &self.input
    }
}
pub struct Dispatch {
    request: InvocationRequest,
    handler: Handler,
}
impl fmt::Debug for Dispatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dispatch")
            .field("request", &self.request)
            .finish()
    }
}
impl Dispatch {
    pub fn is_async(&self) -> bool {
        matches!(self.handler, Handler::Async(_))
    }
    /// Execute a synchronous callback on a host worker thread, never under a registry lock.
    pub fn run(self) -> Result<(InvocationRequest, Vec<u8>)> {
        self.request.context.check_live()?;
        let output = match self.handler {
            Handler::Sync(cb) => cb.call(&self.request.context, &self.request.input)?,
            Handler::Async(_) => {
                return Err(Error::unsupported("async callback requires run_async"))
            }
        };
        self.request.context.check_live()?;
        Ok((self.request, output))
    }
    /// Await an async callback outside the registry lock. Sync callbacks must be
    /// offloaded via `run`; they are deliberately not executed on an async executor.
    pub async fn run_async(self) -> Result<(InvocationRequest, Vec<u8>)> {
        self.request.context.check_live()?;
        let output = match self.handler {
            Handler::Async(cb) => {
                cb.call(self.request.context.clone(), self.request.input.clone())
                    .await?
            }
            Handler::Sync(_) => {
                return Err(Error::unsupported(
                    "blocking callback requires offloaded run",
                ))
            }
        };
        self.request.context.check_live()?;
        Ok((self.request, output))
    }
}
#[derive(Debug, Clone)]
struct Grant {
    generation: Generation,
    scope: String,
}
pub struct HostRegistry {
    callbacks: HashMap<CallbackId, (RegisteredCallback, Handler)>,
    grants: HashMap<(SandboxId, CallbackId), Grant>,
    closed: HashSet<SandboxId>,
    next_id: u64,
    next_grant: u64,
    capacity: usize,
}
impl fmt::Debug for HostRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostRegistry")
            .field("callbacks", &self.callbacks.len())
            .field("grants", &self.grants.len())
            .finish()
    }
}
impl Default for HostRegistry {
    fn default() -> Self {
        Self::new(256)
    }
}
fn valid_scope(scope: &str) -> bool {
    !scope.is_empty()
        && scope.len() <= 64
        && scope
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-/".contains(&b))
}
impl HostRegistry {
    pub fn new(capacity: usize) -> Self {
        Self {
            callbacks: HashMap::new(),
            grants: HashMap::new(),
            closed: HashSet::new(),
            next_id: 1,
            next_grant: 1,
            capacity,
        }
    }
    pub fn register(
        &mut self,
        name: &str,
        input_limit: usize,
        output_limit: usize,
        callback: impl Callback + 'static,
    ) -> Result<RegisteredCallback> {
        self.insert(
            name,
            input_limit,
            output_limit,
            Handler::Sync(Arc::new(callback)),
        )
    }
    pub fn register_async(
        &mut self,
        name: &str,
        input_limit: usize,
        output_limit: usize,
        callback: impl AsyncCallback + 'static,
    ) -> Result<RegisteredCallback> {
        self.insert(
            name,
            input_limit,
            output_limit,
            Handler::Async(Arc::new(callback)),
        )
    }
    fn insert(
        &mut self,
        name: &str,
        input_limit: usize,
        output_limit: usize,
        callback: Handler,
    ) -> Result<RegisteredCallback> {
        if self.callbacks.len() >= self.capacity {
            return Err(Error::limit_exceeded("callback registry full"));
        }
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.:-".contains(&b))
            || input_limit == 0
            || output_limit == 0
        {
            return Err(Error::invalid_argument("invalid callback registration"));
        }
        if self.callbacks.values().any(|(m, _)| m.name == name) {
            return Err(Error::invalid_argument("callback name already registered"));
        }
        let next = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| Error::overflow("callback id exhausted"))?;
        let meta = RegisteredCallback {
            id: CallbackId(self.next_id),
            name: name.into(),
            input_limit,
            output_limit,
        };
        self.next_id = next;
        self.callbacks.insert(meta.id, (meta.clone(), callback));
        Ok(meta)
    }
    pub fn grant(
        &mut self,
        sandbox: SandboxId,
        cb: &RegisteredCallback,
        scope: &str,
    ) -> Result<()> {
        if self.closed.contains(&sandbox) {
            return Err(Error::stale_handle("sandbox closed"));
        }
        if self.callbacks.get(&cb.id).map(|(m, _)| m) != Some(cb) {
            return Err(Error::unknown_callback("callback not registered"));
        }
        if !valid_scope(scope) {
            return Err(Error::invalid_argument("invalid business scope"));
        }
        let generation = Generation(self.next_grant);
        self.next_grant = self
            .next_grant
            .checked_add(1)
            .ok_or_else(|| Error::overflow("grant generation exhausted"))?;
        self.grants.insert(
            (sandbox, cb.id),
            Grant {
                generation,
                scope: scope.into(),
            },
        );
        Ok(())
    }
    pub fn revoke_grant(&mut self, sandbox: SandboxId, cb: &RegisteredCallback) -> bool {
        self.grants.remove(&(sandbox, cb.id)).is_some()
    }
    pub fn close_sandbox(&mut self, sandbox: SandboxId) -> usize {
        self.closed.insert(sandbox);
        let before = self.grants.len();
        self.grants.retain(|(owner, _), _| *owner != sandbox);
        before - self.grants.len()
    }
    pub fn begin(
        &self,
        ctx: &InvocationContext,
        cb: &RegisteredCallback,
        input: Vec<u8>,
    ) -> Result<InvocationRequest> {
        ctx.check_live()?;
        if self.closed.contains(&ctx.sandbox_id) {
            return Err(Error::stale_handle("sandbox closed"));
        }
        if self.callbacks.get(&cb.id).map(|(m, _)| m) != Some(cb) {
            return Err(Error::unknown_callback("callback not registered"));
        }
        let grant = self
            .grants
            .get(&(ctx.sandbox_id, cb.id))
            .ok_or_else(|| Error::denied("callback not granted"))?;
        if grant.scope != ctx.scope {
            return Err(Error::denied("callback scope mismatch"));
        }
        if input.len() > cb.input_limit {
            return Err(Error::limit_exceeded("callback input too large"));
        }
        Ok(InvocationRequest {
            sandbox_id: ctx.sandbox_id,
            callback_id: cb.id,
            generation: grant.generation,
            scope: grant.scope.clone(),
            input,
            context: ctx.clone(),
        })
    }
    pub fn prepare_dispatch(
        &self,
        ctx: &InvocationContext,
        cb: &RegisteredCallback,
        input: Vec<u8>,
    ) -> Result<Dispatch> {
        let request = self.begin(ctx, cb, input)?;
        let handler = self
            .callbacks
            .get(&cb.id)
            .ok_or_else(|| Error::unknown_callback("callback not registered"))?
            .1
            .clone();
        Ok(Dispatch { request, handler })
    }
    /// Convenience for an exclusively owned registry. For a shared registry,
    /// prepare dispatch under its lock, drop the guard, run, then accept result.
    pub fn invoke(
        &self,
        ctx: &InvocationContext,
        cb: &RegisteredCallback,
        input: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let (req, output) = self.prepare_dispatch(ctx, cb, input)?.run()?;
        self.accept_result(&req, output)
    }
    pub fn accept_result(&self, req: &InvocationRequest, output: Vec<u8>) -> Result<Vec<u8>> {
        req.context.check_live()?;
        let grant = self
            .grants
            .get(&(req.sandbox_id, req.callback_id))
            .ok_or_else(|| Error::late_result("grant revoked before completion"))?;
        if self.closed.contains(&req.sandbox_id)
            || grant.generation != req.generation
            || grant.scope != req.scope
        {
            return Err(Error::late_result("result belongs to a stale grant"));
        }
        let (meta, _) = self
            .callbacks
            .get(&req.callback_id)
            .ok_or_else(|| Error::unknown_callback("callback not registered"))?;
        if output.len() > meta.output_limit {
            return Err(Error::limit_exceeded("callback output too large"));
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use std::time::Duration;

    fn setup() -> (
        HostRegistry,
        RegisteredCallback,
        InvocationContext,
        SandboxId,
    ) {
        let mut reg = HostRegistry::default();
        let cb = reg
            .register("echo", 8, 8, |ctx: &InvocationContext, input: &[u8]| {
                let _ = ctx.scope();
                Ok(input.to_vec())
            })
            .unwrap();
        let id = SandboxId::from_host_parts(7, 1);
        reg.grant(id, &cb, "records.read").unwrap();
        let ctx = InvocationContext::new(
            id,
            Deadline::after(Duration::from_secs(5)),
            CancellationToken::new(),
            "records.read",
        );
        (reg, cb, ctx, id)
    }

    #[test]
    fn requires_grant_and_matching_scope() {
        let (reg, cb, ctx, id) = setup();
        assert!(reg.invoke(&ctx, &cb, b"a".to_vec()).is_ok());
        let other = InvocationContext::new(
            SandboxId::from_host_parts(7, 2),
            ctx.deadline(),
            CancellationToken::new(),
            "records.read",
        );
        assert_eq!(
            reg.invoke(&other, &cb, vec![]).unwrap_err().kind(),
            ErrorKind::Denied
        );
        let wrong_scope = InvocationContext::new(
            id,
            ctx.deadline(),
            CancellationToken::new(),
            "records.write",
        );
        assert!(reg.invoke(&wrong_scope, &cb, vec![]).is_err());
    }

    #[test]
    fn bounds_input_and_output() {
        let (reg, cb, ctx, _) = setup();
        assert_eq!(
            reg.invoke(&ctx, &cb, vec![0; 9]).unwrap_err().kind(),
            ErrorKind::LimitExceeded
        );
        let req = reg.begin(&ctx, &cb, vec![0; 8]).unwrap();
        assert_eq!(
            reg.accept_result(&req, vec![0; 9]).unwrap_err().kind(),
            ErrorKind::LimitExceeded
        );
        assert!(reg.accept_result(&req, vec![0; 8]).is_ok());
    }

    #[test]
    fn late_result_after_close_rejected() {
        let (mut reg, cb, ctx, id) = setup();
        let req = reg.begin(&ctx, &cb, b"x".to_vec()).unwrap();
        assert_eq!(reg.close_sandbox(id), 1);
        assert_eq!(
            reg.accept_result(&req, b"done".to_vec())
                .unwrap_err()
                .kind(),
            ErrorKind::LateResult
        );
    }

    #[test]
    fn cancellation_and_deadline_stop_invocation() {
        let (reg, cb, ctx, _) = setup();
        ctx.cancellation().cancel();
        assert_eq!(
            reg.invoke(&ctx, &cb, vec![]).unwrap_err().kind(),
            ErrorKind::Cancelled
        );
        let expired = InvocationContext::new(
            ctx.sandbox_id(),
            Deadline::after(Duration::ZERO),
            CancellationToken::new(),
            ctx.scope(),
        );
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(
            reg.invoke(&expired, &cb, vec![]).unwrap_err().kind(),
            ErrorKind::DeadlineExceeded
        );
    }
}
