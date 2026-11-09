'use strict';
const {Engine} = require('../../bindings/nodejs');
(async () => {
  const engine=new Engine();
  try {
    const plugin=engine.prepare('let count=0; export function run(input) { return {count:++count,input}; }');
    const a=await engine.create(plugin);
    const b=await engine.create(plugin);
    console.log('host PID',process.pid,await a.call('run','A'),await b.call('run','B'));
    await a.close(); await b.close();
  } finally { await engine.shutdown(); }
})().catch(error=>{ console.error(error); process.exitCode=1; });
