'use strict';
// Native parity for the facade multi-module/profile/VFS/resource surface.
// Every case runs real embedded V8 through the public addon; none is skipped.
const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {Engine, SandboxError} = require('..');

const MULTI = {
  'main.ts': `
    import { label } from './lib/label.ts';
    import { bump } from './lib/counter.ts';
    export async function run(input: {n: number}) {
      const lazy = await import('./lib/dynamic.ts');
      const state = bump(input.n);
      return {label: label(), lazy: lazy.value(), state};
    }`,
  'lib/label.ts': 'export function label(): string { return "multi-ok"; }',
  'lib/counter.ts': 'let count = 0; export function bump(n: number): number { count += n; return count; }',
  'lib/dynamic.ts': 'export function value(): number { return 7; }',
};

test('prepareModules runs static, dynamic and TypeScript module graphs', async () => {
  const engine = new Engine();
  try {
    const plugin = engine.prepareModules(MULTI, 'main.ts');
    assert.equal(plugin.entrypoint, 'main.ts');
    assert.equal(plugin.profile, 'javascript-only');
    assert.deepEqual([...plugin.modulePaths], ['lib/counter.ts', 'lib/dynamic.ts', 'lib/label.ts', 'main.ts']);
    const [a, b] = [await engine.create(plugin), await engine.create(plugin)];
    assert.deepEqual(await a.call('run', {n: 2}), {label: 'multi-ok', lazy: 7, state: 2});
    assert.deepEqual(await a.call('run', {n: 3}), {label: 'multi-ok', lazy: 7, state: 5});
    // Module state is per isolate.
    assert.deepEqual(await b.call('run', {n: 1}), {label: 'multi-ok', lazy: 7, state: 1});
    await a.close(); await b.close();
  } finally { assert.equal(await engine.shutdown(), true); }
});

test('prepareModules reports typed graph errors instead of rewriting the graph', () => {
  const engine = new Engine();
  try {
    assert.throws(() => engine.prepareModules({'main.js': "import './missing.js';"}, 'main.js'),
      e => e instanceof SandboxError && e.kind === 'module_not_found');
    assert.throws(() => engine.prepareModules({'main.js': "import 'left-pad';"}, 'main.js'),
      e => e instanceof SandboxError && e.kind === 'module_denied');
    assert.throws(() => engine.prepareModules({'main.js': "import 'node:fs';"}, 'main.js'),
      e => e instanceof SandboxError && e.kind === 'module_denied');
    assert.throws(() => engine.prepareModules({'main.js': 'export {};'}, 'other.js'),
      e => e instanceof SandboxError && e.kind === 'invalid_plugin');
    assert.throws(() => engine.prepareModules({'main.js': 'export {};'}, 'main.js', 'everything'),
      e => e instanceof RangeError && /invalid_configuration/.test(e.message));
    // The trusted worker_threads alias exists only under pure-js-npm.
    assert.throws(
      () => engine.prepareModules({'main.js': "import {Worker} from 'node:worker_threads'; export {Worker};"}, 'main.js'),
      e => e instanceof SandboxError && e.kind === 'module_denied');
    const pure = engine.prepareModules({'main.js': "import {Worker} from 'node:worker_threads'; export function kind(){ return typeof Worker; }"}, 'main.js', 'pure-js-npm');
    assert.equal(pure.profile, 'pure-js-npm');
  } finally { engine.shutdown(); }
});

test('guest Worker graphs run from the prepared module map only', async () => {
  const engine = new Engine();
  try {
    const plugin = engine.prepareModules({
      'main.js': `
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
        }`,
      // Child replies with a plain number: `postMessage(21)`.
      'worker.js': "self.onmessage = () => { self.postMessage(21); };",
    }, 'main.js');
    assert.deepEqual([...plugin.modulePaths], ['main.js', 'worker.js']);
    const sandbox = await engine.create(plugin);
    // The child answers the parent's postMessage with 21; the parent scales by n.
    assert.equal(await sandbox.call('run', 2), 42);
    await sandbox.close();
  } finally { assert.equal(await engine.shutdown(), true); }
});

test('private VFS read/write and env come from engine ceiling and sandbox request', async () => {
  const js = `export async function roundtrip(input) {
    await sandbox.fs.writeTextFile('/private/value', input.value);
    sandbox.env.set('PRIVATE', input.value);
    return {text: await sandbox.fs.readTextFile('/private/value'), env: sandbox.env.get('PRIVATE')};
  }
  export function inspect(){ return sandbox.env.get('PRIVATE'); }`;
  const engine = new Engine({hostCapabilities: ['env'], readRoots: ['/private'], writeRoots: ['/private']});
  try {
    // A sandbox request above the ceiling fails closed with a policy error.
    await assert.rejects(
      engine.create(engine.prepare(js), {readRoots: ['/elsewhere']}),
      e => e instanceof SandboxError && e.kind === 'policy');
    const allowed = await engine.create(engine.prepare(js), {
      capabilities: ['env'], readRoots: ['/private'], writeRoots: ['/private'],
    });
    const denied = await engine.create(engine.prepare(js));
    // A guest-supplied ID/value can never add authority.
    assert.deepEqual(await allowed.call('roundtrip', {value: 'a', sandboxId: 'sbx-forged'}), {text: 'a', env: 'a'});
    assert.equal(await allowed.call('inspect', null), 'a');
    // Separate sandbox: no shared file, env or cwd.
    assert.equal(await denied.call('inspect', null), null);
    await assert.rejects(denied.call('roundtrip', {value: 'b'}), SandboxError);
    await allowed.close(); await denied.close();
  } finally { assert.equal(await engine.shutdown(), true); }
});

test('mountDirectory attaches a trusted host directory within policy', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'denosanbox-node-mount-'));
  fs.writeFileSync(path.join(root, 'host.txt'), 'from-host');
  // Each export takes exactly one JSON argument from the host.
  const js = `export async function read(input){ return await sandbox.fs.readTextFile(input.path); }
    export async function write(input){ await sandbox.fs.writeTextFile(input.path, input.text); return await sandbox.fs.readTextFile(input.path); }`;
  const engine = new Engine({readRoots: ['/mount'], writeRoots: ['/mount']});
  try {
    const plugin = engine.prepare(js);
    const rw = await engine.create(plugin, {readRoots: ['/mount'], writeRoots: ['/mount']});
    const ro = await engine.create(plugin, {readRoots: ['/mount']});
    const unmounted = await engine.create(plugin, {readRoots: ['/mount'], writeRoots: ['/mount']});
    engine.mountDirectory(rw, '/mount', root, 'read-write');
    engine.mountDirectory(ro, '/mount', root, 'read');
    assert.throws(() => engine.mountDirectory(rw, '/mount', root, 'append'), RangeError);
    assert.equal(await rw.call('read', {path: '/mount/host.txt'}), 'from-host');
    assert.equal(await ro.call('read', {path: '/mount/host.txt'}), 'from-host');
    assert.equal(await rw.call('write', {path: '/mount/new.txt', text: 'inside'}), 'inside');
    assert.equal(fs.readFileSync(path.join(root, 'new.txt'), 'utf8'), 'inside');
    await assert.rejects(ro.call('write', {path: '/mount/host.txt', text: 'no'}), SandboxError);
    // A sandbox without the mount cannot reach the same host path.
    await assert.rejects(unmounted.call('read', {path: '/mount/host.txt'}), SandboxError);
    // Path traversal through the virtual prefix is rejected by the facade.
    await assert.rejects(rw.call('read', {path: '/mount/../etc/passwd'}), SandboxError);
    await rw.close(); await ro.close(); await unmounted.close();
  } finally {
    await engine.shutdown();
    fs.rmSync(root, {recursive: true, force: true});
  }
});

test('a plugin prepared by one engine cannot bypass another engine limits', async () => {
  // Regression: prepared graphs are immutable but must still pass through the
  // creating engine's own EngineLimits. Engine B exposes maxModuleBytes, so a
  // 29-byte module must be rejected with the exact typed limit error.
  const a = new Engine();
  const b = new Engine({maxModuleBytes: 1});
  try {
    const source = 'export function run(){return 1;}'; // 29 bytes
    const plugin = a.prepare(source);
    await assert.rejects(b.create(plugin), e => e instanceof SandboxError && e.kind === 'limit_exceeded');
    assert.equal(b.limits.maxModuleBytes, 1);
    const sandbox = await a.create(plugin);
    assert.equal(await sandbox.call('run', null), 1);
    await sandbox.close();
  } finally { await a.shutdown(); await b.shutdown(); }
});

test('engine hard limits are enforced by the facade', async () => {
  const k = new Engine({maxSandboxes: 1});
  try {
    const plugin = k.prepare('export function id(x){return x;}');
    const only = await k.create(plugin);
    assert.equal(k.activeSandboxes, 1);
    await assert.rejects(k.create(plugin), e => e instanceof SandboxError && e.kind === 'limit_exceeded');
    await only.close();
    assert.equal(k.activeSandboxes, 0);
  } finally { assert.equal(await k.shutdown(), true); }

  const m = new Engine({maxMessageBytes: 64});
  try {
    const sandbox = await m.create(m.prepare('export function echo(x){return x;}'));
    await assert.rejects(sandbox.call('echo', {x: 'y'.repeat(200)}), e => e instanceof SandboxError && e.kind === 'limit_exceeded');
    await sandbox.close();
  } finally { assert.equal(await m.shutdown(), true); }

  // Bounded external-budget probe (same-tree OOM is explicitly not contained;
  // never run a fatal/unbounded heap allocation). 9 MiB ArrayBuffer against a
  // 16 MiB own heap target and a 24 MiB sandbox-tree memory budget must fail
  // with a typed limit error and must not crash the host.
  const heap = new Engine({heapTargetBytes: 16 * 1024 * 1024, memoryBudgetBytes: 24 * 1024 * 1024});
  try {
    const plugin = heap.prepare(`export function bytesHeld(size) {
      const held = []; // roots the buffer until return
      const buffer = new ArrayBuffer(size);
      held.push(buffer);
      return buffer.byteLength;
    }`);
    const sandbox = await heap.create(plugin);
    await assert.rejects(sandbox.call('bytesHeld', 9 * 1024 * 1024), e => e instanceof SandboxError && e.kind === 'limit_exceeded');
    await sandbox.close();
  } finally { assert.equal(await heap.shutdown(), true); }
});

test('async host callback keeps Node loop responsive and Node PID', async () => {
  const engine = new Engine({hostCapabilities: ['pid', 'slow']});
  let ticks = 0;
  const timer = setInterval(() => ticks++, 5);
  engine.registerHost('pid', async () => process.pid);
  engine.registerHost('slow', async () => { await new Promise(r => setTimeout(r, 40)); return 'slow-done'; });
  try {
    const plugin = engine.prepare(`export async function run(){
      const pid = await sandbox.host.call('pid', null);
      return {pid, slow: await sandbox.host.call('slow', null)};
    }`);
    const sandbox = await engine.create(plugin, {hostGrants: {pid: 'process', slow: 'tenant-a'}});
    const result = await sandbox.call('run', null);
    assert.equal(result.pid, process.pid);
    assert.equal(result.slow, 'slow-done');
    assert.ok(ticks > 1, 'Node timers must advance while awaiting host callbacks');
    const report = await sandbox.close();
    assert.equal(report.closedCleanly, true);
    assert.equal(report.pendingHostOperations, 0);
  } finally {
    clearInterval(timer);
    assert.equal(await engine.shutdown(), true);
  }
});

test('close report stays truthful for non-cooperative callback and close is terminal', async () => {
  const engine = new Engine({hostCapabilities: ['stuck']});
  let release;
  engine.registerHost('stuck', () => new Promise(resolve => { release = resolve; }));
  try {
    const plugin = engine.prepare("export async function run(){ return await sandbox.host.call('stuck', null); }");
    const sandbox = await engine.create(plugin, {hostGrants: {stuck: 'tenant-a'}});
    await assert.rejects(sandbox.call('run', null, {timeoutMs: 60}),
      e => e instanceof SandboxError && ['deadline_exceeded', 'cancelled', 'closed'].includes(e.kind));
    // The callback ignores the AbortSignal and keeps running.
    const report = await sandbox.close();
    assert.equal(typeof report.closedCleanly, 'boolean');
    if (!report.closedCleanly) {
      assert.ok(report.pendingHostOperations >= 1, 'pending operations must be reported');
      const again = await sandbox.close();
      assert.equal(again.closedCleanly, false, 'an incomplete close must not be upgraded to success');
    }
    await assert.rejects(sandbox.call('run', null), e => e instanceof SandboxError && e.kind === 'closed');
    release?.('late');
  } finally { await engine.shutdown(); }
});

test('multiple engines in one process keep independent lifecycles', async () => {
  const one = new Engine();
  const two = new Engine();
  const js = 'let n=0; export function next(){ return ++n; }';
  const a = await one.create(one.prepare(js));
  const b = await two.create(two.prepare(js));
  assert.equal(await a.call('next', null), 1);
  assert.equal(await b.call('next', null), 1);
  assert.equal(await one.shutdown(), true);
  await assert.rejects(a.call('next', null), e => e instanceof SandboxError && e.kind === 'closed');
  assert.equal(await b.call('next', null), 2);
  assert.equal((await b.close()).closedCleanly, true);
  assert.equal(await two.shutdown(), true);
});
