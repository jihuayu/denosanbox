'use strict';
const {test} = require('node:test');
const assert = require('node:assert/strict');
const cases = require('../../../fixtures/behavior/v1.json');
const {Engine, SandboxError} = require('..');
for (const fixture of cases) {
  test(`shared behavior: ${fixture.id}`, async () => {
    const engine = new Engine();
    try {
      // One-module graph through the multi-module API (same facade path).
      const plugin = engine.prepareModules({[fixture.entrypoint]: fixture.source}, fixture.entrypoint);
      const sandboxes = [await engine.create(plugin), await engine.create(plugin)];
      for (const step of fixture.steps) {
        const call = sandboxes[step.sandbox].call(step.export, step.input);
        if (step.expected.kind === 'ok') assert.deepEqual(await call, step.expected.value);
        else await assert.rejects(call, e => e instanceof SandboxError && e.kind === step.expected.code);
      }
    } finally { await engine.shutdown(); }
  });
}
