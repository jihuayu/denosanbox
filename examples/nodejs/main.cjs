'use strict';
// Multi-module graph, private VFS return values and multiple engines.
const {Engine} = require('../../bindings/nodejs');

const MODULES = {
  'main.ts': `
    import { greeting } from './lib/greeting.ts';
    let count = 0;
    export function run(input) { return {count: ++count, input, greeting: greeting(String(input))}; }`,
  'lib/greeting.ts': 'export function greeting(value: string): string { return `hello ${value}`; }',
};

(async () => {
  const engine = new Engine();
  const other = new Engine();
  try {
    const plugin = engine.prepareModules(MODULES, 'main.ts');
    console.log('modules', plugin.modulePaths, 'profile', plugin.profile);
    const a = await engine.create(plugin);
    const b = await other.create(other.prepare('let n=0; export function next(){return ++n;}'));
    console.log('host PID', process.pid, await a.call('run', 'A'), await a.call('run', 'B'));
    console.log('second engine', await b.call('next', null));
    console.log('close a', (await a.close()).closedCleanly);
    console.log('close b', (await b.close()).closedCleanly);
  } finally {
    await engine.shutdown();
    await other.shutdown();
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
