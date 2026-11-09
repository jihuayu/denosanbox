# API, ownership and lifecycle

The facade is `core/src/lib.rs`. Public values contain no Deno/V8 objects. The engine-independent model and raw contracts live in `core/core`; `core/runtimes/deno` owns V8 runtimes and compiled state. Bindings consume the facade directly. Optional observability belongs in `core/layers`, reusable behavior contracts in `core/testkit`, native binding packages in `bindings/<language>`, and host framework adapters in `integrations`.

## Prepare and create

`Engine::new(EngineConfig)` validates limits and selects one backend for the engine's lifetime. Backend replacement requires building another Engine; no live sandbox migration occurs. `prepare(PluginSource)` bounds source/graph size, parses/transpiles modules and freezes a private immutable graph. Preparation performs no guest I/O. Reusing a prepared graph shares source text, never module namespaces or mutable JavaScript values.

`create(plugin, SandboxOptions)` reserves admission before starting an execution thread. Each sandbox creates, runs and disposes its isolate on that thread; the driver is intentionally not `Send`. Factories and cancellation controls are thread-safe. Failed or cancelled startup releases its reservation. An engine cannot admit work after shutdown begins.

## Invoke and cancel

`call(export, JSON value, InvocationOptions)` submits to a bounded serialized queue. Different sandboxes run concurrently. Full admission returns a typed queue/limit error. JSON inputs/results have byte bounds; function names are validated. Wall-clock deadlines cover queued time and execution, and child contexts inherit the parent's absolute deadline. A watchdog interrupts JavaScript independently of the JavaScript event loop. Cancellation is terminal for the sandbox; create a fresh sandbox for the next task.

A dropped handle requests cancellation without blocking an async host thread. Explicit `close().await` reports observed cleanup and joins managed execution. `Engine::shutdown().await` freezes admission, cancels live roots/descendants and gathers close reports. Close and shutdown are idempotent; a timed-out or non-cooperative native callback can yield an incomplete report while its ownership/accounting remains retained.

## Host capabilities and resources

Policies deny by default. Host-created identities and owner/generation checks, rather than guest-supplied IDs, authorize resource access. Child authority is bounded by the host ceiling, parent's effective policy, parent's delegation policy and the child request. Self-narrowing affects future checks; it does not undo committed side effects or revoke data already returned to a guest. An authorized parent can intentionally proxy work for a child.

Host callbacks receive trusted identity, bounded input, deadline/cancellation and an explicit business scope. Register business operations such as `orders.read`, not raw database handles, arbitrary SQL or private keys. Callback work never holds the supervisor or registry lock. Timeout/cancellation does not roll back a committed business side effect. A host callback that ignores cancellation must remain accounted for until it actually finishes.

## Errors and protocol

The Rust error enum exposes stable `kind()` categories; native bindings translate those categories. Unsupported languages/APIs and unavailable backends fail explicitly. Capability negotiation is performed before guest execution. Guest/host framing uses protocol version 1 (`DSB1`), bounded payloads and explicit request IDs; unknown versions, reserved bytes, malformed lengths and trailing data are rejected. Business operation schemas are separate from transport framing.

The fake backend is an explicitly selected contract test backend. It does not claim JavaScript, TypeScript or host API compatibility. Supported production capabilities are reported by the selected runtime/facade combination.

## Toolchain and release policy

Rust 1.99.1, `deno_core 0.413.0`, the V8 version selected by its lockfile, and `deno_ast 0.53.3` are the validated source/build combination. Commit lockfiles for reproducibility and rerun conformance/native coexistence tests before upgrades. Deno extensions/snapshots from another version cannot be mixed into this adapter. There is no externally generated snapshot in this implementation.

Bindings release independently from the core's 0.1 API. A binding must document its own compatible core version, interpreter/runtime, native artifact platform, ownership and cancellation behavior. Installing Python or Node packages never grants native-code execution to guest modules.
