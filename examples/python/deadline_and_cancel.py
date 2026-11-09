#!/usr/bin/env python3
"""Deadline and cancellation example.

Run after installing the binding from ``bindings/python``:

    python examples/python/deadline_and_cancel.py
"""

from __future__ import annotations

import asyncio

from denosanbox import DeadlineExceededError, Engine

JS = """
export function spin() {
  while (true) {}
}
"""


async def main() -> None:
    async with Engine() as engine:
        prepared = engine.prepare(JS)

        runaway = await engine.create(prepared)
        try:
            await runaway.call("spin", "null", timeout_ms=150)
        except DeadlineExceededError as exc:
            print(f"deadline hit: kind={exc.kind!r} code={exc.code!r}")
        report = await runaway.close()
        print("after deadline close:", report)

        cancelled = await engine.create(prepared)
        task = asyncio.create_task(
            cancelled.call("spin", "null", timeout_ms=60_000)
        )
        await asyncio.sleep(0.1)
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            print("Python task cancelled; sandbox cancellation requested")
        print("after cancel close:", await cancelled.close())


if __name__ == "__main__":
    asyncio.run(main())
