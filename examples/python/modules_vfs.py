#!/usr/bin/env python3
"""Multi-module graph, private VFS/mount and a real async host callback.

Run after installing the binding from ``bindings/python``:

    python examples/python/modules_vfs.py
"""

from __future__ import annotations

import asyncio
import json
import os
import tempfile
from pathlib import Path

from denosanbox import Engine, EngineOptions, SandboxOptions

MODULES = {
    "main.ts": """
        import { greeting } from './lib/greeting.ts';

        export async function store(input: {value: string}) {
          await sandbox.fs.writeTextFile('/private/value', input.value);
          sandbox.env.set('PRIVATE', input.value);
          return {greeting: greeting(input.value), text: await sandbox.fs.readTextFile('/private/value'),
                  env: sandbox.env.get('PRIVATE')};
        }
        export async function hostPid() { return await sandbox.host.call('pid', null); }
        export async function readMounted(input: {path: string}) { return await sandbox.fs.readTextFile(input.path); }
    """,
    "lib/greeting.ts": 'export function greeting(value: string): string { return `hello ${value}`; }',
}


async def main() -> None:
    with tempfile.TemporaryDirectory(prefix="denosanbox-example-") as root:
        Path(root, "tenant.txt").write_text("mounted tenant file")

        engine = Engine(
            options=EngineOptions(
                host_capabilities=["env", "pid"],
                read_roots=["/private", "/mount"],
                write_roots=["/private"],
                memory_budget_bytes=128 * 1024 * 1024,
            )
        )

        async def pid(context, input_json: str) -> str:
            await asyncio.sleep(0.001)
            return json.dumps({"pid": os.getpid(), "scope": context.scope})

        engine.register_host("pid", pid)

        try:
            plugin = engine.prepare_modules(MODULES, "main.ts")
            print("modules:", plugin.module_paths, "profile:", plugin.profile)

            sandbox = await engine.create(
                plugin,
                options=SandboxOptions(
                    capabilities=["env", "pid"],
                    read_roots=["/private", "/mount"],
                    write_roots=["/private"],
                    host_grants={"pid": "tenant-a"},
                ),
            )
            # Trusted host values only; the guest never chooses these.
            engine.mount_directory(sandbox, "/mount", root, "read")
            print("store:", json.loads(await sandbox.call("store", '{"value":"alice"}')))
            print("hostPid:", json.loads(await sandbox.call("hostPid", "null")))
            print("mounted:", json.loads(await sandbox.call("readMounted", '{"path":"/mount/tenant.txt"}')))
            print("close:", await sandbox.close())
        finally:
            print("shutdown:", await engine.shutdown())


if __name__ == "__main__":
    asyncio.run(main())
