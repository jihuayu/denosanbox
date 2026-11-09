#!/usr/bin/env python3
"""Create two independent JavaScript sandboxes and call them from Python.

Run after installing the binding from ``bindings/python``:

    python examples/python/main.py
"""

from __future__ import annotations

import asyncio
import json

from denosanbox import Engine

JS = """
let counter = 0;

export async function echo(input) {
  return input;
}

export function next() {
  counter += 1;
  return counter;
}
"""


async def main() -> None:
    async with Engine() as engine:
        prepared = engine.prepare(JS)
        async with await engine.create(prepared) as a:
            async with await engine.create(prepared) as b:
                print("echo:", json.loads(await a.call("echo", '{"message": "hi"}')))
                print("a.next:", json.loads(await a.call("next", "null")))
                print("b.next:", json.loads(await b.call("next", "null")))
                print("a.next:", json.loads(await a.call("next", "null")))

                report_a = await a.close()
                report_b = await b.close()
                print("close a:", report_a)
                print("close b:", report_b)


if __name__ == "__main__":
    asyncio.run(main())
