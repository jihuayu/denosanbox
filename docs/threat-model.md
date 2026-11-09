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
6. **Memory.** Each sandbox's own JS heap target (default 64 MiB, minimum
   8 MiB on Deno) is set as a V8 *finite best-effort* heap limit with roughly
   4 MiB of headroom before the near-heap-limit callback fires
   (`heap_limits(0, target - 4 MiB)`). `ArrayBuffer` backing stores go through a
   budget-accounted allocator charged against a subtree memory budget (default
   256 MiB) shared with ancestors. This is accounting, **not** a hard RSS cap:
   hard RSS limits and containment of native/process-wide OOM are explicitly not
   promised, and V8-internal OOM cannot be repaired from the host.
   Exhaustion may *retire* the isolate: the call fails with a typed terminal
   limit error (`LimitExceeded { limit: "js_heap_target" | "subtree_memory_bytes" }`)
   and the near-heap-limit callback cancels the host context, so later calls on
   that sandbox report `closed`/`cancelled`. That is accepted behaviour as long
   as siblings and newly created sandboxes stay usable and an explicit
   `close()`/drop releases its admission and reservations. The backend never
   leaves a denied sandbox in an undefined working state, and denials never
   surface as arbitrary `execution`/`backend` errors. On the current runtime
   snapshot a 9 MiB store under a 16 MiB heap / 24 MiB tree sandbox is refused as
   `js_heap_target` and retires that isolate (`runtime_limits.rs::array_buffer_over_tree_budget_is_typed_and_cleanly_retired`).
7. **Logs.** `console.*` output is discarded after being charged against a
   per-invocation retention cap; there is no host stdout sink. The default cap
   is 64 KiB. The cap must follow the effective `Policy::limits().max_log_bytes()`
   of the invocation, including after live `narrow_policy`. The runtime owner has
   implemented this dynamic-context cap; the acceptance test
   `runtime_limits.rs::narrowed_policy_log_cap_is_enforced_live` runs active and
   fails on any backend that still hardcodes 64 KiB until that integration is
   merged.
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

- **Memory exhaustion.** Heap targets and budgets are finite best-effort
  accountings; a guest can still drive the *process* to OOM through paths
  outside them (internal V8 structures, concurrent isolates) and a
  process-wide OOM can abort the host. Hard RSS limits are not promised.
- **Native faults.** V8, `deno_core`, Rust `unsafe` code or a host integration
  bug can crash or corrupt the shared process. Guest-triggered panics in host
  callbacks become typed `backend` errors, but aborts and native assertion
  failures are unrecoverable.
- **V8 platform state.** deno_core captures the Tokio runtime `Handle` when
  `JsRuntime::new` runs and aborts the process if V8 later posts a delayed task
  without one. The runtime owner is fixing `DenoFactory` to enter its executor
  before construction; until merged, tests use multi-thread Tokio runtimes.
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

- Narrowed `max_log_bytes` acceptance depends on the runtime owner's dynamic-
  context implementation being merged (test is active, not ignored).
- Memory exhaustion retires the affected isolate rather than preserving it for
  reuse (accepted behaviour above); no hard RSS/native-OOM containment.
- Managed runtime-created Workers pending the runtime job.
- No C-binding callback API yet (polling subset only).
- No hard per-sandbox RSS limit, CPU isolation or timeout rollback.

## Reporting assumptions

Any security claim must name the tested profile, Deno/V8 version, host
configuration, deadline and resource limits. Passing tests show only that the
listed cases behaved as expected in that configuration; they do not prove the
absence of escapes, leaks or denial-of-service paths.
