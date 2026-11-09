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
    assert.equal((await a.close()).closedCleanly, true);
    assert.equal(await first.shutdown(), true);
    assert.deepEqual(await b.call('next', null), {count:2,input:null});
    assert.equal((await b.close()).closedCleanly, true);
    assert.equal((await b.close()).closedCleanly, true);
  } finally { await first.shutdown(); await second.shutdown(); }
});
test('Engine.prepare single-source plugins create and run alongside prepareModules', async () => {
  // Regression: a single-source PreparedPlugin must create through the same
  // native graph API as prepareModules; raw source is not a module map.
  const engine = new Engine();
  try {
    const single = engine.prepare('export function id(x){return x;}');
    assert.deepEqual([...single.modulePaths], ['main.js']);
    assert.equal(single.entrypoint, 'main.js');
    assert.equal(single.profile, 'javascript-only');
    const nested = engine.prepare('export function ok(){return 7;}', 'lib/entry.js');
    assert.deepEqual([...nested.modulePaths], ['lib/entry.js']);
    assert.equal(nested.entrypoint, 'lib/entry.js');
    const multi = engine.prepareModules({'main.js': "import {v} from './v.js'; export function get(){ return v; }", 'v.js': 'export const v = 3;'}, 'main.js');
    const [a, b, c] = await Promise.all([engine.create(single), engine.create(nested), engine.create(multi)]);
    assert.deepEqual(await a.call('id', {x: 1}), {x: 1});
    assert.equal(await b.call('ok', null), 7);
    assert.equal(await c.call('get', null), 3);
    for (const sandbox of [a, b, c]) assert.equal((await sandbox.close()).closedCleanly, true);
    // The same prepared plugin can create a second independent sandbox.
    const again = await engine.create(single);
    assert.deepEqual(await again.call('id', 'again'), 'again');
    await again.close();
    // Native validation still runs first for invalid single sources.
    assert.throws(() => engine.prepare("import 'left-pad'; export {};"), e => e instanceof SandboxError && e.kind === 'module_denied');
    assert.throws(() => engine.prepare('export {};', '/abs.js'), e => e instanceof SandboxError);
  } finally { assert.equal(await engine.shutdown(), true); }
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

test('async host callback uses Node event loop, same PID and trusted scope', async () => {
  const engine = new Engine({hostCapabilities:['orders.read']});
  engine.registerHost('orders.read', async (context,input) => {
    await new Promise(resolve=>setTimeout(resolve,10));
    return {pid:process.pid,scope:context.scope,sandboxId:context.sandboxId,input};
  });
  const plugin=engine.prepare("export async function run(input) { return await sandbox.host.call('orders.read',input); }");
  try {
    const allowed=await engine.create(plugin,{hostGrants:{'orders.read':'tenant-a'}});
    const denied=await engine.create(plugin);
    const result=await allowed.call('run',{scope:'forged-b'});
    assert.equal(result.pid,process.pid);
    assert.equal(result.scope,'tenant-a');
    assert.match(result.sandboxId,/^sbx-/);
    assert.equal(result.input.scope,'forged-b');
    await assert.rejects(denied.call('run',null),SandboxError);
    await allowed.close(); await denied.close();
  } finally { assert.equal(await engine.shutdown(),true); }
});

test('host callback observes sandbox cancellation through AbortSignal', async () => {
  const engine = new Engine({hostCapabilities:['orders.read']});
  let started;
  let aborted=false;
  const ready=new Promise(resolve=>{started=resolve;});
  engine.registerHost('orders.read',(context)=>new Promise((resolve,reject)=>{
    started();
    context.signal.addEventListener('abort',()=>{aborted=true;reject(new Error('private-host-secret'));},{once:true});
  }));
  const plugin=engine.prepare("export async function run(input) { return await sandbox.host.call('orders.read',input); }");
  const sandbox=await engine.create(plugin,{hostGrants:{'orders.read':'tenant-a'}});
  try {
    const invocation=sandbox.call('run',null);
    await ready;
    sandbox.cancel();
    await assert.rejects(invocation,error=>!error.message.includes('private-host-secret'));
    assert.equal(aborted,true);
    await new Promise(resolve=>setTimeout(resolve,20));
    assert.equal((await sandbox.close()).closedCleanly,true);
  } finally { await engine.shutdown(); }
});

test('explicit native callback shutdown permits Node process exit', () => {
  const {spawnSync}=require('node:child_process');
  const modulePath=require('node:path').resolve(__dirname,'..');
  const script=`
    const {Engine}=require(${JSON.stringify(modulePath)});
    (async()=>{
      const engine=new Engine({hostCapabilities:['pid']});
      engine.registerHost('pid',async()=>process.pid);
      const plugin=engine.prepare("export async function run(){return await sandbox.host.call('pid',null);}");
      const sandbox=await engine.create(plugin,{hostGrants:{pid:'process'}});
      if(await sandbox.call('run',null)!==process.pid) throw Error('PID mismatch');
      if(!(await sandbox.close()).closedCleanly || !await engine.shutdown()) throw Error('incomplete close');
      console.log('shutdown-ok');
    })().catch(error=>{console.error(error);process.exitCode=1;});`;
  const completed=spawnSync(process.execPath,['-e',script],{encoding:'utf8',timeout:10000});
  assert.equal(completed.error,undefined);
  assert.equal(completed.status,0,completed.stderr);
  assert.match(completed.stdout,/shutdown-ok/);
});
