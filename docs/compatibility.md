# Compatibility and API matrix

This matrix records what implementation commit `4156653` actually provides.
"Validated" means executed by an in-repository test or example on the stated
platform; anything else is explicitly not claimed. The authoritative data model
and lifecycle rules are in [API](api.md); each binding has its own README.

## Platform and toolchains

Only **Linux x86_64** is tested and supported:

| Component | Tested version |
| --- | --- |
| Rust facade and products | Rust 1.99.1 (`rust-toolchain.toml`, rustfmt + Clippy) |
| Embedded runtime | `deno_core 0.413.0` (V8 selected by the lockfile), `deno_ast 0.53.3` |
| Python binding | CPython 3.12 |
| Node binding | Node exactly 24.22.1 |
| Go adapter | Go 1.27.3 |

No other OS, architecture, Python, Node or Go version is claimed. No package is
published to any registry by this repository or its CI; Python and Node packages
are tested from locally built wheel / `npm pack` artifacts.

## Host runtimes and bindings

| Product | Integration | Status |
| --- | --- | --- |
| Rust facade (`denosanbox`) | direct `Engine` API over embedded Deno/V8 (default feature) | production backend; `Engine::with_host_config` exposes schemas, audit and shared host/memory pools |
| `denosanbox` with `--no-default-features` | facade + fake backend | lifecycle contract only; no JavaScript |
| Python | PyO3 via maturin | independently versioned 0.1.0; no published wheel; no advanced `HostConfig` controls |
| Node.js | napi-rs | independently versioned 0.1.0; loader fails closed on other versions/platforms; no advanced `HostConfig` controls |
| C | versioned owned-handle ABI, `deno_abi_version()` (see `bindings/c/include/denosanbox.h`) | polling completion plus native host callbacks; `user_data` released only after the last owner |
| Go | cgo over the C ABI | context-aware polling; native callbacks; context cancellation preserves deadline identity |

## Core API compatibility

- The Rust facade's data model is frozen at `API_VERSION = "0.1.0"`; bindings
  consume that facade directly and version themselves independently.
- The backend is chosen per engine at construction (`EngineConfig.backend`,
  default `BackendKind::Deno`). There is no runtime-swap API and no live sandbox
  migration between backends; build a new `Engine`. An unavailable backend
  fails with a typed `runtime_unavailable` error instead of falling back.
- Host capability protocol version is `1`; `call_versioned` rejects other
  versions. Guest↔host frames use `DSB1` framing with bounded payloads.
- Guest module specifiers are never handed to an external loader; only graph
  modules are loaded.

## Guest language and module profile

| Feature | Supported | Notes |
| --- | --- | --- |
| ESM `.js` / `.mjs` | yes | entrypoint and ordinary relative imports/re-exports/dynamic imports inside the host-vendored graph only |
| TypeScript `.ts` / `.mts` | yes | transpiled, not type-checked |
| JSX / TSX, CommonJS, JSON modules, WASM | no | rejected at preparation/create |
| npm packages | vendored, host-prepared only | digest-verified pure-JS graph via `denosanbox::modules`; no npm installer or runtime download |
| Builtins | approved list only | `node:worker_threads` only in the explicit `PureJsNpm` profile |
| Graph validation | at `create` | including re-validation of foreign `PreparedPlugin`s against the creating engine's `EngineLimits` |
| `fetch` | only when policy grants a matching host/port (and private grant for non-global IPs) | all DNS answers and redirects re-checked; byte and deadline limits; unsupported proxy fails closed |
| `WebAssembly`, `SharedArrayBuffer`, `Atomics` | removed | never re-exposed |
| Workers | managed guest Workers, JSON request/reply during the child message handler | no unsolicited top-level pushes, timers, streaming, `eval`/classic Workers, transfer/SAB/`sharedEnv`/native; browser `terminate()` returns a joinable Promise |
| Native addons, subprocess, signals, inspector, FFI | never | no API surface |

## Limits actually enforced

Defaults are designed so the default sandbox count is usable. Limits are
accounting targets, not hard RSS bounds or native OOM/crash containment.

| Limit | Default | Enforced by | Notes |
| --- | --- | --- | --- |
| global active sandbox count (`EngineLimits.max_active_sandboxes`) | 4 | facade admission | includes descendants; released on close |
| per-isolate JS heap target | 64 MiB | V8 finite heap limit (`target - 4 MiB` before the near-heap-limit callback) | finite 4 MiB termination headroom; best-effort, not RSS |
| per-tree managed-memory budget (`memory_budget_bytes`) | 256 MiB | budget-accounted allocator + ancestor reservations | exhaustion may retire that isolate with a typed terminal error; siblings stay usable |
| shared `HostConfig` memory pool | 512 MiB | shared `Budget` | explicit smaller pools are never auto-expanded |
| host concurrency (`HostConfig.host_budget`) | 32 slots | shared `Budget` | per-sandbox `queue_depth` ≤ 32 |
| `max_pending_invocations` | 32 | facade queue | `queue_full` when saturated |
| `max_message_bytes` | 1 MiB | facade + backend | inputs, results, Worker messages |
| invocation deadline | 30 s default, 300 s max | facade + per-sandbox watchdog | terminal for the sandbox |
| log retention per invocation (`max_log_bytes`) | 64 KiB | backend per-invocation counter under the effective policy | follows live narrowing of the invocation's effective policy |
| schema budgets | depth 128, node/string/array/property budgets | `JsonSchema::validate` | Rust `HostConfig` only |
| audit buffer | bounded, dropped events counted | `HostAudit` | Rust `HostConfig` only |

`narrow_policy` narrows permissions, messages, queue, log and deadline for
subsequent checks. Live heap, tree-memory and sandbox-count changes are
unsupported (typed rejection); recreate the sandbox. Existing children retain
their creation ceilings, and root cancel revokes the subtree.

Each DNS query holds its own native-completion admission until native cleanup;
unrelated queries do not inherit it. Mounts use no-follow confinement.

No hard per-sandbox RSS limit, CPU quota or timeout rollback is implemented; V8
native/process OOM is not contained. See the [threat model](threat-model.md).
