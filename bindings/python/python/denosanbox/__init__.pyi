from __future__ import annotations

from typing import Any, Final, Callable, Awaitable, Mapping, Sequence

__version__: Final[str]

BackendKind = str
MountAccess = str

class SandboxError(Exception):
    kind: str
    code: str
    message: str
    def __init__(self, message: str = "", *, kind: str | None = None) -> None: ...

class RuntimeUnavailableError(SandboxError): ...
class UnsupportedFeatureError(SandboxError): ...
class UnsupportedLanguageError(SandboxError): ...
class InvalidPluginError(SandboxError): ...
class ModuleNotFoundSandboxError(SandboxError): ...
class ModuleDeniedError(SandboxError): ...
class LimitExceededError(SandboxError): ...
class QueueFullError(SandboxError): ...
class ClosedError(SandboxError): ...
class CancelledError(SandboxError): ...
class DeadlineExceededError(SandboxError): ...
class ExecutionError(SandboxError): ...
class BackendError(SandboxError): ...
class InvalidConfigurationError(SandboxError): ...
class InternalError(SandboxError): ...
class PolicyError(SandboxError): ...

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

class EngineLimits:
    max_active_sandboxes: int
    max_pending_invocations: int
    max_message_bytes: int
    default_invocation_timeout_ms: int
    max_invocation_timeout_ms: int
    max_modules: int
    max_module_bytes: int
    max_graph_bytes: int

class EngineOptions:
    host_capabilities: tuple[str, ...]
    read_roots: tuple[str, ...]
    write_roots: tuple[str, ...]
    heap_target_bytes: int | None
    memory_budget_bytes: int | None
    max_sandboxes: int | None
    max_message_bytes: int | None
    deadline_ms: int | None
    queue_depth: int | None
    max_log_bytes: int | None
    def __init__(
        self,
        *,
        host_capabilities: Sequence[str] = ...,
        read_roots: Sequence[str] = ...,
        write_roots: Sequence[str] = ...,
        heap_target_bytes: int | None = ...,
        memory_budget_bytes: int | None = ...,
        max_sandboxes: int | None = ...,
        max_message_bytes: int | None = ...,
        deadline_ms: int | None = ...,
        queue_depth: int | None = ...,
        max_log_bytes: int | None = ...,
    ) -> None: ...

class SandboxOptions:
    default_timeout_ms: int | None
    memory_limit_bytes: int | None
    capabilities: tuple[str, ...]
    read_roots: tuple[str, ...]
    write_roots: tuple[str, ...]
    deadline_ms: int | None
    queue_depth: int | None
    max_message_bytes: int | None
    max_log_bytes: int | None
    host_grants: dict[str, str]
    def __init__(
        self,
        *,
        default_timeout_ms: int | None = ...,
        memory_limit_bytes: int | None = ...,
        capabilities: Sequence[str] = ...,
        read_roots: Sequence[str] = ...,
        write_roots: Sequence[str] = ...,
        deadline_ms: int | None = ...,
        queue_depth: int | None = ...,
        max_message_bytes: int | None = ...,
        max_log_bytes: int | None = ...,
        host_grants: Mapping[str, str] | None = ...,
    ) -> None: ...

class ShutdownReport:
    sandboxes_closed: int
    sandboxes_forced: int
    clean: bool

class PreparedPlugin:
    def __init__(self, native: Any) -> None: ...
    @property
    def entrypoint(self) -> str: ...
    @property
    def module_count(self) -> int: ...
    @property
    def total_bytes(self) -> int: ...
    @property
    def profile(self) -> str: ...
    @property
    def module_paths(self) -> tuple[str, ...]: ...

class Sandbox:
    def __init__(self, native: Any) -> None: ...
    async def call(
        self,
        export: str,
        input_json: str,
        timeout_ms: int | None = None,
    ) -> str: ...
    def cancel(self) -> None: ...
    async def close(self) -> CloseReport: ...
    @property
    def closed(self) -> bool: ...
    @property
    def id(self) -> str: ...
    async def __aenter__(self) -> Sandbox: ...
    async def __aexit__(self, exc_type: Any, exc: Any, tb: Any) -> None: ...

class Engine:
    def __init__(
        self,
        backend: BackendKind = "deno",
        *,
        options: EngineOptions | None = None,
        max_active_sandboxes: int | None = None,
        max_pending_invocations: int | None = None,
        max_message_bytes: int | None = None,
        default_invocation_timeout_ms: int | None = None,
        max_invocation_timeout_ms: int | None = None,
        max_modules: int | None = None,
        max_module_bytes: int | None = None,
        max_graph_bytes: int | None = None,
        host_capabilities: list[str] | tuple[str, ...] | None = None,
    ) -> None: ...
    def prepare(self, source: str, entrypoint: str = "main.js") -> PreparedPlugin: ...
    def prepare_modules(
        self,
        modules: Mapping[str, str],
        entrypoint: str = "main.js",
        *,
        profile: str | None = None,
    ) -> PreparedPlugin: ...
    async def create(
        self,
        prepared: PreparedPlugin,
        *,
        options: SandboxOptions | None = None,
        default_timeout_ms: int | None = None,
        memory_limit_bytes: int | None = None,
        host_grants: Mapping[str, str] | None = None,
    ) -> Sandbox: ...
    def mount_directory(
        self,
        sandbox: Sandbox,
        prefix: str,
        path: str,
        access: MountAccess = "read",
    ) -> None: ...
    def register_host(self, name: str, callback: Callable[[InvocationContext, str], Awaitable[str]], *, input_limit: int = 1048576, output_limit: int = 1048576) -> None: ...
    async def shutdown(self) -> ShutdownReport: ...
    @property
    def backend(self) -> str: ...
    @property
    def active_sandboxes(self) -> int: ...
    @property
    def limits(self) -> EngineLimits: ...
    async def __aenter__(self) -> Engine: ...
    async def __aexit__(self, exc_type: Any, exc: Any, tb: Any) -> None: ...
