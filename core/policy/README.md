# denosanbox-policy

Dependency-free security **primitives** for an in-process JavaScript sandbox.
They are building blocks for a runtime; they are **not** a containment boundary
on their own.

## Scope

| Module | Provides |
| --- | --- |
| `id` | Host-minted `SandboxId` (no guest construction/deserialization), `Generation`, owner-qualified `ResourceHandle`. |
| `path` | Canonical absolute virtual paths; traversal, empty/malformed segments rejected; segment-wise subtree checks (no string-prefix escape). |
| `policy` | Deny-default `Policy` over virtual read/write subtrees, host capabilities, network rules and limits. Intersection, subset, self-narrowing, and `Policy::child(host, parent, delegation, requested)`. |
| `resource` | Owner + generation registry; `close_all` bumps the generation so all older handles are stale. |
| `vfs` | `ResourceView`: private in-memory FS, logical cwd/env/tmp, bounded allocations. No host FS/env/process access. |
| `net` | Explicit-host/port grants, checked against every resolved IP; non-global addresses need an explicit private grant. No network I/O. |
| `limits`/`budget` | Default limits; hierarchical byte `Budget` with RAII `Lease`, checked overflow, `Deadline`, cooperative `CancellationToken`. |
| `registry` | Synchronous `HostRegistry` callbacks, per-sandbox grants with business scope, bounded input/output, stale result rejection. |
| `protocol` | Versioned bounded framing (`DSB1` v1) and sanitized typed error encoding. |

## Integration API (for the runtime agent)

```rust
use denosanbox_policy::*;
use std::net::IpAddr;
use std::time::Duration;

let id = SandboxId::mint();

// Policy is deny-default; grant only what is needed.
let host_policy = Policy::default()
    .allow_read("/app")?
    .allow_write("/tmp/work")?
    .allow_capability("clock")?
    .with_network(NetworkRules::deny_all().allow("api.example.com", 443)?);

// child = requested ∩ delegation ∩ parent ∩ host (narrowing only).
let child = Policy::child(&host_policy, &host_policy, &host_policy, &host_policy)?;

let mut view = ResourceView::new(id, child.clone(), ViewLimits::default());
view.write("/tmp/work/input.txt", b"data")?;
let bytes = view.read("/tmp/work/input.txt")?;

let mut registry = ResourceRegistry::default();
let handle = registry.insert(id, ResourceSpec::File {
    path: VirtualPath::parse("/tmp/work/input.txt")?,
    writable: true,
})?;
registry.get(handle)?;            // validates owner + generation + kind
registry.close_all(id);           // invalidates `handle`
assert!(registry.get(handle).is_err());

let budget = Budget::new(8 * 1024 * 1024);
let lease = budget.reserve(1024)?; // released on drop, charged to ancestors

let token = CancellationToken::new();
let ctx = InvocationContext::new(
    id,
    Deadline::after(Duration::from_secs(30)),
    token.clone(),
    "records.read",
);
let mut hosts = HostRegistry::default();
let cb = hosts.register("db.read", 64 * 1024, 256 * 1024, |_ctx, input| {
    Ok(input.to_vec()) // real host work belongs here, on a worker thread
})?;
hosts.grant(id, &cb, "records.read")?;
let out = hosts.invoke(&ctx, &cb, b"query".to_vec())?;

// Network check: pass every address you would actually dial.
let resolved: Vec<IpAddr> = vec!["93.184.216.34".parse().unwrap()];
host_policy.network().check("api.example.com", 443, &resolved)?;
host_policy.network().check_redirect("api.example.com", 443, &resolved)?;
```

`HostRegistry::begin` / `accept_result` implement the offloaded two-phase path:
the runtime moves `InvocationRequest` to a worker, then calls
`accept_result`; it returns `ErrorKind::LateResult` if the sandbox was closed or
the grant revoked meanwhile. Callbacks are **synchronous/blocking** — this crate
does not provide an async executor.

Wire protocol: `Frame::encode(max_payload)` / `Frame::decode(bytes, max_payload)`,
version `PROTOCOL_VERSION = 1`, magic `DSB1`, 20-byte header, payload capped at
`MAX_PAYLOAD` (1 MiB). Errors travel as `encode_error` / `decode_error` with a
stable numeric `ErrorKind` and sanitized UTF-8 text.

## Explicitly unsupported

* Host filesystem mounts (`Policy::mount_host_path`, `ResourceView::mount_host`).
* Symlinks (`Policy::create_symlink`, `ResourceView::symlink`) — both return
  `ErrorKind::Unsupported` until a race-safe implementation exists.
* Any process execution, host environment mutation, or real network I/O.
* Async callbacks, wildcard/DNS-suffix network rules, IP-literal hosts.

## Gaps and caveats

* `Limits::heap_target_bytes` is a JS-engine target, **not** a hard RSS bound.
  The host must enforce memory externally (cgroups/ulimit/job objects).
* Virtual paths are pure data; mapping them to host files is intentionally
  absent. Guest code cannot reach host paths through this crate.
* Timing side channels, CPU limits, and syscall filtering are out of scope.
* `SandboxId` is opaque and non-secret; authorization is policy + handle
  ownership, never the id alone.
* Callback implementations are trusted host code and must uphold the
  guest-safe error contract themselves.
* The protocol framing has no authentication or encryption; run it over a
  trusted channel only.

Do not claim security containment from these primitives alone. Containment
requires a hardened engine (V8 isolate with a minimal embedder), a supervisor
enforcing deadlines/memory, OS-level sandboxing, and a review of every host
callback for confused-deputy bugs.

## Tests

`cargo test --manifest-path core/policy/Cargo.toml` covers resource
owner/stale handles, policy child intersections, sibling VFS/env isolation,
traversal, network private-resolution, ancestor budget/drop release, overflow,
cancellation/deadline, callback bounds and late results. See the final task
report for the exact commands and results observed.
