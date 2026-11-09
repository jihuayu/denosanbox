from __future__ import annotations

from typing import Any, Final, Callable, Awaitable, Sequence

__version__: Final[str]

class NativeError(Exception):
    kind: str
    code: str

class NativeEngine:
    def __init__(
        self,
        backend: str = "deno",
        *,
        max_active_sandboxes: int | None = None,
        max_pending_invocations: int | None = None,
        max_message_bytes: int | None = None,
        default_invocation_timeout_ms: int | None = None,
        max_invocation_timeout_ms: int | None = None,
        max_modules: int | None = None,
        max_module_bytes: int | None = None,
        max_graph_bytes: int | None = None,
        host_capabilities: list[str] | None = None,
        host_read_roots: list[str] | None = None,
        host_write_roots: list[str] | None = None,
        host_heap_target_bytes: int | None = None,
        host_memory_budget_bytes: int | None = None,
        host_max_sandboxes: int | None = None,
        host_max_message_bytes: int | None = None,
        host_deadline_ms: int | None = None,
        host_queue_depth: int | None = None,
        host_max_log_bytes: int | None = None,
    ) -> None: ...
    def prepare(
        self, source: str, entrypoint: str = "main.js"
    ) -> NativePreparedPlugin: ...
    def prepare_modules(
        self,
        modules: list[tuple[str, str]],
        entrypoint: str = "main.js",
        profile: str | None = None,
    ) -> NativePreparedPlugin: ...
    def create(
        self,
        prepared: NativePreparedPlugin,
        *,
        default_timeout_ms: int | None = None,
        memory_limit_bytes: int | None = None,
        host_grants: list[tuple[str, str]] | None = None,
        capabilities: list[str] | None = None,
        read_roots: list[str] | None = None,
        write_roots: list[str] | None = None,
        deadline_ms: int | None = None,
        queue_depth: int | None = None,
        max_message_bytes: int | None = None,
        max_log_bytes: int | None = None,
    ) -> Awaitable[NativeSandbox]: ...
    def register_host(self, name: str, callback: Any, input_limit: int = 1048576, output_limit: int = 1048576) -> None: ...
    def mount_directory(self, sandbox: NativeSandbox, prefix: str, path: str, access: str = "read") -> None: ...
    def shutdown(self) -> Awaitable[ShutdownReport]: ...
    @property
    def backend(self) -> str: ...
    @property
    def active_sandboxes(self) -> int: ...
    @property
    def limits(self) -> EngineLimits: ...

class NativePreparedPlugin:
    @property
    def entrypoint(self) -> str: ...
    @property
    def module_count(self) -> int: ...
    @property
    def total_bytes(self) -> int: ...
    @property
    def profile(self) -> str: ...
    @property
    @property
    def module_paths(self) -> list[str]: ...

class NativeSandbox:
    def call(
        self,
        export: str,
        input_json: str,
        timeout_ms: int | None = None,
    ) -> Awaitable[str]: ...
    def cancel(self) -> None: ...
    def close(self) -> Awaitable[CloseReport]: ...
    @property
    def id(self) -> str: ...

class InvocationContext:
    sandbox_id: str
    scope: str
    cancelled: bool
    remaining_ms: int | None

class CloseReport:
    closed_cleanly: bool
    forced_termination: bool
    invocations_completed: int
    invocations_cancelled: int
    pending_host_operations: int

class ShutdownReport:
    sandboxes_closed: int
    sandboxes_forced: int
    clean: bool

class EngineLimits:
    max_active_sandboxes: int
    max_pending_invocations: int
    max_message_bytes: int
    default_invocation_timeout_ms: int
    max_invocation_timeout_ms: int
    max_modules: int
    max_module_bytes: int
    max_graph_bytes: int
