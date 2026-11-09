"""Native async Python bindings for the denosanbox JavaScript sandbox facade.

The extension module is :mod:`denosanbox._native`. This package is the
supported public surface: it translates the internal native exception into a
stable :class:`SandboxError` hierarchy and adds deterministic async context
manager helpers.

Only a single process-local sandbox runtime is used. There is no sidecar and
no subprocess. Async host callbacks use the running Python event loop and require explicit
host capability ceilings and per-sandbox grants.
"""

from __future__ import annotations

import asyncio
import inspect
from typing import Any, Final

from . import _native

__version__: Final[str] = "0.1.0"

__all__ = [
    "BackendKind",
    "CancelledError",
    "CloseReport",
    "ClosedError",
    "DeadlineExceededError",
    "Engine",
    "ExecutionError",
    "InvalidConfigurationError",
    "InvalidPluginError",
    "InvocationContext",
    "LimitExceededError",
    "ModuleDeniedError",
    "ModuleNotFoundSandboxError",
    "PreparedPlugin",
    "QueueFullError",
    "RuntimeUnavailableError",
    "Sandbox",
    "SandboxError",
    "ShutdownReport",
    "UnsupportedFeatureError",
    "UnsupportedLanguageError",
    "BackendError",
    "InternalError",
    "PolicyError",
]


# ---------------------------------------------------------------------------
# Stable error hierarchy.
#
# The numeric/string category comes from Rust's ``Error::kind()`` and is
# mirrored on every public exception as both ``kind`` and ``code``.
# ---------------------------------------------------------------------------


class SandboxError(Exception):
    """Base class for every error raised by the public binding.

    ``kind`` and ``code`` are stable machine-readable strings and both contain
    the same value returned by the Rust facade's ``Error::kind()``. ``message``
    is the human-readable rendering.
    """

    kind: str = "sandbox_error"
    code: str = "sandbox_error"

    def __init__(self, message: str = "", *, kind: str | None = None) -> None:
        super().__init__(message)
        self.message = message
        if kind is not None:
            self.kind = kind
            self.code = kind


class RuntimeUnavailableError(SandboxError):
    kind = "runtime_unavailable"
    code = "runtime_unavailable"


class UnsupportedFeatureError(SandboxError):
    kind = "unsupported_feature"
    code = "unsupported_feature"


class UnsupportedLanguageError(SandboxError):
    kind = "unsupported_language"
    code = "unsupported_language"


class InvalidPluginError(SandboxError):
    kind = "invalid_plugin"
    code = "invalid_plugin"


class ModuleNotFoundSandboxError(SandboxError):
    kind = "module_not_found"
    code = "module_not_found"


class ModuleDeniedError(SandboxError):
    kind = "module_denied"
    code = "module_denied"


class LimitExceededError(SandboxError):
    kind = "limit_exceeded"
    code = "limit_exceeded"


class QueueFullError(SandboxError):
    kind = "queue_full"
    code = "queue_full"


class ClosedError(SandboxError):
    kind = "closed"
    code = "closed"


class CancelledError(SandboxError):
    kind = "cancelled"
    code = "cancelled"


class DeadlineExceededError(SandboxError):
    kind = "deadline_exceeded"
    code = "deadline_exceeded"


class ExecutionError(SandboxError):
    kind = "execution"
    code = "execution"


class BackendError(SandboxError):
    kind = "backend"
    code = "backend"


class InvalidConfigurationError(SandboxError):
    kind = "invalid_configuration"
    code = "invalid_configuration"


class PolicyError(SandboxError):
    kind = "policy"
    code = "policy"


class InternalError(SandboxError):
    kind = "internal"
    code = "internal"


_KIND_TO_ERROR: dict[str, type[SandboxError]] = {
    cls.kind: cls
    for cls in (
        RuntimeUnavailableError,
        UnsupportedFeatureError,
        UnsupportedLanguageError,
        InvalidPluginError,
        ModuleNotFoundSandboxError,
        ModuleDeniedError,
        LimitExceededError,
        QueueFullError,
        ClosedError,
        CancelledError,
        DeadlineExceededError,
        ExecutionError,
        BackendError,
        InvalidConfigurationError,
        InternalError,
        PolicyError,
    )
}


def _translate_native_error(exc: BaseException) -> SandboxError:
    """Converts the private native exception into a public stable error."""

    kind = getattr(exc, "kind", "sandbox_error")
    if not isinstance(kind, str) or not kind:
        kind = "sandbox_error"
    error_type = _KIND_TO_ERROR.get(kind, SandboxError)
    return error_type(str(exc), kind=kind)


async def _await_native(awaitable: Any) -> Any:
    try:
        return await awaitable
    except _native.NativeError as exc:  # pragma: no cover - exercised by tests
        raise _translate_native_error(exc) from None


# Report value objects are owned by the extension. They expose read-only
# attributes and are the actual types returned by ``close``/``shutdown``.
InvocationContext = _native.InvocationContext
CloseReport = _native.CloseReport
ShutdownReport = _native.ShutdownReport

BackendKind = str
"""Backend names accepted by :class:`Engine`: ``"deno"`` or ``"fake"``."""


class PreparedPlugin:
    """A validated, immutable JavaScript module graph.

    Instances are created by :meth:`Engine.prepare` and may be reused to create
    multiple independent sandboxes.
    """

    __slots__ = ("_native",)

    def __init__(self, native: _native.NativePreparedPlugin) -> None:
        self._native = native

    @property
    def entrypoint(self) -> str:
        return self._native.entrypoint

    @property
    def module_count(self) -> int:
        return self._native.module_count

    @property
    def total_bytes(self) -> int:
        return self._native.total_bytes

    def __repr__(self) -> str:
        return (
            f"PreparedPlugin(entrypoint={self.entrypoint!r}, "
            f"module_count={self.module_count}, total_bytes={self.total_bytes})"
        )


class Sandbox:
    """An independent JavaScript sandbox with its own isolate and module state.

    Create instances with ``await engine.create(prepared)``. Use ``await
    sandbox.close()`` for deterministic worker join and reporting. The async
    context manager also closes the sandbox on exit.
    """

    __slots__ = ("_native", "_closed", "_close_report")

    def __init__(self, native: _native.NativeSandbox) -> None:
        self._native = native
        self._closed = False
        self._close_report: CloseReport | None = None

    async def call(
        self,
        export: str,
        input_json: str,
        timeout_ms: int | None = None,
    ) -> str:
        """Calls ``export(input)`` and returns the JSON-encoded result string.

        ``input_json`` must be a JSON document. ``timeout_ms`` overrides the
        engine/sandbox default timeout for this call. If the Python task is
        cancelled while awaiting, the sandbox is cancelled terminally.
        """

        if self._closed:
            raise ClosedError("sandbox is closed")
        if not isinstance(input_json, str):
            raise TypeError("input_json must be a str containing JSON")
        try:
            return await self._native.call(export, input_json, timeout_ms)
        except asyncio.CancelledError:
            # The native call future also has a drop guard. This explicit call
            # covers cancellation before the future is polled to completion and
            # makes the cancellation contract deterministic for Python users.
            self._native.cancel()
            self._closed = True
            raise
        except _native.NativeError as exc:
            if getattr(exc, "kind", "") in {"closed", "cancelled", "deadline_exceeded"}:
                self._closed = True
            raise _translate_native_error(exc) from None

    def cancel(self) -> None:
        """Terminally cancels the current invocation and future calls.

        Cancellation is not a temporary pause: the core facade marks the
        sandbox closed. Use :meth:`close` afterwards to join and obtain the
        report.
        """

        self._closed = True
        self._native.cancel()

    async def close(self) -> CloseReport:
        """Requests close, joins the worker and returns a close report.

        Calls are idempotent: subsequent calls return the first report.
        """

        if self._close_report is not None:
            return self._close_report
        self._closed = True
        try:
            report = await _await_native(self._native.close())
        except asyncio.CancelledError:
            self._native.cancel()
            raise
        self._close_report = report
        return report

    @property
    def closed(self) -> bool:
        return self._closed

    async def __aenter__(self) -> "Sandbox":
        return self

    async def __aexit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        report = await self.close()
        if not report.closed_cleanly:
            error = SandboxError("sandbox close is incomplete; host work still owns resources", kind="incomplete_close")
            error.report = report
            raise error

    def __repr__(self) -> str:
        state = "closed" if self._closed else "open"
        return f"Sandbox(state={state})"


class Engine:
    """Engine owning a backend factory and a bounded set of sandboxes.

    :param backend: ``"deno"`` (production default) or ``"fake"`` (contract
        tests only).
    :param max_active_sandboxes: optional engine limit override.
    :param max_pending_invocations: optional engine limit override.
    :param max_message_bytes: optional JSON message size limit.
    :param default_invocation_timeout_ms: default per-call timeout.
    :param max_invocation_timeout_ms: hard per-call timeout ceiling.
    """

    __slots__ = ("_native",)

    def __init__(
        self,
        backend: BackendKind = "deno",
        *,
        max_active_sandboxes: int | None = None,
        max_pending_invocations: int | None = None,
        max_message_bytes: int | None = None,
        default_invocation_timeout_ms: int | None = None,
        max_invocation_timeout_ms: int | None = None,
        max_modules: int | None = None,
        max_module_bytes: int | None = None,
        max_graph_bytes: int | None = None,
        host_capabilities: tuple[str, ...] | list[str] = (),
    ) -> None:
        try:
            self._native = _native.NativeEngine(
                backend,
                max_active_sandboxes=max_active_sandboxes,
                max_pending_invocations=max_pending_invocations,
                max_message_bytes=max_message_bytes,
                default_invocation_timeout_ms=default_invocation_timeout_ms,
                max_invocation_timeout_ms=max_invocation_timeout_ms,
                max_modules=max_modules,
                max_module_bytes=max_module_bytes,
                max_graph_bytes=max_graph_bytes,
                host_capabilities=list(host_capabilities),
            )
        except _native.NativeError as exc:
            raise _translate_native_error(exc) from None

    def prepare(self, source: str, entrypoint: str = "main.js") -> PreparedPlugin:
        """Validates and freezes a single-module JavaScript plugin.

        JavaScript ESM and TypeScript (``.ts``/``.mts``) use the core offline parser/transpiler.
        This synchronous operation releases the GIL while Rust validates the
        graph.
        """

        try:
            native = self._native.prepare(source, entrypoint)
        except _native.NativeError as exc:
            raise _translate_native_error(exc) from None
        return PreparedPlugin(native)

    async def create(
        self,
        prepared: PreparedPlugin,
        *,
        default_timeout_ms: int | None = None,
        memory_limit_bytes: int | None = None,
        host_grants: dict[str, str] | None = None,
    ) -> Sandbox:
        """Creates a sandbox with an independent isolate and module instances."""

        if not isinstance(prepared, PreparedPlugin):
            raise TypeError("prepared must be a PreparedPlugin from Engine.prepare")
        native = await _await_native(
            self._native.create(
                prepared._native,
                default_timeout_ms=default_timeout_ms,
                memory_limit_bytes=memory_limit_bytes,
                host_grants=list((host_grants or {}).items()),
            )
        )
        return Sandbox(native)

    def register_host(self, name: str, callback: Any, *, input_limit: int = 1048576, output_limit: int = 1048576) -> None:
        """Register an async callback on the current running asyncio loop.

        The callback receives trusted context and JSON text, and returns JSON
        text. Registration does not grant any sandbox permission to call it.
        """
        asyncio.get_running_loop()
        if not inspect.iscoroutinefunction(callback):
            raise TypeError("host callback must be an async function")

        async def invoke(context: InvocationContext, input_json: str) -> str:
            task = asyncio.create_task(callback(context, input_json))
            try:
                while True:
                    done, _ = await asyncio.wait({task}, timeout=0.005)
                    if done:
                        return task.result()
                    if context.cancelled or context.remaining_ms is None:
                        task.cancel()
                        await task
            finally:
                if not task.done():
                    task.cancel()

        try:
            self._native.register_host(name, invoke, input_limit, output_limit)
        except _native.NativeError as exc:
            raise _translate_native_error(exc) from None

    async def shutdown(self) -> ShutdownReport:
        """Closes every live sandbox owned by this engine."""

        return await _await_native(self._native.shutdown())

    @property
    def backend(self) -> str:
        return self._native.backend

    @property
    def active_sandboxes(self) -> int:
        return self._native.active_sandboxes

    async def __aenter__(self) -> "Engine":
        return self

    async def __aexit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        report = await self.shutdown()
        if not report.clean:
            error = SandboxError("engine shutdown is incomplete; host work still owns resources", kind="incomplete_close")
            error.report = report
            raise error

    def __repr__(self) -> str:
        return (
            f"Engine(backend={self.backend!r}, "
            f"active_sandboxes={self.active_sandboxes})"
        )
