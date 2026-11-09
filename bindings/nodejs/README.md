# Experimental Node.js binding

Version 0.1.0 calls the `denosanbox` 0.1.x Rust facade directly through napi-rs.
It does not start Deno, a subprocess, or a sidecar. The addon is trusted host
code; guest native addons remain unavailable.

This package is private while its release gate is incomplete. The loader
explicitly rejects combinations outside Linux x86_64 / Node 24.22.1. A successful
local load is evidence for that exact build, not a guarantee for every Node 24
patch release, libc, linker, or operating system.

## Build and test

Install Rust, a C/C++ linker, Python 3 (for build metadata), and Node 24.22.1. From a
clean checkout:

```sh
cd bindings/nodejs
npm run build
npm test
node ../../examples/nodejs/main.cjs
```

The first Rust build downloads the pinned deno_core/V8 artifact. Build output
`denosanbox.node` is a native library copied from Cargo's target directory;
there is no runtime executable dependency. Cargo.lock records the dependency
versions for repeatable development builds.

```js
const {Engine} = require('denosanbox');
const engine = new Engine();
try {
  const plugin = engine.prepare('export function run(input) { return input; }');
  const sandbox = await engine.create(plugin);
  console.log(await sandbox.call('run', {value: 42}, {timeoutMs: 100}));
  await sandbox.close();
} finally {
  await engine.shutdown();
}
```

Calls return Promises and execute on the Rust runtime rather than the Node event
loop. `cancel()` requests terminal cancellation of its sandbox; it does not
roll back side effects. `close()` / `shutdown()` must be awaited: `close()`
resolves to the facade `CloseReport` (`closedCleanly`, `forcedTermination`,
`invocationsCompleted`, `invocationsCancelled`, `pendingHostOperations`) and
`shutdown()` resolves to whether core reports clean shutdown. An incomplete
report is never upgraded to success; `Symbol.asyncDispose` throws
`incomplete_close` with the pending count. Losing a JavaScript reference is not a
substitute for explicit close. JSON messages are limited to the default core
1 MiB limit. Errors expose a stable `kind` and a human-readable message.

## V8 coexistence gate

The initial environment diagnostic on 2026-11-09 used Node v24.22.1
(V8 13.6.233.17-node.58), Linux x86_64, and a Rust cdylib containing
deno_core 0.413.0. A small N-API shim loaded the cdylib with
`dlopen(..., RTLD_NOW | RTLD_LOCAL)`, called its `environment_eval` export twice,
and allowed a Node `setImmediate` to execute. Both evaluations returned 42 in
the Node host process. The command shape was:

```sh
cc -Wall -Wextra -Werror -fPIC -shared node-gate.c -ldl -o node-gate.node
node -e 'const a=require("./node-gate.node"); console.log(a.evaluate(), a.evaluate()); setImmediate(()=>console.log("node_event_loop_ok"));'
# 42 42
# node_event_loop_ok
```

That probe checked environment linkage only. The repository's actual napi-rs
addon gate is `npm run build && npm test`: it exercises two independent engines,
state separation, Promise calls, Node timers during an infinite guest loop,
deadlines, cancellation, repeated close, and shutdown order. Never infer addon
support solely from the preliminary C shim.

## Module graphs, profiles, policy and mounts

```js
const plugin = engine.prepareModules({
  'main.ts': "import { twice } from './lib/math.ts'; export function run(n: number) { return twice(n); }",
  'lib/math.ts': 'export function twice(n: number): number { return n * 2; }',
}, 'main.ts', 'javascript-only');
plugin.modulePaths; // ['lib/math.ts', 'main.ts']
plugin.profile;     // 'javascript-only'
```

`prepare(source, entrypoint)` remains the one-module shortcut. `prepareModules`
freezes a host-supplied graph; static imports, re-exports, computed dynamic
imports and a guest `new Worker('./worker.js', {type: 'module'})` graph
resolve only inside it. `profile` is explicit: `'javascript-only'` (default) or
`'pure-js-npm'` (host-prepared vendored package graph plus the trusted
`worker_threads` alias). The validated profile is stored on the
`PreparedPlugin` and passed through `create` to the facade
`Engine::prepare_with_profile`; an unknown profile name is rejected and is
never silently downgraded to `javascript-only`. No profile grants
VFS/env/network/callback access. The tests create the `pure-js-npm` sandbox and
call an exported function that uses an imported `worker_threads` alias, and
confirm the default `javascript-only` graph still denies that alias.

`new Engine(options)` also exposes the facade hard limits (`EngineLimits`),
enforced on every `create`, including a `PreparedPlugin` prepared by a
different engine: `maxActiveSandboxes`, `maxPendingInvocations`,
`engineMessageBytes` (the hard JSON limit, distinct from the policy
`maxMessageBytes`), `defaultInvocationTimeoutMs`, `maxInvocationTimeoutMs`,
`maxModules`, `maxModuleBytes`, `maxGraphBytes`. The effective values are
readable through `engine.limits`.

`new Engine(options)` builds the immutable, deny-by-default ceiling from trusted
host values: `hostCapabilities`, `readRoots`, `writeRoots`, and resource limits
mapped one-to-one to the facade policy `Limits`: `heapTargetBytes` (per-isolate
JS heap target, not RSS), `memoryBudgetBytes` (aggregate budget for the sandbox
and descendants), `maxSandboxes`, `deadlineMs`, `queueDepth`,
`maxMessageBytes`, `maxLogBytes`. `engine.create(plugin, options)` sends the
per-sandbox *request* (`capabilities`, `readRoots`, `writeRoots`, `deadlineMs`,
`queueDepth`, `maxMessageBytes`, `maxLogBytes`, `defaultTimeoutMs`,
`memoryLimitBytes`) which the facade narrows against the ceiling; exceeding it
rejects with `kind === 'policy'`. `memoryLimitBytes` is the isolate heap
reservation while `memoryBudgetBytes` is the subtree budget.

`sandbox` guest access to the private VFS (`sandbox.fs.readTextFile`/
`writeTextFile`) and env (`sandbox.env.get/set`) requires the matching roots and
the `env` capability in both the ceiling and the sandbox request. Hosted
directories are attached with trusted values only:

```js
engine.mountDirectory(sandbox, '/mount', '/srv/tenant-a', 'read-write'); // or 'read'
```

The facade retains the directory capability (rejects symlinks and traversal)
and still requires the sandbox policy to allow `/mount`. Every sandbox `call`
takes exactly one JSON `input` value (plus optional `{timeoutMs, signal}`), so
export one object when an operation needs several fields:
`sandbox.call('read', {path: '/mount/host.txt'})`. Grant scopes always
come from the host's `hostGrants` object; a forged scope inside guest JSON has
no effect.

## Remaining release requirements

Async host callbacks use bounded N-API threadsafe dispatch and return Promises. Shared behavior fixtures, multi-module/profile/VFS/mount tests and explicit process shutdown run against the native addon. Broader platform/Node version support and registry publication remain outside the tested matrix. No automatic
fallback is provided for unsupported APIs, platforms, or failed native loading.

The same-process threat model applies: native faults and process OOM can affect
the Node host. No per-isolate hard RSS limit or native crash containment is
claimed. Node's V8 values never cross into the embedded V8 runtime; only JSON
strings cross the facade.

## Local validation — 2026-11-09

The actual napi-rs addon built and loaded on Linux x86_64, Node v24.22.1
(V8 13.6.233.17-node.58), Rust 1.99.1, deno_core 0.413.0, napi 3.16.1.
All three `node --test tests/native.test.cjs` cases passed (0 skipped), and the
native example completed with two independent guest counters. Cargo fmt and
clippy with `-D warnings` passed. `npm pack --dry-run` confirmed that the native
artifact, declarations, loader, README, and license are included. The local
debug artifact was about 175 MB unpacked; release artifacts and install tests
are still a release requirement. Nothing was published to npm.

## Scoped async host callbacks

```js
const engine = new Engine({hostCapabilities: ['orders.read']});
engine.registerHost('orders.read', async (context, input) => {
  // context.scope is host-provided; input is untrusted guest data.
  return {scope: context.scope, pid: process.pid};
});
const plugin = engine.prepare("export async function run(input) { return await sandbox.host.call('orders.read', input); }");
const sandbox = await engine.create(plugin, {hostGrants: {'orders.read': 'tenant-a'}});
try { console.log(await sandbox.call('run', {scope: 'untrusted'})); }
finally { await sandbox.close(); await engine.shutdown(); }
```

Trusted callback context has `sandboxId`, `scope`, dispatch-time `remainingMs`,
and an `AbortSignal`. Callback input is a separate JSON value. Cancellation,
close, and deadlines abort the signal; callbacks must cooperate and check it.
Non-cooperative work remains owned/accounted by core and close may return false.
`Symbol.asyncDispose` throws `incomplete_close` rather than discarding a false
cleanup report. Timeouts do not roll back committed side effects. Callbacks
execute on the Node event loop, so avoid synchronous blocking work. N-API
callback queues are bounded to 32 entries. Callback failures are sanitized.

The completed local callback gate passed **10 native tests** with zero skips,
including the four shared behavior fixtures and process exit after explicit
callback close. Multi-module/profile/VFS/mount parity tests were added in
`tests/modules.test.cjs` (including a bounded 9 MiB `ArrayBuffer` probe against a
16 MiB heap target / 24 MiB tree budget; no unbounded heap allocation is run);
see `bindings/python/VALIDATION.md` for status. An actual `npm pack` archive was installed into a separate local
package directory, then loaded and exercised with an async host PID callback;
that installed-package smoke test passed. No package was published. The loader
and package engine field intentionally accept only Node **24.22.1** until more
combinations are verified.
