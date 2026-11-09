# Host capability boundary

A registered Rust callback is trusted host code. The guest supplies only its
name and JSON arguments. `HostContext` supplies sandbox identity, the original
absolute deadline, cancellation, and the host-granted business scope; arguments
never replace that scope. The callback must enforce its business scope when
reading or changing application resources. Timeouts do not roll back side effects.

`HostRegistry::prepare_dispatch` clones the callback/context under the registry
lock. `Dispatch::run` (blocking) or `run_async` execute after that lock is released;
`accept_result` checks cancellation, deadline, closure and grant generation before
returning a result. Revoking then regranting the same scope invalidates older work.
Synchronous and async callbacks run on bounded native host threads, keeping isolate
and supervisor control loops free. Non-cooperative host work retains its context
and accounting until it actually ends; close must report outstanding work.

`HostConfig::schemas` maps registered names to `CapabilitySchemas { input, output }`.
Supported schemas cover null, booleans, integers/numbers, bounded strings/arrays,
and objects with required/optional properties and explicit additional-property
behavior. This is a documented subset, not a full JSON Schema implementation.
Absent entries use `JsonSchema::Any`, which still enforces recursive depth, node,
string, property-count and array-count budgets. Input validation runs before host
work; output validation runs before any result reaches the guest. Validation
errors include no payload values or dynamic paths.

Create the engine's root contexts using cloned `HostConfig` so they share its
`host_budget` admission pool. Create children with their parent's `host_config()`
to inherit all ancestors. Each in-flight callback reserves one unit in the local
and every ancestor pool. The lease is released by the actual native callback
completion, including after deadline or cancellation. Guests cannot supply or
replace this host-owned configuration.

`HostConfig::audit` is a bounded shared ring buffer. The host drains records with
`HostAudit::drain(max)` and checks `dropped()` for overwritten records. Records
contain time, trusted sandbox identity, registered capability name, and outcome.
They omit arguments, results, environment values, business scope and callback
error text. Unregistered guest names become the fixed `<ungranted>` tag. Audit
storage has a fixed record cap and never calls an application logging hook from
the supervisor path.

`Limits::heap_target_bytes` is the current isolate's own JS heap target (64 MiB
by default). `Limits::memory_budget_bytes` independently limits accounting for
its entire subtree (256 MiB by default). The engine reserves each isolate's heap
target from that subtree pool before allocating V8; managed VFS allocations use
the same pool. Children charge every ancestor. Four 64 MiB heap reservations
exhaust a 256 MiB pool, so external buffers require additional headroom and can
produce typed capacity rejection. These are controlled/reserved allocations,
not a process-RSS guarantee or a measured capacity claim.
