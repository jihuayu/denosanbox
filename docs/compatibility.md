# Compatibility and API matrix

This matrix records what the current code in this repository actually provides.
"Validated" means executed by an in-repository test or example on the stated
platform; anything else is explicitly not claimed. The authoritative data model
and lifecycle rules are in [API](api.md); each binding has its own README.

## Host runtimes and bindings

| Product | Integration | Built/validated against | Platform tested | Status |
| --- | --- | --- | --- | --- |
| Rust facade (`denosanbox`) | direct `Engine` API | Rust 1.99.1, `deno_core = 0.413.0`, `deno_ast 0.53.3` | Linux x86_64 | real in-process Deno/V8 backend is the default feature |
| `denosanbox` with `--no-default-features` | facade + fake backend | Rust 1.99.1 | Linux x86_64 | lifecycle contract only; no JavaScript |
| Python | PyO3 via maturin | CPython 3.12, matching core checkout | Linux x86_64 | independently versioned 0.1.0; no published wheel |
| Node.js | napi-rs | Node 24.22.1 | Linux x86_64 | independently versioned 0.1.0; loader fails closed on other versions/platforms |
| C | explicit `deno_abi_version() == 1` | Rust 1.99.1 | Linux x86_64 | experimental polling subset; no callback API |
| Go | cgo over the C ABI | Go 1.27.3 | Linux x86_64 | optional; polling completion |

No other OS, architecture, Python, Node or Go version is claimed. No package is
published to any registry by this repository or its CI.

## Core API compatibility

- The Rust facade's data model is frozen at `API_VERSION = "0.1.0"`; bindings
  consume that facade directly and version themselves independently.
- The backend is chosen per engine at construction (`EngineConfig.backend`,
  default `BackendKind::Deno`). There is no `selectedEngine` / runtime-swap API
  and **no live sandbox migration** between backends; build a new `Engine`.
  Calling an unavailable backend fails with a typed `runtime_unavailable`
  error instead of an implicit fallback.
- Host capability protocol version is `1`; `call_versioned` rejects other
  versions. Guest↔host frames use `DSB1` framing with bounded payloads.
- Guest module specifiers are never handed to an external loader; only graph
  modules are loaded.

## Guest language and module profile

| Feature | Supported | Notes |
| --- | --- | --- |
| ESM `.js` / `.mjs` | yes | entrypoint and relative imports inside the prepared graph only |
| TypeScript `.ts` / `.mts` | yes | transpiled, not type-checked |
| JSX / TSX, CommonJS, JSON modules, WASM | no | rejected at preparation |
| npm packages | vendored, host-prepared only | digest-verified pure-JS packages via `denosanbox::modules`; no package manager |
| Dynamic `import()` | graph-only | computed specifiers cannot leave the graph |
| `fetch` | only when policy grants a matching host/port (and private grant for non-global IPs) | all DNS answers and redirects re-checked; byte and deadline limits |
| `WebAssembly`, `SharedArrayBuffer`, `Atomics` | removed | never re-exposed |
| Workers | host-created `WorkerHandle` (registered child, depth ≤ 4) | guest-created managed JSON Workers are forthcoming in the runtime job |
| Native addons, subprocess, signals, inspector, FFI | never | no API surface |

## Limits actually enforced today

| Limit | Enforced by | Notes |
| --- | --- | --- |
| `max_active_sandboxes` (engine, default 4) | facade admission | released on close |
| `max_pending_invocations` (default 32) | facade queue | `queue_full` when saturated |
| `max_message_bytes` (default 1 MiB) | facade + backend | inputs and results |
| invocation timeout (`default_*` 30 s, `max_*` 300 s) | facade + per-sandbox watchdog | terminal for the sandbox |
| JS heap target (`heap_target_bytes`, default 64 MiB; min 8 MiB on Deno) | V8 near-heap-limit callback + engine check | accounting target, not RSS |
| subtree memory budget (`memory_budget_bytes`, default 256 MiB) | budget-accounted ArrayBuffer allocator + ancestor reservations | `limit_exceeded`; siblings unaffected; **pending fix:** over-budget reports `js_heap_target` and closes that sandbox |
| log retention per invocation (64 KiB) | backend op counter | **hardcoded**; narrowed `max_log_bytes` policy pending runtime fix |
| host concurrency (`queue_depth`, ≤ 32 per sandbox) | `HostContext` + ancestor budget | control operations bypass admission |
| schema budgets | `JsonSchema::validate` | depth clamped at 128, node/string/array/property budgets |
| audit buffer | `HostAudit` | bounded, dropped events counted |

No hard per-sandbox RSS limit, CPU quota or timeout rollback is implemented.
See the [threat model](threat-model.md) for the shared-process caveats.
