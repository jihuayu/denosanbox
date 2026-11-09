"""Real JavaScript integration tests for the default Deno/V8 backend.

These tests require the Deno backend and fail when it is unavailable. Run only
test_fake_contract.py when explicitly building without the production backend.
"""

from __future__ import annotations

import asyncio
import json

import pytest

import denosanbox
from denosanbox import (
    ClosedError,
    DeadlineExceededError,
    Engine,
)

pytestmark = pytest.mark.deno

JS = r"""
const state = { counter: 0 };

export async function echo(input) {
  return input;
}

export function next() {
  state.counter += 1;
  return state.counter;
}

export function spin() {
  while (true) {}
}
"""


def _engine_or_skip() -> Engine:
    try:
        return Engine()
    except RuntimeUnavailableError as exc:
        pytest.skip(f"Deno backend not compiled into this extension: {exc}")


async def _create(engine: Engine):
    prepared = engine.prepare(JS)
    return await engine.create(prepared)


async def test_two_independent_js_sandboxes() -> None:
    engine = _engine_or_skip()
    assert engine.backend == "deno"
    prepared = engine.prepare(JS)
    a = await engine.create(prepared)
    b = await engine.create(prepared)

    # Module-level JavaScript state must be independent between isolates.
    assert json.loads(await a.call("next", "null")) == 1
    assert json.loads(await b.call("next", "null")) == 1
    assert json.loads(await a.call("next", "null")) == 2
    assert json.loads(await b.call("next", "null")) == 2

    payload = {"text": "hello", "n": [1, 2, 3]}
    assert json.loads(await a.call("echo", json.dumps(payload))) == payload
    assert json.loads(await b.call("echo", json.dumps(payload))) == payload

    report_a = await a.close()
    report_b = await b.close()
    assert report_a.closed_cleanly is True
    assert report_b.closed_cleanly is True
    assert report_a.invocations_completed >= 3
    assert report_b.invocations_completed >= 3

    shutdown = await engine.shutdown()
    assert shutdown.clean is True


async def test_infinite_js_loop_hits_deadline_and_close_reports() -> None:
    engine = _engine_or_skip()
    prepared = engine.prepare(JS)
    runaway = await engine.create(prepared)
    healthy = await engine.create(prepared)

    with pytest.raises(DeadlineExceededError) as caught:
        await runaway.call("spin", "null", timeout_ms=150)
    assert caught.value.kind == "deadline_exceeded"
    assert caught.value.code == "deadline_exceeded"

    # Terminating one isolate must not affect the other sandbox.
    assert json.loads(await healthy.call("next", "null")) == 1

    report = await runaway.close()
    assert isinstance(report, denosanbox.CloseReport)
    assert isinstance(report.closed_cleanly, bool)
    assert isinstance(report.forced_termination, bool)
    with pytest.raises(ClosedError):
        await runaway.call("echo", "null")

    await healthy.close()
    shutdown = await engine.shutdown()
    assert shutdown.clean is True


async def test_js_call_cancelled_when_python_task_is_cancelled() -> None:
    engine = _engine_or_skip()
    sandbox = await _create(engine)

    task = asyncio.create_task(sandbox.call("spin", "null", timeout_ms=60_000))
    await asyncio.sleep(0.1)
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task

    assert sandbox.closed is True
    with pytest.raises(ClosedError):
        await sandbox.call("echo", "null")

    report = await sandbox.close()
    assert isinstance(report, denosanbox.CloseReport)
    await engine.shutdown()


async def test_js_async_context_manager() -> None:
    engine = _engine_or_skip()
    prepared = engine.prepare(JS)
    async with await engine.create(prepared) as sandbox:
        value = json.loads(await sandbox.call("echo", '{"ok": true}'))
        assert value == {"ok": True}
    assert sandbox.closed is True
    await engine.shutdown()


async def test_cancelled_creation_releases_initializing_sandbox() -> None:
    engine = Engine(default_invocation_timeout_ms=150, max_invocation_timeout_ms=300)
    prepared = engine.prepare("while (true) {} export {};")
    task = asyncio.create_task(engine.create(prepared))
    await asyncio.sleep(0.02)
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    for _ in range(100):
        if engine.active_sandboxes == 0:
            break
        await asyncio.sleep(0.01)
    assert engine.active_sandboxes == 0
    assert (await engine.shutdown()).clean


async def test_single_module_typescript_is_prepared_offline() -> None:
    async with Engine() as engine:
        plugin = engine.prepare("export function run(input: number): number { return input + 1; }", "main.ts")
        async with await engine.create(plugin) as sandbox:
            assert await sandbox.call("run", "41") == "42"
