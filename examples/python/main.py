#!/usr/bin/env python3
"""Create two independent JavaScript sandboxes and call them from Python.

Run after installing the binding from ``bindings/python``:

    python examples/python/main.py
"""

from __future__ import annotations

import asyncio
import json

from denosanbox import Engine

JS = {
    "main.ts": """
        import { twice } from './lib/math.ts';
        let counter = 0;
        export async function echo(input) { return input; }
        export function next(): number { counter += 1; return counter; }
        export function double(n: number): number { return twice(n); }
    """,
    "lib/math.ts": "export function twice(n: number): number { return n * 2; }",
}


async def main() -> None:
    async with Engine() as engine:
        prepared = engine.prepare_modules(JS, "main.ts")
        print("modules:", prepared.module_paths, "profile:", prepared.profile)
        async with await engine.create(prepared) as a:
            async with await engine.create(prepared) as b:
                print("echo:", json.loads(await a.call("echo", '{"message": "hi"}')))
                print("double:", json.loads(await a.call("double", "21")))
                print("a.next:", json.loads(await a.call("next", "null")))
                print("b.next:", json.loads(await b.call("next", "null")))
                print("a.next:", json.loads(await a.call("next", "null")))

                report_a = await a.close()
                report_b = await b.close()
                print("close a:", report_a)
                print("close b:", report_b)


if __name__ == "__main__":
    asyncio.run(main())
