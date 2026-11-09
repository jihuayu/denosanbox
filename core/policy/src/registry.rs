//! Host callback registration, per-sandbox grants and invocation context.
//!
//! Callbacks are **synchronous** and blocking; the runtime is responsible for
//! offloading them to a worker thread. A result produced after the sandbox's
//! grant generation was closed is rejected by [`HostRegistry::accept_result`].

use std::collections::HashMap;
use std::fmt;

use crate::budget::{CancellationToken, Deadline};
use crate::error::{Error, Result};
use crate::id::{Generation, SandboxId};

/// Blocking host callback.
pub trait Callback: Send + Sync {
    /// Handle bounded `input`; return bounded output or a guest-safe error.
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

/// Host-minted callback id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallbackId(u64);

/// Public metadata for a registered callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredCallback {
    id: CallbackId,
    name: String,
    input_limit: usize,
    output_limit: usize,
}

impl RegisteredCallback {
    /// Callback id.
    #[must_use]
    pub fn id(&self) -> CallbackId {
        self.id
    }
    /// Display name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Maximum input bytes.
    #[must_use]
    pub fn input_limit(&self) -> usize {
        self.input_limit
    }
    /// Maximum output bytes.
    #[must_use]
    pub fn output_limit(&self) -> usize {
        self.output_limit
    }
}

/// Trusted per-invocation context.
#[derive(Debug, Clone)]
pub struct InvocationContext {
    sandbox_id: SandboxId,
    deadline: Deadline,
    cancellation: CancellationToken,
    scope: String,
}

impl InvocationContext {
    /// Host-only constructor.
    #[must_use]
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
    /// Trusted sandbox identity.
    #[must_use]
    pub fn sandbox_id(&self) -> SandboxId {
        self.sandbox_id
    }
    /// Deadline.
    #[must_use]
    pub fn deadline(&self) -> Deadline {
        self.deadline
    }
    /// Cancellation token.
    #[must_use]
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
    /// Explicit business scope granted for this call.
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }
    fn check_live(&self) -> Result<()> {
        self.cancellation.check()?;
        self.deadline.check()
    }
}

/// A validated request; safe to hand to a worker thread.
#[derive(Debug, Clone)]
pub struct InvocationRequest {
    sandbox_id: SandboxId,
    callback_id: CallbackId,
    generation: Generation,
    scope: String,
    input: Vec<u8>,
}

impl InvocationRequest {
    /// Sandbox the request belongs to.
    #[must_use]
    pub fn sandbox_id(&self) -> SandboxId {
        self.sandbox_id
    }
    /// Callback id.
    #[must_use]
    pub fn callback_id(&self) -> CallbackId {
        self.callback_id
    }
    /// Grant generation captured at begin.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generation
    }
    /// Business scope.
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }
    /// Bounded input.
    #[must_use]
    pub fn input(&self) -> &[u8] {
        &self.input
    }
}

#[derive(Debug, Clone)]
struct Grant {
    generation: Generation,
    scope: String,
}

/// Callback table + grants. Not cloneable; one per host runtime.
pub struct HostRegistry {
    callbacks: HashMap<CallbackId, (RegisteredCallback, Box<dyn Callback>)>,
    grants: HashMap<(SandboxId, CallbackId), Grant>,
    generations: HashMap<SandboxId, Generation>,
    next_id: u64,
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
    /// Registry with a callback cap.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            callbacks: HashMap::new(),
            grants: HashMap::new(),
            generations: HashMap::new(),
            next_id: 1,
            capacity,
        }
    }

    /// Register a host callback with hard input/output byte bounds.
    pub fn register(
        &mut self,
        name: &str,
        input_limit: usize,
        output_limit: usize,
        callback: impl Callback + 'static,
    ) -> Result<RegisteredCallback> {
        if self.callbacks.len() >= self.capacity {
            return Err(Error::limit_exceeded("callback registry full"));
        }
        if name.is_empty() || name.len() > 64 || input_limit == 0 || output_limit == 0 {
            return Err(Error::invalid_argument("invalid callback registration"));
        }
        let id = CallbackId(
            self.next_id
                .checked_add(1)
                .map(|_| self.next_id)
                .ok_or_else(|| Error::overflow("callback id exhausted"))?,
        );
        self.next_id = self.next_id.saturating_add(1);
        let meta = RegisteredCallback {
            id,
            name: name.to_string(),
            input_limit,
            output_limit,
        };
        self.callbacks
            .insert(id, (meta.clone(), Box::new(callback)));
        Ok(meta)
    }

    fn generation_of(&self, id: SandboxId) -> Generation {
        self.generations
            .get(&id)
            .copied()
            .unwrap_or(Generation::ZERO)
    }

    /// Grant `sandbox` the right to call `cb` within an explicit scope.
    pub fn grant(
        &mut self,
        sandbox: SandboxId,
        cb: &RegisteredCallback,
        scope: &str,
    ) -> Result<()> {
        if !self.callbacks.contains_key(&cb.id) {
            return Err(Error::unknown_callback("callback not registered"));
        }
        if !valid_scope(scope) {
            return Err(Error::invalid_argument("invalid business scope"));
        }
        let generation = self
            .generations
            .entry(sandbox)
            .or_insert_with(|| Generation(1))
            .to_owned();
        self.grants.insert(
            (sandbox, cb.id),
            Grant {
                generation,
                scope: scope.to_string(),
            },
        );
        Ok(())
    }

    /// Revoke one grant.
    pub fn revoke_grant(&mut self, sandbox: SandboxId, cb: &RegisteredCallback) -> bool {
        self.grants.remove(&(sandbox, cb.id)).is_some()
    }

    /// Close a sandbox: revoke all grants and bump its generation, so results
    /// still in flight can no longer be accepted.
    pub fn close_sandbox(&mut self, sandbox: SandboxId) -> usize {
        let before = self.grants.len();
        self.grants.retain(|(owner, _), _| *owner != sandbox);
        self.generations
            .entry(sandbox)
            .and_modify(|g| *g = g.next())
            .or_insert_with(|| Generation(1));
        before - self.grants.len()
    }

    /// Validate and prepare a request for off-thread execution.
    pub fn begin(
        &self,
        ctx: &InvocationContext,
        cb: &RegisteredCallback,
        input: Vec<u8>,
    ) -> Result<InvocationRequest> {
        ctx.check_live()?;
        let (meta, _) = self
            .callbacks
            .get(&cb.id)
            .ok_or_else(|| Error::unknown_callback("callback not registered"))?;
        if meta.input_limit != cb.input_limit || meta.output_limit != cb.output_limit {
            return Err(Error::unknown_callback("callback metadata mismatch"));
        }
        let grant = self
            .grants
            .get(&(ctx.sandbox_id(), cb.id))
            .ok_or_else(|| Error::denied("callback not granted"))?;
        if grant.generation != self.generation_of(ctx.sandbox_id()) {
            return Err(Error::stale_handle("callback grant is stale"));
        }
        if grant.scope != ctx.scope() {
            return Err(Error::denied("callback scope mismatch"));
        }
        if input.len() > cb.input_limit {
            return Err(Error::limit_exceeded("callback input too large"));
        }
        Ok(InvocationRequest {
            sandbox_id: ctx.sandbox_id(),
            callback_id: cb.id,
            generation: grant.generation,
            scope: grant.scope.clone(),
            input,
        })
    }

    /// Execute synchronously on the calling thread (runtime should offload).
    pub fn invoke(
        &self,
        ctx: &InvocationContext,
        cb: &RegisteredCallback,
        input: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let req = self.begin(ctx, cb, input)?;
        let (_, callback) = self
            .callbacks
            .get(&req.callback_id)
            .ok_or_else(|| Error::unknown_callback("callback not registered"))?;
        let output = callback.call(ctx, req.input())?;
        self.accept_result(&req, output)
    }

    /// Validate a worker-produced result; rejects late results after closure.
    pub fn accept_result(&self, req: &InvocationRequest, output: Vec<u8>) -> Result<Vec<u8>> {
        if self.generation_of(req.sandbox_id) != req.generation {
            return Err(Error::late_result("result belongs to a closed generation"));
        }
        let grant = self
            .grants
            .get(&(req.sandbox_id, req.callback_id))
            .ok_or_else(|| Error::late_result("grant revoked before completion"))?;
        if grant.generation != req.generation || grant.scope != req.scope {
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
