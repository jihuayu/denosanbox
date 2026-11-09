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
from typing import Any, Final, Mapping, Sequence

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
    "SandboxOptions",
    "EngineOptions",
    "EngineLimits",
    "MountAccess",
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

MountAccess = str
"""Mount access modes accepted by :meth:`Engine.mount_directory`."""

EngineLimits = _native.EngineLimits
"""Read-only engine hard limits as configured (see :attr:`Engine.limits`)."""


class EngineOptions:
    """Trusted host configuration for the engine's policy ceiling.

    An attribute left as ``None`` keeps the facade default. The facade
    immutable ceiling is deny-by-default: the binding builds the ceiling only
    from these host-supplied values and narrows it through the facade, which
    rejects malformed values instead of widening anything.

    ``host_capabilities`` are facade capability names (for example ``"env"``
    or a registered callback name such as ``"orders.read"``). They authorize,
    they never reach the guest by themselves. ``read_roots``/``write_roots`` are
    virtual paths. Resource limits map one-to-one onto the facade policy
    ``Limits``; ``memory_budget_bytes`` is the aggregate managed-memory budget
    for a sandbox *and its descendants* while ``heap_target_bytes`` is the JS
    heap target of a single isolate, not an RSS ceiling.
    """

    __slots__ = (
        "host_capabilities",
        "read_roots",
        "write_roots",
        "heap_target_bytes",
        "memory_budget_bytes",
        "max_sandboxes",
        "max_message_bytes",
        "deadline_ms",
        "queue_depth",
        "max_log_bytes",
    )

    def __init__(
        self,
        *,
        host_capabilities: Sequence[str] = (),
        read_roots: Sequence[str] = (),
        write_roots: Sequence[str] = (),
        heap_target_bytes: int | None = None,
        memory_budget_bytes: int | None = None,
        max_sandboxes: int | None = None,
        max_message_bytes: int | None = None,
        deadline_ms: int | None = None,
        queue_depth: int | None = None,
        max_log_bytes: int | None = None,
    ) -> None:
        self.host_capabilities = tuple(host_capabilities)
        self.read_roots = tuple(read_roots)
        self.write_roots = tuple(write_roots)
        self.heap_target_bytes = heap_target_bytes
        self.memory_budget_bytes = memory_budget_bytes
        self.max_sandboxes = max_sandboxes
        self.max_message_bytes = max_message_bytes
        self.deadline_ms = deadline_ms
        self.queue_depth = queue_depth
        self.max_log_bytes = max_log_bytes

    def __repr__(self) -> str:
        fields = ", ".join(f"{name}={getattr(self, name)!r}" for name in self.__slots__)
        return f"EngineOptions({fields})"


class SandboxOptions:
    """Trusted host request narrowed against the engine ceiling.

    This is what the host asks for; it becomes the guest's effective policy
    only after the facade intersects it with the immutable engine ceiling.
    Guest JavaScript can never supply or edit these values.

    ``capabilities`` and ``read_roots``/``write_roots`` must be covered by the
    ceiling built from :class:`EngineOptions`; ``deadline_ms`` is the
    per-invocation policy deadline, ``queue_depth`` the pending-call limit and
    ``max_message_bytes``/``max_log_bytes`` the per-message and log bounds.
    ``default_timeout_ms`` and ``memory_limit_bytes`` map to the facade
    ``SandboxOptions`` fields. ``host_grants`` maps a registered callback name
    to a trusted business scope and implicitly requests that capability.
    """

    __slots__ = (
        "default_timeout_ms",
        "memory_limit_bytes",
        "capabilities",
        "read_roots",
        "write_roots",
        "deadline_ms",
        "queue_depth",
        "max_message_bytes",
        "max_log_bytes",
        "host_grants",
    )

    def __init__(
        self,
        *,
        default_timeout_ms: int | None = None,
        memory_limit_bytes: int | None = None,
        capabilities: Sequence[str] = (),
        read_roots: Sequence[str] = (),
        write_roots: Sequence[str] = (),
        deadline_ms: int | None = None,
        queue_depth: int | None = None,
        max_message_bytes: int | None = None,
        max_log_bytes: int | None = None,
        host_grants: Mapping[str, str] | None = None,
    ) -> None:
        self.default_timeout_ms = default_timeout_ms
        self.memory_limit_bytes = memory_limit_bytes
        self.capabilities = tuple(capabilities)
        self.read_roots = tuple(read_roots)
        self.write_roots = tuple(write_roots)
        self.deadline_ms = deadline_ms
        self.queue_depth = queue_depth
        self.max_message_bytes = max_message_bytes
        self.max_log_bytes = max_log_bytes
        self.host_grants = dict(host_grants or {})

    def __repr__(self) -> str:
        fields = ", ".join(f"{name}={getattr(self, name)!r}" for name in self.__slots__)
        return f"SandboxOptions({fields})"


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

    @property
    def profile(self) -> str:
        """Explicit facade compatibility profile: ``javascript-only`` or ``pure-js-npm``."""
        return self._native.profile

    @property
    def module_paths(self) -> tuple[str, ...]:
        """Sorted root-relative module paths in the immutable graph."""
        return tuple(self._native.module_paths)

    def __repr__(self) -> str:
        return (
            f"PreparedPlugin(entrypoint={self.entrypoint!r}, "
            f"module_count={self.module_count}, total_bytes={self.total_bytes}, "
            f"profile={self.profile!r})"
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

        The report is the facade's own accounting: ``closed_cleanly`` is false
        while host callback work still owns resources, and
        ``pending_host_operations`` states how many operations remain. A
        not-clean report is cached and returned again by later ``close()``
        calls instead of being presented as a final clean result; a clean
        report is cached and returned as-is.
        """

        if self._close_report is not None and self._close_report.closed_cleanly:
            return self._close_report
        self._closed = True
        try:
            report = await _await_native(self._native.close())
        except asyncio.CancelledError:
            self._native.cancel()
            raise
        if report.closed_cleanly:
            self._close_report = report
        return report

    @property
    def closed(self) -> bool:
        """True after ``close()``, ``cancel()`` or a terminal call failure."""
        return self._closed

    @property
    def id(self) -> str:
        """Host-minted sandbox identity (never guest-controlled)."""
        return self._native.id

    def __repr__(self) -> str:
        state = "closed" if self._closed else "open"
        return f"Sandbox(id={self.id!r}, state={state})"

    async def __aenter__(self) -> "Sandbox":
        return self

    async def __aexit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        report = await self.close()
        if not report.closed_cleanly:
            error = SandboxError("sandbox close is incomplete; host work still owns resources", kind="incomplete_close")
            error.report = report
            raise error


class Engine:
    """Engine owning a backend factory and a bounded set of sandboxes.

    :param backend: ``"deno"`` (production default) or ``"fake"`` (contract
        tests only).
    :param options: :class:`EngineOptions` trusted host configuration that sets
        the immutable, deny-by-default policy ceiling.
    :param max_active_sandboxes: optional engine limit override.
    :param max_pending_invocations: optional engine limit override.
    :param max_message_bytes: optional JSON message size limit.
    :param default_invocation_timeout_ms: default per-call timeout.
    :param max_invocation_timeout_ms: hard per-call timeout ceiling.
    :param host_capabilities: shortcut for ``options.host_capabilities``.

    Passing both ``options`` and ``host_capabilities`` is rejected so the
    ceiling has exactly one source.
    """

    __slots__ = ("_native",)

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
        host_capabilities: tuple[str, ...] | list[str] | None = None,
    ) -> None:
        if options is not None and host_capabilities is not None:
            raise TypeError("pass either options or host_capabilities, not both")
        if options is None and host_capabilities is not None:
            options = EngineOptions(host_capabilities=host_capabilities)
        if options is not None and not isinstance(options, EngineOptions):
            raise TypeError("options must be an EngineOptions instance")
        options = options or EngineOptions()
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
                host_capabilities=list(options.host_capabilities),
                host_read_roots=list(options.read_roots),
                host_write_roots=list(options.write_roots),
                host_heap_target_bytes=options.heap_target_bytes,
                host_memory_budget_bytes=options.memory_budget_bytes,
                host_max_sandboxes=options.max_sandboxes,
                host_max_message_bytes=options.max_message_bytes,
                host_deadline_ms=options.deadline_ms,
                host_queue_depth=options.queue_depth,
                host_max_log_bytes=options.max_log_bytes,
            )
        except _native.NativeError as exc:
            raise _translate_native_error(exc) from None

    def prepare(self, source: str, entrypoint: str = "main.js") -> PreparedPlugin:
        """Validates and freezes a single-module ``javascript-only`` plugin.

        JavaScript ESM and TypeScript (``.ts``/``.mts``) use the core offline parser/transpiler.
        This synchronous operation releases the GIL while Rust validates the
        graph.
        """

        try:
            native = self._native.prepare(source, entrypoint)
        except _native.NativeError as exc:
            raise _translate_native_error(exc) from None
        return PreparedPlugin(native)

    def prepare_modules(
        self,
        modules: Mapping[str, str],
        entrypoint: str = "main.js",
        *,
        profile: str | None = None,
    ) -> PreparedPlugin:
        """Validates and freezes a multi-module graph.

        ``modules`` maps root-relative paths (``.js``, ``.mjs``, ``.ts``,
        ``.mts``) to host-supplied source text; ``entrypoint`` must be one of
        its keys. Static imports, re-exports, computed dynamic imports and a
        guest ``new Worker()`` graph resolve only against this immutable map.
        TypeScript modules are transpiled offline.

        ``profile`` is explicit: ``None``/``"javascript-only"`` prepares the
        facade JavaScript profile, ``"pure-js-npm"`` additionally authorizes a
        host-prepared vendored package graph and the trusted
        ``worker_threads`` alias. No profile grants VFS, environment, network
        or callback privileges; those come from the engine/sandbox policy.

        Raises :class:`InvalidConfigurationError` for an unknown profile name
        or non-mapping input and translated :class:`SandboxError` subtypes for
        invalid graphs.
        """

        if not isinstance(modules, Mapping):
            raise TypeError("modules must be a mapping of module path to source text")
        if profile is not None and not isinstance(profile, str):
            raise TypeError("profile must be None or a string")
        pairs: list[tuple[str, str]] = []
        for path, text in modules.items():
            if not isinstance(path, str) or not isinstance(text, str):
                raise TypeError("module paths and sources must be str")
            pairs.append((path, text))
        if not isinstance(entrypoint, str):
            raise TypeError("entrypoint must be a str")
        try:
            native = self._native.prepare_modules(pairs, entrypoint, profile)
        except _native.NativeError as exc:
            raise _translate_native_error(exc) from None
        return PreparedPlugin(native)

    async def create(
        self,
        prepared: PreparedPlugin,
        *,
        options: SandboxOptions | None = None,
        default_timeout_ms: int | None = None,
        memory_limit_bytes: int | None = None,
        host_grants: Mapping[str, str] | None = None,
    ) -> Sandbox:
        """Creates a sandbox with an independent isolate and module instances.

        ``options`` is the trusted host request; it is intersected with the
        immutable engine ceiling by the facade, so a request above the ceiling
        raises :class:`PolicyError`. ``default_timeout_ms``, ``memory_limit_bytes``
        and ``host_grants`` are shortcuts for the matching ``options`` fields
        and may not be combined with ``options``.
        """

        if not isinstance(prepared, PreparedPlugin):
            raise TypeError("prepared must be a PreparedPlugin from Engine.prepare")
        if options is not None:
            if not isinstance(options, SandboxOptions):
                raise TypeError("options must be a SandboxOptions instance")
            if default_timeout_ms is not None or memory_limit_bytes is not None or host_grants is not None:
                raise TypeError(
                    "pass either options or default_timeout_ms/memory_limit_bytes/host_grants, not both"
                )
        else:
            options = SandboxOptions(
                default_timeout_ms=default_timeout_ms,
                memory_limit_bytes=memory_limit_bytes,
                host_grants=host_grants,
            )
        native = await _await_native(
            self._native.create(
                prepared._native,
                default_timeout_ms=options.default_timeout_ms,
                memory_limit_bytes=options.memory_limit_bytes,
                host_grants=list(options.host_grants.items()),
                capabilities=list(options.capabilities),
                read_roots=list(options.read_roots),
                write_roots=list(options.write_roots),
                deadline_ms=options.deadline_ms,
                queue_depth=options.queue_depth,
                max_message_bytes=options.max_message_bytes,
                max_log_bytes=options.max_log_bytes,
            )
        )
        return Sandbox(native)

    def mount_directory(
        self,
        sandbox: Sandbox,
        prefix: str,
        path: str,
        access: MountAccess = "read",
    ) -> None:
        """Attaches a trusted host directory to an existing sandbox.

        All arguments are trusted host values; guest input must never reach
        them. The Rust facade retains the directory capability, rejects
        symlinks, and still requires the sandbox policy to allow(read/write) the
        virtual ``prefix``. ``access`` is ``"read"`` (read-only) or
        ``"read-write"``. The sandbox must not be closed.
        """

        if not isinstance(sandbox, Sandbox):
            raise TypeError("sandbox must be a Sandbox created by this package")
        if sandbox.closed:
            raise ClosedError("sandbox is closed")
        try:
            self._native.mount_directory(sandbox._native, prefix, path, access)
        except _native.NativeError as exc:
            raise _translate_native_error(exc) from None

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

    @property
    def limits(self) -> EngineLimits:
        """Engine hard limits as configured at construction."""
        return self._native.limits

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
