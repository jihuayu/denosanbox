# Threat model

Status: describes the implemented Rust in-process Deno/V8 containment layer as
of the current `core` workspace. Claims below are scoped to the **validated
profile** in [What has actually been validated](#what-has-actually-been-validated);
everything else is design intent or a known gap. Test names refer to the
repository's executable suites.

## Assets and trust boundaries

- **Trusted:** the Rust host process, its configuration (`EngineConfig`,
  `HostConfig`, `Policy`), the module/package preparation step, the host
  callbacks, the HTTP connector/resolver and the audit sink.
- **Untrusted:** guest JavaScript/TypeScript, guest module specifiers, guest
  messages, guest-requested virtual paths, environment values and any data
  returned by a guest.
- **Shared:** one OS process, one address space, one allocator, host file
  descriptors and V8's process-wide platform state. The guest is not a separate
  OS security principal.

The sandbox is a containment layer inside a trusted host, not an OS sandbox. It
must not be offered as protection against a malicious host operator or against
arbitrary native code.

## Runtime profile (explicit)

| Capability | Status |
| --- | --- |
| ESM (`js`/`mjs`) modules, static imports restricted to the prepared graph | allowed |
| TypeScript (`ts`/`mts`) | allowed; transpiled with `deno_ast`, **not type-checked** |
| Approved, host-prepared vendored pure-JS packages | allowed through `denosanbox::modules` (digest-verified, no lifecycle scripts/native deps) |
| JSX/TSX, CommonJS, WASM, JSON module imports | denied |
| Managed JSON Workers created by the runtime in response to guest `new Worker`-style calls | **forthcoming**, owned by the runtime job; the current facade exposes a host-created `WorkerHandle` request/reply API and guest Worker creation is not wired |
| Raw `Deno.core`, `Deno.*`, `process`, `require` | removed/absent before guest evaluation |
| `SharedArrayBuffer`, `Atomics`, `WebAssembly` | deleted from the realm |
| Native addons, FFI/`dlopen`, subprocess creation, signal handling, inspector/debugger attach, cluster | **never allowed**; no mediated API exists |
| `process.exit` equivalent | `sandbox.exit()` only terminates the guest's own isolate; it cannot exit the host |

## Denial model (implemented)

1. **Realm isolation.** Every sandbox creates a fresh V8 isolate on a dedicated
   worker thread (`DenoFactory`), loads a bootstrap that removes the trusted op
   namespace, deletes the shared-memory/atomics/WASM globals and defines the
   frozen mediated APIs. Engine-level tests: `core/tests/conformance.rs`,
   `core/tests/runtime_limits.rs`.
2. **Module isolation.** `GraphLoader` only resolves `sandbox:///` relative
   paths inside the prepared graph; computed dynamic imports cannot reach the
   network or filesystem. Tests: `conformance.rs::real_js::*`, `module_profiles.rs`.
3. **Guest API surface.** `sandbox.{fs,env,host,cwd,chdir,exit}`, `fetch` and
   (host-created) Workers are the only mediated functions. Each goes through
   `HostContext`, which checks the sandbox `Policy` before any I/O.
4. **Message validation.** Facade inputs/results are size-checked
   (`max_message_bytes`, default 1 MiB) before submission and after return;
   host callbacks receive bounded byte buffers. Tests:
   `runtime_limits.rs::oversize_inputs_and_results_are_typed`.
5. **Host boundary.** Host grants are name+scope pairs registered on the engine,
   bound to a host-minted `SandboxId`; the guest cannot supply identity or scope.
   Guest-supplied IDs are data only. Business operation schemas (`JsonSchema`)
   validate input *before* dispatch and the result *before* return, with depth,
   node and string budgets. Ungranted or policy-denied capabilities are rejected
   before the callback runs. Audit events carry only sandbox id, capability
   name (or the fixed `<ungranted>`/`<invalid>` tags) and an outcome — never
   payloads, scopes, environment values or callback error text. Tests:
   `core/tests/host_security.rs`, `core/core/src/host.rs` unit tests.
6. **Memory.** The isolate's own JS heap target (default 64 MiB) is handed to
   V8, and `ArrayBuffer` backing stores go through a budget-accounted allocator
   charged against a subtree memory budget (default 256 MiB) shared with
   ancestors. Other sandboxes are unaffected. **Known gaps (pending runtime
   fix):** a 9 MiB store in a 16 MiB heap / 24 MiB tree sandbox is refused as
   `limit_exceeded { limit: "js_heap_target" }` by the near-heap-limit callback,
   which also closes that sandbox, instead of a distinct subtree-budget error
   that leaves it usable (`runtime_limits.rs::array_buffer_over_tree_budget_is_bounded_and_typed`,
   ignored). This is accounting, **not** a hard RSS ceiling.
7. **Logs.** `console.*` output is discarded after being charged against a
   per-invocation retention cap; there is no host stdout sink. The default cap
   is 64 KiB. **Known gap (pending runtime fix):** the cap is hardcoded in the
   bootstrapped JS and the effective/live-narrowed `Policy::limits().max_log_bytes()`
   is not re-read per invocation, so a narrower policy is not yet enforced
   (`runtime_limits.rs::narrowed_policy_log_cap_is_enforced_live`, ignored).
8. **Network.** Deny by default. Every request, **every DNS answer** and every
   redirect hop must match an explicit rule (host pattern, port range, optional
   private-address grant for non-global IPs) plus request/response byte and
   deadline limits. Private/loopback/link-local addresses require
   `allow_private*`. A configured proxy that the connector cannot support fails
   closed (`unsupported_proxy`) rather than silently connecting directly.
   Tests: `core/connectors`, `host.rs` network tests, `mounts`/`host_bridge`
   integration.
9. **Mounts.** Host directories are attached with no-follow capability handles,
   per-owner generation checks, `MountAccess` (read/write) and a max mount count;
   closing a sandbox closes its mount table.
10. **Deadlines and cancellation.** A watchdog thread plus the facade deadline
    interrupt synchronous loops, pending promises and queued work; timeout and
    cancel are terminal for the sandbox. A closed sandbox rejects later calls
    (`closed`/`cancelled`) and queued waiters are settled with typed errors.
    Host callbacks that ignore cancellation keep their accounting (outstanding
    counter, admission lease, context ownership) until they really finish, and
    the close report marks the sandbox as not cleanly closed when such work
    remains at the join grace boundary.

## Shared-process risks (not contained)

- **Memory exhaustion.** A guest can still drive the *process* to OOM through
  paths outside the accounted JS heap/ArrayBuffer path (code, internal V8
  structures, concurrent isolates); a process-wide OOM can abort the host.
- **Native faults.** V8, `deno_core`, Rust `unsafe` code or a host integration
  bug can crash or corrupt the shared process. Guest-triggered panics in host
  callbacks become typed `backend` errors, but aborts and native assertion
  failures are unrecoverable.
- **V8 platform state.** Isolates registered on a thread without a Tokio
  runtime handle abort the process when V8 posts a delayed task (observed with
  the current `DenoFactory`, which builds `JsRuntime` without
  `executor.enter()`; test runtimes must therefore be multi-thread Tokio
  runtimes and the runtime owner is fixing the factory).
- **Deadline limits.** Deadlines cannot preempt every native operation and
  cannot roll back committed side effects (VFS writes, callback effects, CPU
  and memory consumed).
- **Non-cooperative host work** (blocking callbacks, `getaddrinfo`) keeps
  native threads, and its admission accounting, until it really returns.
- **Resource accounting** is bounded per process (e.g. host concurrency
  admission), so hostile guest load can still degrade availability.

Operational consequence: run untrusted workloads with an external supervisor and
OS-level memory/CPU limits when stronger isolation is needed.

## What has actually been validated

- **Platform:** Linux x86_64 only. No other OS/architecture is claimed tested.
- **Toolchain:** Rust 1.99.1, `deno_core = 0.413.0` (V8 pinned by its
  lockfile), Node 24.22.1 and CPython 3.12 for the bindings, Go for the adapter.
- **Executable suites:** `core` workspace tests (facade, policy, modules,
  mounts, connectors, testkit parity, host bridge, runtime limits, host
  security), Python and Node binding tests, the C/Go adapter tests via
  `bindings/c/test.sh`, fuzz smoke runs and the benchmark example. Commands are
  listed in the README and CI workflow.
- `dev/validate-fixtures.py` only validates the declarative
  `fixtures/conformance` catalog structurally; it never executes JavaScript
  and proves no property.

## Known gaps

- Narrowed `max_log_bytes` policy not enforced (above).
- ArrayBuffer over-budget rejection is reported as `js_heap_target` and closes
  the sandbox (above).
- Managed runtime-created Workers pending the runtime job.
- No C-binding callback API yet (polling subset only).
- No hard per-sandbox RSS limit, CPU isolation or timeout rollback.

## Reporting assumptions

Any security claim must name the tested profile, Deno/V8 version, host
configuration, deadline and resource limits. Passing tests show only that the
listed cases behaved as expected in that configuration; they do not prove the
absence of escapes, leaks or denial-of-service paths.
