# API, ownership and lifecycle

The facade is `core/src/lib.rs`. Public values contain no Deno/V8 objects. The engine-independent model and raw contracts live in `core/core`; `core/runtimes/deno` owns V8 isolates and compiled state and is the production backend (embedded `deno_core`, no sidecar). Bindings call the facade directly. Optional observability belongs in `core/layers`, reusable behavior contracts in `core/testkit`, native binding packages in `bindings/<language>`, and host framework adapters in `integrations`.

## Prepare and create

`Engine::new(EngineConfig)` validates limits and selects one backend for the engine's lifetime (`BackendKind::Deno` by default; the explicit `fake` backend never executes JavaScript). Backend replacement requires building another Engine; no live sandbox migration occurs. `prepare(PluginSource)` parses/transpiles modules, bounds source/graph sizes and freezes a private immutable graph. Preparation performs no guest I/O. Reusing a prepared graph shares source text, never module namespaces or mutable JavaScript values.

`create(plugin, SandboxOptions)` validates the module graph, re-validates the prepared plugin against the creating engine's `EngineLimits` (also for a plugin prepared by another engine), narrows the requested policy against the engine ceiling and reserves admission before starting an execution thread. Each sandbox creates, runs and disposes its isolate on that thread; the driver is intentionally not `Send`. Factories and cancellation controls are thread-safe. Failed or cancelled startup releases its reservation. An engine cannot admit work after shutdown begins.

## Advanced Rust host configuration

`Engine::with_host_config(config, ceiling: Policy, HostConfig)` installs trusted host state before any sandbox is created. `HostConfig` holds per-capability JSON schemas and their limits, a shared `HostAudit`, a shared host-concurrency `Budget` (default 32 slots) and a shared managed-memory `Budget` (default 512 MiB); clone the config so sibling sandboxes share the same pools. Explicitly smaller pools are never expanded automatically. These advanced shared-pool, schema and audit controls are Rust-facade features: Python, Node, C and Go expose their own ceilings, requests, grants, callbacks and mounts but not this `HostConfig` surface.

## Invoke and cancel

`call(export, JSON value, InvocationOptions)` submits to a bounded serialized queue; policy `queue_depth`, message and deadline limits apply to both queued and direct calls. Different sandboxes run concurrently. Full admission returns a typed queue/limit error. JSON inputs/results have byte bounds; function names are validated. Wall-clock deadlines cover queued time and execution, and child contexts inherit the parent's absolute deadline. A watchdog interrupts JavaScript independently of the JavaScript event loop. Cancellation is terminal for the sandbox; create a fresh sandbox for the next task. Cancelling a root revokes its whole subtree.

A dropped handle requests cancellation without blocking an async host thread. Explicit `close().await` reports observed cleanup and joins managed execution. `Engine::shutdown().await` freezes admission, cancels live roots/descendants and gathers close reports. Close and shutdown are idempotent; a timed-out or non-cooperative native callback can yield an incomplete report while its ownership/accounting remains retained.

`SandboxHandle::narrow_policy(&Policy)` only narrows: permissions/capabilities, message, queue, log and deadline limits apply to subsequent checks. It cannot undo committed side effects or revoke data already returned to a guest. Live changes to heap size, tree memory budget or sandbox counts are explicitly unsupported and reject typed; recreate the sandbox to change them. Existing children keep the ceilings they were created under.

## Host capabilities and resources

Policies deny by default. Host-created identities and owner/generation checks, rather than guest-supplied IDs, authorize resource access. Child authority is bounded by the host ceiling, the parent's effective policy, the parent's delegation policy and the child request. An authorized parent can intentionally proxy work for a child.

In the core API (`HostContext`), host callbacks receive trusted identity, bounded input, deadline/cancellation and an explicit business scope. Guest JSON is data only: fields such as `tenant`, `scope` or `sandboxId` in the payload cannot replace the trusted context, and guest-supplied ids never authorize resources (owner/generation checks do). Ungranted or policy-denied capabilities are rejected before dispatch; registered `JsonSchema` input/output validation and schema traversal budgets apply before/after the callback. `HostAudit` records bounded metadata only (sandbox id, registered capability name or the fixed `<ungranted>`/`<invalid>` tag, outcome) and never payloads, scopes or callback error text. Register business operations such as `orders.read`, not raw database handles, arbitrary SQL or private keys. Callback work never holds the supervisor or registry lock. Timeout/cancellation does not roll back a committed business side effect. A host callback that ignores cancellation must remain accounted for until it actually finishes.

## Workers

Managed guest Workers (`new Worker('./child.js', {type: 'module'})`) are a bounded JSON request/reply subset over modules inside the frozen graph. The child's message handler may reply with JSON; there are no unsolicited top-level pushes, timers, streaming, `eval`, classic Workers, transferables/`SharedArrayBuffer`, `sharedEnv` or native Workers. The browser-style `terminate()` returns a joinable Promise. `node:worker_threads` is aliased only in the explicit `PureJsNpm` profile. Children occupy sandbox-count and memory slots inherited from their root.

## Errors and protocol

The Rust error enum exposes stable `kind()` categories; native bindings translate those categories. Unsupported languages/APIs and unavailable backends fail explicitly. Capability negotiation is performed before guest execution. Guest/host framing uses protocol version 1 (`DSB1`), bounded payloads and explicit request IDs; unknown versions, reserved bytes, malformed lengths and trailing data are rejected. Business operation schemas are separate from transport framing.

The public facade re-exports policy, schema, audit and mount types but not `HostContext`; host-boundary tests that need the trusted context type (e.g. `core/tests/host_security.rs`) use `denosanbox-core::HostContext` directly, which is the same crate the facade is built on.

## Network

`fetch` is denied unless a policy rule matches the host and port (plus an explicit private-address grant for non-global addresses). Every DNS answer and redirect hop is re-checked, with request/response byte and deadline bounds. Each DNS query retains its own native-completion admission until native cleanup finishes; unrelated queries do not inherit it. A configured proxy the connector cannot support fails closed rather than connecting directly. Mounts use no-follow capability handles with per-owner generation checks.

## Toolchain and release policy

The validated profile is Linux x86_64 with Rust 1.99.1, `deno_core 0.413.0`, the V8 version selected by its lockfile, `deno_ast 0.53.3`, CPython 3.12, Node exactly 24.22.1 and Go 1.27.3. Commit lockfiles for reproducibility and rerun conformance/native coexistence tests before upgrades. Deno extensions/snapshots from another version cannot be mixed into this adapter. There is no externally generated snapshot in this implementation. Bindings release independently from the facade's 0.1 API. Installing Python or Node packages never grants native-code execution to guest modules, and no package is published by this repository.
