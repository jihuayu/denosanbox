'use strict';
if (process.platform !== 'linux' || process.arch !== 'x64' || Number(process.versions.node.split('.')[0]) !== 24) {
  throw new Error('unsupported_platform: the experimental native gate currently covers only Node 24 on Linux x86_64');
}
const native = require('./denosanbox.node');
const prepared = new WeakMap();
class SandboxError extends Error {
  constructor(error) {
    super(error.message);
    this.name = 'SandboxError';
    this.kind = /^([a-z_]+):/.exec(error.message)?.[1] ?? 'native';
  }
}
function sync(fn) { try { return fn(); } catch (error) { throw new SandboxError(error); } }
async function asyncCall(fn) { try { return await fn(); } catch (error) { throw new SandboxError(error); } }
class PreparedPlugin {
  constructor(source, entrypoint) { prepared.set(this, {source, entrypoint}); Object.freeze(this); }
}
class Engine {
  #native;
  constructor() { this.#native = sync(() => new native.NativeEngine()); }
  prepare(source, entrypoint = 'main.js') {
    if (typeof source !== 'string' || typeof entrypoint !== 'string') throw new TypeError('source and entrypoint must be strings');
    sync(() => this.#native.prepare(source, entrypoint));
    return new PreparedPlugin(source, entrypoint);
  }
  async create(plugin) {
    const value = prepared.get(plugin);
    if (!value) throw new TypeError('expected a PreparedPlugin returned by Engine.prepare');
    return new Sandbox(await asyncCall(() => this.#native.create(value.source, value.entrypoint)));
  }
  shutdown() { return asyncCall(() => this.#native.shutdown()); }
  async [Symbol.asyncDispose]() { await this.shutdown(); }
}
class Sandbox {
  #native;
  constructor(handle) { this.#native = handle; }
  call(exportName, input, options = {}) {
    const json = JSON.stringify(input);
    if (json === undefined) return Promise.reject(new TypeError('input must be JSON serializable'));
    return asyncCall(async () => JSON.parse(await this.#native.call(exportName, json, options.timeoutMs)));
  }
  cancel() { this.#native.cancel(); }
  close() { return asyncCall(() => this.#native.close()); }
  async [Symbol.asyncDispose]() { await this.close(); }
}
module.exports = {Engine, PreparedPlugin, Sandbox, SandboxError};
