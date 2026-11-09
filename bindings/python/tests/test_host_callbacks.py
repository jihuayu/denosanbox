"""Real native callback dispatch, identity and cancellation on asyncio."""
import asyncio
import gc
import json
import os
import threading
import weakref

import pytest

from denosanbox import Engine, SandboxError

pytestmark = pytest.mark.deno
JS = "export async function run(input) { return await sandbox.host.call('orders.read', input); }"


async def test_async_callback_runs_on_python_loop_with_trusted_scope_and_pid():
    engine = Engine(host_capabilities=['orders.read'])
    loop_thread = threading.get_ident()

    async def read_order(context, input_json):
        await asyncio.sleep(0.01)
        assert threading.get_ident() == loop_thread
        return json.dumps({'scope': context.scope, 'sandbox': context.sandbox_id,
                           'pid': os.getpid(), 'input': json.loads(input_json)})

    engine.register_host('orders.read', read_order)
    prepared = engine.prepare(JS)
    try:
        allowed = await engine.create(prepared, host_grants={'orders.read': 'tenant-a'})
        denied = await engine.create(prepared)
        output = json.loads(await allowed.call('run', '{"scope":"forged-b"}'))
        assert output['scope'] == 'tenant-a'
        assert output['pid'] == os.getpid()
        assert output['sandbox'].startswith('sbx-')
        assert output['input']['scope'] == 'forged-b'
        with pytest.raises(SandboxError):
            await denied.call('run', 'null')
        await allowed.close()
        await denied.close()
    finally:
        assert (await engine.shutdown()).clean


async def test_python_task_cancellation_cancels_host_coroutine():
    engine = Engine(host_capabilities=['orders.read'])
    started = asyncio.Event()
    cancelled = asyncio.Event()

    async def slow(context, input_json):
        started.set()
        try:
            await asyncio.sleep(30)
            return 'null'
        finally:
            cancelled.set()

    engine.register_host('orders.read', slow)
    sandbox = await engine.create(engine.prepare(JS), host_grants={'orders.read': 'tenant-a'})
    try:
        invocation = asyncio.create_task(sandbox.call('run', 'null'))
        await asyncio.wait_for(started.wait(), 1)
        invocation.cancel()
        with pytest.raises(asyncio.CancelledError):
            await invocation
        await asyncio.wait_for(cancelled.wait(), 1)
        await asyncio.sleep(0.02)  # callback worker releases its accounting after the coroutine
        assert (await sandbox.close()).closed_cleanly
    finally:
        await engine.shutdown()


async def test_callback_failure_does_not_expose_python_exception_text():
    engine = Engine(host_capabilities=['orders.read'])

    async def failing(context, input_json):
        raise ValueError('private-host-secret')

    engine.register_host('orders.read', failing)
    sandbox = await engine.create(engine.prepare(JS), host_grants={'orders.read': 'tenant-a'})
    try:
        with pytest.raises(SandboxError) as error:
            await sandbox.call('run', 'null')
        assert 'private-host-secret' not in str(error.value)
        await sandbox.close()
    finally:
        await engine.shutdown()


def test_explicit_callback_close_allows_interpreter_shutdown():
    import subprocess
    import sys
    script = r'''
import asyncio, json, os
from denosanbox import Engine
async def main():
    async with Engine(host_capabilities=['pid']) as engine:
        async def pid(context, input_json):
            await asyncio.sleep(0.001)
            return json.dumps(os.getpid())
        engine.register_host('pid', pid)
        plugin=engine.prepare("export async function run() { return await sandbox.host.call('pid',null); }")
        async with await engine.create(plugin, host_grants={'pid':'process'}) as sandbox:
            assert json.loads(await sandbox.call('run','null')) == os.getpid()
asyncio.run(main())
print('shutdown-ok')
'''
    completed = subprocess.run([sys.executable, '-c', script], capture_output=True, text=True, timeout=10)
    assert completed.returncode == 0, completed.stderr
    assert 'shutdown-ok' in completed.stdout


async def test_shutdown_releases_registered_callback_while_engine_wrapper_lives():
    engine = Engine(host_capabilities=['callback.read'])

    async def callback(context, input_json):
        return 'null'

    callback_ref = weakref.ref(callback)
    engine.register_host('callback.read', callback)
    del callback

    report = await engine.shutdown()
    assert report.clean
    for _ in range(4):
        gc.collect()
        await asyncio.sleep(0)
        if callback_ref() is None:
            break

    # Keep the public wrapper alive while checking that shutdown retired its
    # callback registry and released the Python function it owned.
    assert engine.backend == 'deno'
    assert callback_ref() is None


def _make_engine_callback_cycle():
    engine = Engine(host_capabilities=['callback.read'])

    async def callback(context, input_json):
        # Exercise the hidden Rust -> Python callback -> Engine reference cycle.
        _ = engine.backend
        return 'null'

    callback_ref = weakref.ref(callback)
    engine.register_host('callback.read', callback)
    return engine, callback_ref


async def test_shutdown_breaks_callback_cycle_after_engine_wrapper_drop():
    engine, callback_ref = _make_engine_callback_cycle()
    report = await engine.shutdown()
    assert report.clean
    del engine

    for _ in range(4):
        gc.collect()
        await asyncio.sleep(0)
        if callback_ref() is None:
            break

    assert callback_ref() is None
