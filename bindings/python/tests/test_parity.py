"""The same native behavior contract as Rust and Node (no Deno executable)."""
import json
from pathlib import Path
import pytest
from denosanbox import Engine, SandboxError
CASES = json.loads((Path(__file__).resolve().parents[3] / 'fixtures/behavior/v1.json').read_text())

@pytest.mark.parametrize('case', CASES, ids=lambda c: c['id'])
@pytest.mark.asyncio
async def test_shared_behavior(case):
    engine = Engine()
    try:
        plugin = engine.prepare(case['source'], case['entrypoint'])
        sandboxes = [await engine.create(plugin), await engine.create(plugin)]
        for step in case['steps']:
            sandbox = sandboxes[step['sandbox']]
            if step['expected']['kind'] == 'ok':
                assert json.loads(await sandbox.call(step['export'], json.dumps(step['input'], ensure_ascii=False))) == step['expected']['value']
            else:
                with pytest.raises(SandboxError) as caught:
                    await sandbox.call(step['export'], json.dumps(step['input'], ensure_ascii=False))
                assert caught.value.kind == step['expected']['code']
    finally:
        await engine.shutdown()
