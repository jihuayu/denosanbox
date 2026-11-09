# Threat model

Status: design intent for the Rust in-process Deno sandbox. The core runtime,
policy layer, and fixture harness are still being implemented. Nothing in this
document should be read as evidence that isolation, deadlines, or cleanup have
been validated at runtime.

## Assets and trust boundaries

- **Trusted:** the Rust host process, its configuration, the authorization
  policy, the module resolver, and host callbacks.
- **Untrusted:** guest JavaScript, guest module specifiers, guest messages,
  guest-requested paths, environment values, and any data returned by a guest.
- **Shared:** one OS process, one address space, one allocator, and the host's
  file descriptors. The guest is not a separate security principal at the OS
  level.

The sandbox is a containment layer inside a trusted host, not an OS sandbox.
It must not be offered as protection against a malicious host operator or
against arbitrary native code.

## Denial model

The intended default is deny unless a capability is explicitly mediated:

1. **Global isolation.** Each sandbox instance receives a fresh JavaScript
   realm. Guest mutations to globals and prototypes must not reach the host or
   another instance.
2. **Module isolation.** Static and dynamic imports resolve only through the
   host module loader against an explicit allowlist or sandbox root. Relative
   traversal, `file:` URLs, remote URLs, Node builtins, and computed specifiers
   are denied unless a mediated module has been registered. Static-import
   denial should fail before guest module evaluation.
3. **Guest API denial.** `process.exit`, `process.binding`, native addon
   loading, FFI/`dlopen`, subprocess creation, signals, cluster, inspector, and
   equivalent aliases are absent or deny without performing the operation.
   `process.exit` must never terminate the host.
4. **Message and path validation.** Host messages and guest-requested paths are
   parsed with explicit size and shape limits before they reach runtime code.
   Malformed or oversized input is rejected, not truncated into a weaker form.
5. **Deadlines.** Synchronous loops, infinite microtask chains, and pending
   promises must be bounded by a host deadline. Timeout is a failure outcome;
   partial results from a timed-out instance are not accepted.
6. **Teardown.** Closing an instance is idempotent and must release its isolate,
   task queue, channels, and reserved resources. A closed instance must reject
   new work and must not be resurrected by a queued callback.

Denial must be explicit and observable as an authorization, resolution, or
timeout failure. A silent fallback to a weaker API is a security bug.

## Shared-process risks

These risks are inherent to running Deno/V8 inside the host process and are not
fully solvable by the sandbox API:

- **Memory exhaustion.** A guest can allocate until the shared process hits its
  memory limit. There is no hard per-instance RSS cap in the in-process profile.
  A host-wide OOM can abort the entire process. Accounting and heap limits are
  mitigations, not isolation.
- **Native faults.** Bugs in V8, `deno_core`, Rust unsafe code, or a host
  integration can crash or corrupt the shared process. Guest-triggered
  `abort`, panic, stack overflow, or native assertion failures may be
  unrecoverable. The sandbox does not make untrusted native code safe.
- **Deadline limits.** A deadline can stop or discard a guest task, but it
  cannot preempt every native operation and cannot roll back side effects that
  already occurred. Guest writes through host callbacks, partial output,
  consumed CPU, and allocated memory may persist after a timeout.
- **No rollback guarantee.** Timeout and teardown are cleanup attempts, not
  transactions. Host state changed by an authorized callback before a timeout
  is not automatically reverted.
- **Shared fate.** File descriptors, environment variables, working directory,
  signal handlers, and process-wide resources are shared unless a mediated API
  explicitly virtualizes them. A bug in one instance can affect all instances
  and the host.

Operational consequence: run untrusted workloads with an external supervisor,
OS-level memory/CPU limits, and restart policy when stronger isolation is
required. The in-process profile is intended for trusted deployments that need
capability mediation and fault containment, not hostile multi-tenant isolation.

## Availability and compatibility limits

Not available until implemented and validated:

- managed child Workers and recursive worker supervision;
- npm package installation or Node compatibility beyond mediated pure-JS
  modules;
- Python, Node-API, or other language bindings;
- a hard per-instance RSS limit, CPU isolation, or timeout rollback;
- guest native addons, FFI, subprocesses, signals, inspector, or `cluster`.

The repository currently contains declarative conformance fixtures and a
structural validator. `dev/validate-fixtures.py` does not execute JavaScript and
does not demonstrate that any denial or cleanup property holds. Runtime
conformance requires the core facade and testkit, which are pending.

## Reporting assumptions

Any future security claim must name the tested profile, Deno/V8 version, host
configuration, deadline, and resource limits. Passing fixtures show only that
the listed cases behaved as expected in that configuration; they do not prove
the absence of escapes, leaks, or denial-of-service paths.
