from __future__ import annotations

from typing import Any, Final

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
    ) -> None: ...
    def prepare(
        self, source: str, entrypoint: str = "main.js"
    ) -> NativePreparedPlugin: ...
    def create(
        self,
        prepared: NativePreparedPlugin,
        *,
        default_timeout_ms: int | None = None,
        memory_limit_bytes: int | None = None,
    ) -> Any: ...
    def shutdown(self) -> Any: ...
    @property
    def backend(self) -> str: ...
    @property
    def active_sandboxes(self) -> int: ...

class NativePreparedPlugin:
    @property
    def entrypoint(self) -> str: ...
    @property
    def module_count(self) -> int: ...
    @property
    def total_bytes(self) -> int: ...

class NativeSandbox:
    def call(
        self,
        export: str,
        input_json: str,
        timeout_ms: int | None = None,
    ) -> Any: ...
    def cancel(self) -> None: ...
    def close(self) -> Any: ...

class CloseReport:
    closed_cleanly: bool
    forced_termination: bool
    invocations_completed: int
    invocations_cancelled: int

class ShutdownReport:
    sandboxes_closed: int
    sandboxes_forced: int
    clean: bool
