"""Multi-module graphs, explicit profiles, private VFS/env, mounts and bounds.

All cases run the real embedded V8 backend through the installed native
extension; nothing here is skipped by design.
"""

from __future__ import annotations

import json
import tempfile
from pathlib import Path

import pytest

import denosanbox
from denosanbox import (
    ClosedError,
    Engine,
    EngineOptions,
    LimitExceededError,
    PolicyError,
    RuntimeUnavailableError,
    SandboxError,
    SandboxOptions,
)

pytestmark = pytest.mark.deno

MULTI = {
    "main.ts": """
        import { label } from './lib/label.ts';
        import { bump } from './lib/counter.ts';
        export async function run(input: {n: number}) {
          const lazy = await import('./lib/dynamic.ts');
          const state = bump(input.n);
          return {label: label(), lazy: lazy.value(), state};
        }""",
    "lib/label.ts": 'export function label(): string { return "multi-ok"; }',
    "lib/counter.ts": "let count = 0; export function bump(n: number): number { count += n; return count; }",
    "lib/dynamic.ts": "export function value(): number { return 7; }",
}


def _prepare_modules(engine: Engine, modules, entrypoint="main.ts", **kwargs):
    try:
        return engine.prepare_modules(modules, entrypoint, **kwargs)
    except RuntimeUnavailableError as exc:
        pytest.fail(
            "facade Engine::prepare_with_profile is not integrated in this checkpoint: "
            f"{exc}"
        )


async def test_prepare_modules_static_dynamic_and_typescript_graphs() -> None:
    engine = Engine()
    try:
        plugin = _prepare_modules(engine, MULTI)
        assert plugin.entrypoint == "main.ts"
        assert plugin.profile == "javascript-only"
        assert plugin.module_paths == (
            "lib/counter.ts",
            "lib/dynamic.ts",
            "lib/label.ts",
            "main.ts",
        )
        a = await engine.create(plugin)
        b = await engine.create(plugin)
        assert json.loads(await a.call("run", '{"n":2}')) == {"label": "multi-ok", "lazy": 7, "state": 2}
        assert json.loads(await a.call("run", '{"n":3}')) == {"label": "multi-ok", "lazy": 7, "state": 5}
        # Module state is per isolate.
        assert json.loads(await b.call("run", '{"n":1}')) == {"label": "multi-ok", "lazy": 7, "state": 1}
        await a.close()
        await b.close()
    finally:
        assert (await engine.shutdown()).clean


async def test_pure_js_npm_profile_is_explicit_and_never_js_only_fallback() -> None:
    engine = Engine()
    try:
        # The trusted worker_threads alias exists only under pure-js-npm.
        graph = {
            "main.js": "import {Worker} from 'node:worker_threads'; export function kind(){ return typeof Worker; }"
        }
        with pytest.raises(denosanbox.ModuleDeniedError):
            engine.prepare_modules(graph, "main.js", profile="javascript-only")
        pure = _prepare_modules(engine, graph, "main.js", profile="pure-js-npm")
        assert pure.profile == "pure-js-npm"
        sandbox = await engine.create(pure)
        assert await sandbox.call("kind", "null") == '"function"'
        await sandbox.close()
        with pytest.raises(denosanbox.InvalidConfigurationError):
            engine.prepare_modules(graph, "main.js", profile="everything")
    finally:
        assert (await engine.shutdown()).clean


async def test_prepare_modules_reports_typed_graph_errors() -> None:
    engine = Engine()
    with pytest.raises(denosanbox.ModuleNotFoundSandboxError):
        engine.prepare_modules({"main.js": "import './missing.js';"}, "main.js")
    with pytest.raises(denosanbox.ModuleDeniedError):
        engine.prepare_modules({"main.js": "import 'left-pad';"}, "main.js")
    with pytest.raises(denosanbox.ModuleDeniedError):
        engine.prepare_modules({"main.js": "import 'node:fs';"}, "main.js")
    with pytest.raises(denosanbox.InvalidPluginError):
        engine.prepare_modules({"main.js": "export {};"}, "other.js")
    with pytest.raises(TypeError):
        engine.prepare_modules(["main.js", "x"], "main.js")  # type: ignore[arg-type]
    await engine.shutdown()


async def test_guest_worker_graph_resolves_from_prepared_modules() -> None:
    engine = Engine()
    try:
        plugin = _prepare_modules(
            engine,
            {
                "main.js": """
                export async function run(n) {
                  // Supported minimal surface: string specifier, explicit module type.
                  const worker = new Worker('./worker.js', {type: 'module'});
                  return await new Promise((resolve, reject) => {
                    worker.addEventListener('error', (event) => reject(new Error(event.message)));
                    worker.addEventListener('message', async (event) => {
                      await worker.terminate();
                      resolve(event.data * n);
                    });
                    worker.postMessage('ping');
                  });
                }""",
                # Child replies with a plain number: postMessage(21).
                "worker.js": "self.onmessage = () => { self.postMessage(21); };",
            },
            "main.js",
        )
        assert plugin.module_paths == ("main.js", "worker.js")
        async with await engine.create(plugin) as sandbox:
            # The child answers the parent's postMessage with 21; scaled by n.
            assert await sandbox.call("run", "2") == "42"
    finally:
        assert (await engine.shutdown()).clean


VFS_JS = """
export async function roundtrip(input) {
  await sandbox.fs.writeTextFile('/private/value', input.value);
  sandbox.env.set('PRIVATE', input.value);
  return {text: await sandbox.fs.readTextFile('/private/value'), env: sandbox.env.get('PRIVATE')};
}
export function inspect() { return sandbox.env.get('PRIVATE'); }
"""


async def test_private_vfs_and_env_follow_ceiling_and_request() -> None:
    engine = Engine(
        options=EngineOptions(
            host_capabilities=["env"], read_roots=["/private"], write_roots=["/private"]
        )
    )
    try:
        prepared = engine.prepare(VFS_JS)
        with pytest.raises(PolicyError):
            await engine.create(prepared, options=SandboxOptions(read_roots=["/elsewhere"]))
        allowed = await engine.create(
            prepared,
            options=SandboxOptions(
                capabilities=["env"], read_roots=["/private"], write_roots=["/private"]
            ),
        )
        denied = await engine.create(prepared)
        # Guest-supplied identifiers/values add no authority.
        result = json.loads(await allowed.call("roundtrip", '{"value":"a","sandboxId":"sbx-forged"}'))
        assert result == {"text": "a", "env": "a"}
        assert await allowed.call("inspect", "null") == '"a"'
        assert await denied.call("inspect", "null") == "null"
        with pytest.raises(SandboxError):
            await denied.call("roundtrip", '{"value":"b"}')
        await allowed.close()
        await denied.close()
    finally:
        assert (await engine.shutdown()).clean


async def test_mount_directory_is_bounded_by_policy_and_read_only_mode() -> None:
    js = """
    export async function read(p){ return await sandbox.fs.readTextFile(p); }
    export async function write(input){ await sandbox.fs.writeTextFile(input.path, input.text); return await sandbox.fs.readTextFile(input.path); }
    """
    with tempfile.TemporaryDirectory(prefix="denosanbox-py-mount-") as root:
        Path(root, "host.txt").write_text("from-host")
        engine = Engine(options=EngineOptions(read_roots=["/mount"], write_roots=["/mount"]))
        try:
            plugin = engine.prepare(js)
            rw = await engine.create(
                plugin, options=SandboxOptions(read_roots=["/mount"], write_roots=["/mount"])
            )
            ro = await engine.create(plugin, options=SandboxOptions(read_roots=["/mount"]))
            unmounted = await engine.create(
                plugin, options=SandboxOptions(read_roots=["/mount"], write_roots=["/mount"])
            )
            engine.mount_directory(rw, "/mount", root, "read-write")
            engine.mount_directory(ro, "/mount", root, "read")
            with pytest.raises(denosanbox.InvalidConfigurationError):
                engine.mount_directory(rw, "/mount", root, "append")
            # Python passes one JSON document as text; the export takes one argument.
            assert await rw.call("read", '{"path":"/mount/host.txt"}') == '"from-host"'
            assert await ro.call("read", '{"path":"/mount/host.txt"}') == '"from-host"'
            assert await rw.call("write", '{"path":"/mount/new.txt","text":"inside"}') == '"inside"'
            assert Path(root, "new.txt").read_text() == "inside"
            with pytest.raises(SandboxError):
                await ro.call("write", '{"path":"/mount/host.txt","text":"no"}')
            # No mount on this sandbox: the same host path is unreachable.
            with pytest.raises(SandboxError):
                await unmounted.call("read", '{"path":"/mount/host.txt"}')
            # Traversal through the virtual prefix is rejected.
            with pytest.raises(SandboxError):
                await rw.call("read", '{"path":"/mount/../etc/passwd"}')
            await rw.close()
            await ro.close()
            closed = await unmounted.close()
            assert closed.closed_cleanly
            with pytest.raises(ClosedError):
                engine.mount_directory(unmounted, "/mount", root)
        finally:
            assert (await engine.shutdown()).clean


async def test_engine_hard_limits_are_enforced_by_facade() -> None:
    limited = Engine(max_active_sandboxes=1)
    try:
        plugin = limited.prepare("export function id(x){return x;}")
        only = await limited.create(plugin)
        assert limited.active_sandboxes == 1
        with pytest.raises(LimitExceededError):
            await limited.create(plugin)
        await only.close()
        assert limited.active_sandboxes == 0
        limits = limited.limits
        assert limits.max_active_sandboxes == 1
        assert isinstance(limits.max_graph_bytes, int) and limits.max_graph_bytes > 0
    finally:
        assert (await limited.shutdown()).clean

    messages = Engine(max_message_bytes=64)
    try:
        sandbox = await messages.create(messages.prepare("export function echo(x){return x;}"))
        with pytest.raises(LimitExceededError):
            await sandbox.call("echo", json.dumps({"x": "y" * 200}))
        await sandbox.close()
    finally:
        assert (await messages.shutdown()).clean


async def test_zero_timeout_is_rejected_without_closing_sandbox() -> None:
    engine = Engine()
    try:
        sandbox = await engine.create(engine.prepare("export function ok(){return 1;}"))
        with pytest.raises(denosanbox.InvalidConfigurationError):
            await sandbox.call("ok", "null", timeout_ms=0)
        assert await sandbox.call("ok", "null") == "1"
        await sandbox.close()
    finally:
        assert (await engine.shutdown()).clean


def test_cross_engine_prepared_plugin_rejection_is_typed() -> None:
    """Regression: a PreparedPlugin prepared by engine A must not bypass engine
    B's own `EngineLimits`; the facade `create` path must reject it with the
    exact typed `limit_exceeded` error. The same plugin still works on A.
    """

    import asyncio

    async def scenario() -> None:
        tiny = Engine("fake")
        source = "export function run(){return 1;}"  # 29 bytes
        broad = Engine("fake", max_module_bytes=1)
        assert broad.limits.max_module_bytes == 1
        prepared = tiny.prepare(source)
        try:
            with pytest.raises(LimitExceededError) as caught:
                await broad.create(prepared)
            assert caught.value.kind == "limit_exceeded"
            async with await tiny.create(prepared) as inside:
                assert await inside.call("run", "null") == "1"
        finally:
            await tiny.shutdown()
            await broad.shutdown()

    asyncio.run(scenario())
