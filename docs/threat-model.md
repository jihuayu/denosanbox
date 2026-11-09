# Threat model

Status: describes the implemented Rust in-process Deno/V8 containment layer at
implementation commit `4156653`. The production backend is the embedded
`deno_core`/V8 runtime inside the host process; no Deno executable, sidecar or
subprocess is involved. Claims below are scoped to the **validated profile** in
[What has actually been validated](#what-has-actually-been-validated); everything
else is design intent or a known gap. Test names refer to the repository's
executable suites.

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
| ESM (`js`/`mjs`) modules, imports/re-exports/dynamic imports restricted to the frozen host-vendored graph with ordinary relative paths | allowed |
| TypeScript (`ts`/`mts`) | allowed; transpiled with `deno_ast`, **not type-checked** |
| Approved, host-prepared vendored pure-JS packages | allowed through `denosanbox::modules` (digest-verified, no lifecycle scripts/native deps, no npm installer or runtime download) |
| Approved builtins | only the explicit `PureJsNpm` profile alias `node:worker_threads` |
| JSX/TSX, CommonJS, WASM, JSON module imports | denied |
| Managed guest Workers (`new Worker('./child.js', {type: 'module'})`) | bounded JSON request/reply subset during the child's message handler; browser `terminate()` returns a joinable Promise; no unsolicited top-level pushes, timers, streaming, `eval`/classic Workers, transferables/`SharedArrayBuffer`, `sharedEnv` or native Workers |
| Raw `Deno.core`, `Deno.*`, `process`, `require` | removed/absent before guest evaluation |
| `SharedArrayBuffer`, `Atomics`, `WebAssembly` | deleted from the realm |
| Native addons, FFI/`dlopen`, subprocess creation, signal handling, inspector/debugger attach, cluster | **never allowed**; no mediated API exists |
| `process.exit` equivalent | `sandbox.exit()` only terminates the guest's own isolate; it cannot exit the host |

## Denial model (implemented)

1. **Realm and state isolation.** Every sandbox creates a fresh V8 isolate on a
   dedicated worker thread (`DenoFactory`), loads a bootstrap that removes the
   trusted op namespace, deletes the shared-memory/atomics/WASM globals and
   defines the frozen mediated APIs. Prepared graphs share source text only,
   never module namespaces. Tests: `core/tests/conformance.rs`,
   `core/tests/runtime_limits.rs`.
2. **Module validation.** Module graphs are validated at `create`, including
   re-validation of a `PreparedPlugin` from another engine against the creating
   engine's `EngineLimits` (module count/bytes, graph bytes, message and deadline
   limits). `GraphLoader` only resolves ordinary relative paths inside the
   prepared graph; computed dynamic imports cannot reach network or filesystem.
   Tests: `conformance.rs`, `module_profiles.rs`, `prepared_limits.rs`.
3. **Trusted identities and policies.** Host-created `SandboxId`s and
   owner/generation checks authorize resources; guest-supplied IDs are data only.
   Policies deny by default; child authority is the intersection of host ceiling,
   parent's effective policy, delegation policy and child request. Each tree
   enforces max sandbox count (default 4 globally, descendants included) and
   memory/sandbox limits.
4. **Guest API surface.** `sandbox.{fs,env,host,cwd,chdir,exit}`, `fetch` and
   managed Workers are the only mediated functions; each checks the effective
   `Policy` before I/O.
5. **Live narrowing.** `SandboxHandle::narrow_policy` narrows permissions,
   messages, queue depth, log retention and deadlines for subsequent checks,
   including per-invocation log retention. Live heap size, tree-memory budget and
   sandbox-count changes are explicitly unsupported and rejected (recreate the
   sandbox). Existing children retain their creation ceilings; root cancel
   revokes the whole subtree. Tests: `policy_limits.rs`,
   `runtime_limits.rs::narrowed_policy_log_cap_is_enforced_live`.
6. **Message validation.** Facade inputs/results are size-checked
   (`max_message_bytes`, default 1 MiB) before submission and after return;
   host callbacks receive bounded byte buffers. Tests:
   `runtime_limits.rs::oversize_inputs_and_results_are_typed`.
7. **Host boundary.** Host grants are name+scope pairs registered on the engine,
   bound to a host-minted `SandboxId`. Business operation schemas (`JsonSchema`)
   validate input before dispatch and the result before return, with depth, node
   and string budgets. Ungranted or policy-denied capabilities are rejected
   before the callback runs. Audit events carry only sandbox id, capability name
   (or the fixed `<ungranted>`/`<invalid>` tags) and an outcome — never payloads,
   scopes, environment values or callback error text. The advanced
   `Engine::with_host_config` schemas/audit/shared pools are a Rust-facade
   surface; other SDKs do not expose them. Tests: `core/tests/host_security.rs`,
   `host_bridge.rs` and the `bindings/c/test.sh` host-callback example.
8. **Memory.** Each sandbox's JS heap target (default 64 MiB) is set as a V8
   finite best-effort heap limit with a finite 4 MiB termination headroom
   (`heap_limits(0, target - 4 MiB)`). `ArrayBuffer` backing stores are charged
   against the per-tree managed-memory budget (default 256 MiB) shared with
   ancestors; the Rust `HostConfig` shared pool defaults to 512 MiB and an
   explicitly smaller pool is never auto-expanded. This is accounting, **not** a
   hard RSS cap: hard RSS limits and containment of native/process-wide OOM are
   explicitly not promised. Exhaustion may retire the isolate with a typed
   terminal limit error (`LimitExceeded { limit: "js_heap_target" |
   "subtree_memory_bytes" }`); siblings and new sandboxes stay usable and
   explicit `close()`/drop releases admission. Tests: `runtime_limits.rs`,
   `policy_limits.rs`, `guest_workers.rs`.
9. **Logs.** `console.*` output is charged against the invocation's effective
   `max_log_bytes` (default 64 KiB) and discarded; there is no host stdout sink.
10. **Network.** Deny by default. Every request, **every DNS answer** and every
    redirect hop must match an explicit rule (host pattern, port range, optional
    private-address grant for non-global IPs) plus request/response byte and
    deadline limits. Each DNS query holds its own native-completion admission
    until native cleanup ends; unrelated queries do not inherit it, so they are
    not blocked or drained by each other. A configured proxy that the connector
    cannot support fails closed (`unsupported_proxy`) rather than silently
    connecting directly. Tests: `core/connectors`, `host.rs` network tests,
    `mounts`/`host_bridge` integration.
11. **Mounts.** Host directories are attached with no-follow capability handles
    and confinement (symlinks and traversal rejected), per-owner generation
    checks, `MountAccess` (read/write) and a max mount count; closing a sandbox
    closes its mount table.
12. **Deadlines and cancellation.** A watchdog plus the facade deadline
    interrupt synchronous loops, pending promises and queued work; timeout and
    cancel are terminal for the sandbox and revoke its subtree. A closed sandbox
    rejects later calls with typed errors. Host callbacks that ignore
    cancellation keep their accounting (outstanding counter, admission lease,
    context ownership) until they really finish, and the close report marks the
    sandbox as not cleanly closed when such work remains at the join grace
    boundary. In C/Go, a callback's `user_data` is released exactly once after
    the last registration, grant and in-flight invocation are gone — there is no
    deferred tombstone and no global-thread heuristic. Go context cancellation
    preserves the deadline identity of the native call.

## Bindings

- **C ABI** (`deno_abi_version()` — read `bindings/c/include/denosanbox.h` for the
  current version): owned, kind-checked, non-reused `u64` tokens; polling
  completion; scoped native host callbacks with trusted context, bounded
  input/output and release-after-last-owner semantics. Recoverable panics are
  contained at the extern boundary; aborts, OOM and native pointer faults are not.
- **Go adapter:** cgo over the C ABI; context-aware calls, terminal cancellation,
  idempotent close and native callbacks released without tombstones.
- **Python** (CPython 3.12) and **Node** (exactly 24.22.1): asyncio / N-API
  callbacks, explicit ceilings, grants and mounts, but not the Rust
  `HostConfig` schemas/audit/shared-pool surface.

## Shared-process risks (not contained)

- **Memory exhaustion.** Heap targets and budgets are finite best-effort
  accountings; a guest can still drive the *process* to OOM through paths
  outside them, and a process-wide OOM can abort the host. Hard RSS limits and
  native OOM/crash containment are not promised.
- **Native faults.** V8, `deno_core`, Rust `unsafe` code or a host integration
  bug can crash or corrupt the shared process. Aborts and native assertion
  failures are unrecoverable.
- **Deadline limits.** Deadlines cannot preempt every native operation and cannot
  roll back committed side effects (VFS writes, callback effects, CPU and memory
  consumed).
- **Non-cooperative host work** (blocking callbacks, `getaddrinfo`) keeps native
  threads and its admission accounting until it really returns.
- **Resource accounting** is bounded per process, so hostile guest load can still
  degrade availability.

Operational consequence: run untrusted workloads with an external supervisor and
OS-level memory/CPU limits when stronger isolation is needed.

## What has actually been validated

- **Platform:** Linux x86_64 only. No other OS/architecture is claimed tested.
- **Toolchain:** Rust 1.99.1, `deno_core 0.413.0` (V8 pinned by its lockfile),
  CPython 3.12, Node exactly 24.22.1, Go 1.27.3.
- **Executable suites at `4156653`:** clean `cargo fmt --check` and all-target
  Clippy `-D warnings`; Rust `--all-features` 211 passed and
  `--no-default-features` 155 passed (one ignored upstream doc-test in each);
  `dev/validate-fixtures.py` 8 passed; Node 22/22 with no skips plus an installed
  `npm pack` smoke test; `bindings/c/test.sh` (C examples, `go test -race`, Go
  example) and the Go race deadline/cancel tests 10×.
- **Python:** the installed production wheel passed 41 tests after the
  cross-engine fixture was corrected to use Deno (`56e014a`).
- **Fuzz:** the bounded nightly smoke target runs in CI. The declarative `fixtures/conformance`
  catalog is only validated structurally by `dev/validate-fixtures.py`; it never
  executes JavaScript.

## Known gaps

- No hard per-sandbox RSS limit, CPU isolation or native/process-OOM containment.
- Memory or deadline exhaustion retires the affected isolate rather than
  preserving it for reuse (accepted behaviour above).
- Managed Workers support JSON request/reply from the child's message handler
  only: no unsolicited top-level pushes, timers, streaming, `eval`/classic
  Workers, transferables/`SharedArrayBuffer`, `sharedEnv` or native Workers.
- Live heap, tree-memory and sandbox-count policy changes are unsupported;
  recreate the sandbox.
- Advanced `HostConfig` schemas, audit and shared pools are exposed by the Rust
  facade only, not by every SDK.

## Reporting assumptions

Any security claim must name the tested profile, Deno/V8 version, host
configuration, deadline and resource limits. Passing tests show only that the
listed cases behaved as expected in that configuration; they do not prove the
absence of escapes, leaks or denial-of-service paths.
