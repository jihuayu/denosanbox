'use strict';
const {test} = require('node:test');
const assert = require('node:assert/strict');
const {Engine, SandboxError} = require('..');
const source = `let count=0;
export function next(input) { return {count:++count,input}; }
export function loop() { while (true) {} }
export function hang() { return new Promise(()=>{}); }
export function fail() { throw new Error('fixture'); }`;
test('N-API and embedded V8 coexist across independent engines and lifecycle', async () => {
  const pid = process.pid;
  const first = new Engine();
  const second = new Engine();
  try {
    const [a,b] = await Promise.all([first.create(first.prepare(source)), second.create(second.prepare(source))]);
    assert.deepEqual(await a.call('next', 'A'), {count:1,input:'A'});
    assert.deepEqual(await b.call('next', 'B'), {count:1,input:'B'});
    assert.deepEqual(await a.call('next', null), {count:2,input:null});
    assert.equal(process.pid, pid);
    assert.equal(await a.close(), true);
    assert.equal(await first.shutdown(), true);
    assert.deepEqual(await b.call('next', null), {count:2,input:null});
    assert.equal(await b.close(), true);
    assert.equal(await b.close(), true);
  } finally { await first.shutdown(); await second.shutdown(); }
});
test('native execution leaves Node event loop responsive; deadlines isolate failure', async () => {
  const engine = new Engine();
  try {
    const [a,b] = await Promise.all([engine.create(engine.prepare(source)), engine.create(engine.prepare(source))]);
    let ticks=0;
    const timer=setInterval(()=>ticks++,5);
    try {
      await assert.rejects(a.call('loop', null, {timeoutMs:80}), error=>error instanceof SandboxError && error.kind==='deadline_exceeded');
      assert.ok(ticks > 1, 'Node timers must advance during embedded JS loop');
    } finally { clearInterval(timer); }
    assert.equal((await b.call('next', null)).count,1);
    await b.close();
  } finally { await engine.shutdown(); }
});
test('explicit cancellation and typed errors', async () => {
  const engine = new Engine();
  try {
    const sandbox = await engine.create(engine.prepare(source));
    await assert.rejects(sandbox.call('fail',null), error=>error.kind==='execution');
    const waiting = sandbox.call('loop',null,{timeoutMs:1000});
    setTimeout(()=>sandbox.cancel(),20);
    await assert.rejects(waiting,error=>['cancelled','closed'].includes(error.kind));
    await sandbox.close();
    await assert.rejects(sandbox.call('next',null), error=>error.kind==='closed');
  } finally { await engine.shutdown(); }
});
